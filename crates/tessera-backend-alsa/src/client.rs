//! The client side of the audio-ipc handshake (ADR 0007, `tessera-audio-ipc`)
//! for the PCM plugin: connect to `audio.sock`, ask for an endpoint, validate
//! and map what comes back.
//!
//! Decisions (ADR 0007 amendment): the endpoint name in `Hello` is the device
//! key, channels are numbered from 1, and the ring format is `i32` first
//! (lossless for 16/24/32-bit integer PCM, which `f32` cannot carry for
//! 32-bit data), with a single retry in the other format when the daemon
//! answers `Reject(BadRequest)`.

use std::io;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::socket::{
    AddressFamily, MsgFlags, SockFlag, SockType, UnixAddr, connect, getsockopt, recv, socket,
    sockopt,
};
use tessera_audio_ipc::proto::{Detach, DetachReason, Hello, Message, RejectCode};
use tessera_audio_ipc::{
    ClockOverlayReader, Direction, Endpoint, Limits, MessageReceiver, Ring, RingParams,
    SampleFormat, WakeWait, send_message,
};

use crate::error::Error;
use crate::settings::{Dir, RingFormatPref, Settings, Trust, UserSpec};

/// The wake socket and the audio socket of one attached session.
#[derive(Debug)]
pub struct Link {
    sock: UnixStream,
    receiver: MessageReceiver,
    wake: WakeWait,
    lost: bool,
    last_tick: Instant,
    /// When a drain last found ticks (the reference of the interval estimate).
    last_tick_drain: Option<Instant>,
    /// Decayed sums of the time between tick-bearing drains and of the ticks they
    /// found: their ratio is the mean tick interval, which stays right when the
    /// daemon's ticks arrive in bursts. `tick_samples` counts the drains in.
    tick_dt_ns: f64,
    tick_count: f64,
    tick_samples: u32,
}

/// Two drains that found ticks further apart than this say nothing about the
/// tick interval (the socket fills up and the daemon drops ticks on purpose,
/// so a long gap is a count of nothing).
const TICK_WINDOW: Duration = Duration::from_millis(100);
/// Estimates needed before the interval is trusted.
const TICK_MIN_SAMPLES: u32 = 12;

impl Link {
    /// The descriptor to poll: one byte per daemon engine tick.
    pub fn wake_fd(&self) -> RawFd {
        self.wake.as_fd().as_raw_fd()
    }

    /// How long ago the last wake tick was seen (by a drain). A daemon that
    /// keeps its clock overlay fresh but sends no ticks is broken; the plugin
    /// notices at `prepare`, which drains first.
    pub fn tick_age(&self) -> Duration {
        self.last_tick.elapsed()
    }

    /// The daemon's engine tick (the interval of its wake-ups) in frames of
    /// `rate` Hz; `0` until enough ticks were seen.
    ///
    /// `Attach` and the ring header do not carry the engine period (an
    /// audio-ipc ABI gap, see PROGRESS.md), so it is estimated: only from two
    /// drains that both found ticks and are less than 100 ms apart, as the
    /// time between them divided by the ticks found, smoothed. Idle periods,
    /// a socket that overflowed and stop/start cycles therefore cannot change
    /// it.
    pub fn tick_frames(&self, rate: u32) -> u64 {
        if self.tick_samples < TICK_MIN_SAMPLES || self.tick_count <= 0.0 {
            return 0;
        }
        let tick_ns = self.tick_dt_ns / self.tick_count;
        let frames = (tick_ns * f64::from(rate) / 1e9).ceil();
        if frames.is_finite() && frames >= 1.0 {
            frames as u64
        } else {
            0
        }
    }

    /// Waits up to `budget` for enough wake ticks to know the tick interval
    /// (open path, not real-time).
    pub fn measure_tick(&mut self, budget: Duration) {
        let end = Instant::now() + budget;
        while self.tick_samples < TICK_MIN_SAMPLES && !self.lost {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let ms = u16::try_from(left.as_millis().max(1)).unwrap_or(u16::MAX);
            let mut fds = [PollFd::new(self.wake.as_fd(), PollFlags::POLLIN)];
            if poll(&mut fds, PollTimeout::from(ms)).is_err() {
                break;
            }
            if !self.drain_wake() {
                break;
            }
        }
    }

    /// Liveness of the audio socket without allocating or decoding: a
    /// non-blocking peek of one byte. The end of the stream, an error, or any
    /// message after the attach (only `Detach` is defined) means the session is
    /// over. Returns false (and remembers it) if lost.
    pub fn peek_alive(&mut self) -> bool {
        if self.lost {
            return false;
        }
        let mut b = [0u8; 1];
        match recv(
            self.sock.as_raw_fd(),
            &mut b,
            MsgFlags::MSG_PEEK | MsgFlags::MSG_DONTWAIT,
        ) {
            Err(Errno::EAGAIN | Errno::EINTR) => true,
            Ok(_) | Err(_) => {
                self.lost = true;
                false
            }
        }
    }

    /// True once the daemon is known to be gone.
    pub fn is_lost(&self) -> bool {
        self.lost
    }

    /// Marks the session lost (a stale overlay, a failed ring).
    pub fn mark_lost(&mut self) {
        self.lost = true;
    }

    /// Consumes pending ticks. Returns false (and remembers it) once the
    /// daemon closed its end of the wake socket.
    pub fn drain_wake(&mut self) -> bool {
        if self.lost {
            return false;
        }
        match self.wake.drain() {
            Ok(n) => {
                if n > 0 {
                    let now = Instant::now();
                    self.last_tick = now;
                    if let Some(prev) = self.last_tick_drain {
                        let dt = now.saturating_duration_since(prev);
                        if dt <= TICK_WINDOW {
                            self.tick_dt_ns = self.tick_dt_ns * 0.98 + dt.as_nanos() as f64;
                            self.tick_count = self.tick_count * 0.98 + n as f64;
                            self.tick_samples = self.tick_samples.saturating_add(1);
                        }
                    }
                    self.last_tick_drain = Some(now);
                }
                true
            }
            Err(_) => {
                self.lost = true;
                false
            }
        }
    }

    /// Looks at the audio socket without blocking: a `Detach` or the end of
    /// the stream means the session is over. Returns false if lost.
    pub fn check_sock(&mut self) -> bool {
        if self.lost {
            return false;
        }
        loop {
            match self.receiver.recv(&self.sock) {
                Ok(None) => {
                    self.lost = true;
                    return false;
                }
                Ok(Some(r)) => {
                    if let Message::Detach(_) = r.message {
                        self.lost = true;
                        return false;
                    }
                }
                Err(tessera_audio_ipc::Error::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    return true;
                }
                Err(_) => {
                    self.lost = true;
                    return false;
                }
            }
        }
    }

    /// Tells the daemon we are going away (best effort, bounded; never
    /// raises SIGPIPE).
    pub fn detach(&self) {
        let _ = send_message(
            &self.sock,
            &Message::Detach(Detach {
                reason: DetachReason::ClientClosing,
            }),
            &[],
            Duration::from_millis(100),
        );
    }
}

/// An attached session.
#[derive(Debug)]
pub struct Attached {
    /// The mapped ring (create the role handle from it).
    pub ring: Ring,
    /// The clock overlay.
    pub overlay: Arc<ClockOverlayReader>,
    /// The sockets.
    pub link: Link,
    /// Daemon-assigned session id.
    pub session_id: u64,
    /// The ring parameters the daemon granted.
    pub params: RingParams,
    /// Effective latency at attach time, frames.
    pub latency: u64,
}

fn connect_bounded(path: &Path, deadline: Instant) -> Result<UnixStream, Error> {
    let fd = socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
        None,
    )
    .map_err(io::Error::from)?;
    let addr = UnixAddr::new(path).map_err(io::Error::from)?;
    loop {
        match connect(fd.as_raw_fd(), &addr) {
            Ok(()) => break,
            Err(Errno::EINTR) => {}
            // The listen backlog is full: wait for the daemon to accept.
            Err(Errno::EAGAIN) => {
                if Instant::now() >= deadline {
                    return Err(Error::Timeout("connect"));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => return Err(Error::Io(io::Error::from(e))),
        }
    }
    Ok(UnixStream::from(fd))
}

/// Refuses a daemon that is not one of the allowed users (`SO_PEERCRED`, the
/// credentials of the process that listens on the socket).
fn check_peer(sock: &UnixStream, trust: &Trust) -> Result<(), Error> {
    let Trust::Users(users) = trust else {
        return Ok(());
    };
    let cred = getsockopt(sock, sockopt::PeerCredentials).map_err(io::Error::from)?;
    let uid = cred.uid();
    let me = nix::unistd::geteuid().as_raw();
    let ok = users.iter().any(|u| match u {
        UserSpec::Uid(n) => *n == uid,
        UserSpec::SelfUid => me == uid,
        UserSpec::Name(n) => nix::unistd::User::from_name(n)
            .ok()
            .flatten()
            .is_some_and(|user| user.uid.as_raw() == uid),
    });
    if ok {
        Ok(())
    } else {
        Err(Error::Untrusted(format!(
            "it runs as uid {uid}; allowed: {users:?} (set `daemon_uid` in the PCM configuration)"
        )))
    }
}

fn wait_readable(sock: &UnixStream, deadline: Instant) -> Result<(), Error> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Error::Timeout("attach"));
    }
    let ms = u16::try_from(left.as_millis().min(1000)).unwrap_or(1000);
    let mut fds = [PollFd::new(sock.as_fd(), PollFlags::POLLIN)];
    match poll(&mut fds, PollTimeout::from(ms)) {
        Ok(_) | Err(Errno::EINTR) => Ok(()),
        Err(e) => Err(Error::Io(io::Error::from(e))),
    }
}

fn try_connect(
    s: &Settings,
    dir: Dir,
    client: &str,
    format: SampleFormat,
) -> Result<Attached, Error> {
    let timeout = Duration::from_millis(u64::from(s.connect_timeout_ms));
    let deadline = Instant::now() + timeout;
    let sock = connect_bounded(&s.socket, deadline)?;
    check_peer(&sock, &s.trust)?;
    let direction = match dir {
        Dir::Playback => Direction::Tx,
        Dir::Capture => Direction::Rx,
    };
    let hello = Hello {
        direction,
        format,
        first_channel: s.first_channel,
        channel_count: s.channels,
        sample_rate_hz: s.rate,
        max_period_frames: s.max_buffer,
        endpoint: s.device.clone(),
        client: client.to_string(),
    };
    send_message(&sock, &Message::Hello(hello), &[], timeout)?;
    let mut receiver = MessageReceiver::default();
    let received = loop {
        match receiver.recv(&sock) {
            Ok(Some(r)) => break r,
            Ok(None) => return Err(Error::Closed),
            Err(tessera_audio_ipc::Error::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                wait_readable(&sock, deadline)?;
            }
            Err(e) => return Err(e.into()),
        }
    };
    let (attach, fds) = match received.message {
        Message::Attach(a) => (a, received.fds),
        Message::Reject(r) => {
            return Err(Error::Rejected {
                code: r.code,
                message: r.message,
            });
        }
        Message::Detach(_) => return Err(Error::Closed),
        Message::Hello(_) => return Err(Error::Protocol("daemon sent Hello")),
    };
    let p = &attach.params;
    if p.direction != direction
        || p.channels != s.channels
        || p.first_channel != s.first_channel
        || p.format != format
    {
        return Err(Error::Protocol("attach does not match the request"));
    }
    if s.rate != 0 && p.sample_rate_hz != s.rate {
        return Err(Error::Protocol("daemon granted a different sample rate"));
    }
    let Endpoint {
        session_id,
        params,
        effective_latency_frames,
        ring,
        clock,
        wake,
    } = Endpoint::from_attach(&attach, fds, &Limits::default())?;
    Ok(Attached {
        ring,
        overlay: Arc::new(clock),
        link: Link {
            sock,
            receiver: MessageReceiver::default(),
            wake,
            lost: false,
            last_tick: Instant::now(),
            last_tick_drain: None,
            tick_dt_ns: 0.0,
            tick_count: 0.0,
            tick_samples: 0,
        },
        session_id,
        params,
        latency: effective_latency_frames,
    })
}

/// Connects to the daemon and attaches the endpoint described by `s`.
/// Blocking; every step is bounded by `s.connect_timeout_ms`.
///
/// # Errors
/// [`Error`], with the daemon's refusal reason when it refused.
pub fn connect_session(s: &Settings, dir: Dir, client: &str) -> Result<Attached, Error> {
    let (first, second) = match s.ring_format {
        RingFormatPref::I32 => (SampleFormat::I32, None),
        RingFormatPref::F32 => (SampleFormat::F32, None),
        RingFormatPref::I32ThenF32 => (SampleFormat::I32, Some(SampleFormat::F32)),
        RingFormatPref::F32ThenI32 => (SampleFormat::F32, Some(SampleFormat::I32)),
    };
    match (try_connect(s, dir, client, first), second) {
        (
            Err(Error::Rejected {
                code: RejectCode::BadRequest,
                ..
            }),
            Some(other),
        ) => try_connect(s, dir, client, other),
        (r, _) => r,
    }
}
