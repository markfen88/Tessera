//! The client side of the audio-ipc handshake (ADR 0007 and the
//! `tessera-audio-ipc` crate documentation) and the session objects handed to
//! the real-time side.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tessera_audio_ipc::proto::{Detach, DetachReason, Hello, Message, RejectCode};
use tessera_audio_ipc::{
    ClockOverlayReader, ClockSnapshot, Direction, Endpoint, Limits, MessageReceiver, Ring,
    RingConsumer, RingProducer, SampleFormat, WakeWait, send_message,
};

use crate::error::Error;

/// What to ask the daemon for.
#[derive(Debug, Clone)]
pub struct SessionRequest {
    /// Endpoint (logical device) name: the device key.
    pub endpoint: String,
    /// `Rx` = network to client, `Tx` = client to network.
    pub direction: Direction,
    /// 1-based number of the first channel.
    pub first_channel: u32,
    /// Number of channels.
    pub channels: u32,
    /// Sample rate (the node's rate).
    pub rate: u32,
    /// Largest graph period in frames (sizes the daemon's ring).
    pub max_period: u32,
    /// Client name for the daemon's status display.
    pub client: String,
}

/// The ring role the client plays.
#[derive(Debug)]
pub enum RingIo {
    /// RX endpoint: the client consumes.
    Rx(RingConsumer),
    /// TX endpoint: the client produces.
    Tx(RingProducer),
}

/// The part of a session the real-time callbacks own.
#[derive(Debug)]
pub struct RtSession {
    /// Keeps the mapping alive and gives access to the live latency.
    pub ring: Arc<Ring>,
    /// The role handle.
    pub io: RingIo,
    /// The clock overlay.
    pub overlay: Arc<ClockOverlayReader>,
    /// Sample format of the ring.
    pub format: SampleFormat,
    /// Channels in the ring.
    pub channels: usize,
    /// Sample rate of the ring.
    pub rate: u32,
    /// The last good clock snapshot: a failed read keeps using it.
    pub last_clock: Option<ClockSnapshot>,
}

/// The part of a session the main (control) thread owns.
#[derive(Debug)]
pub struct Link {
    /// The audio socket; EOF means the daemon is gone.
    pub sock: UnixStream,
    receiver: MessageReceiver,
    /// Overlay for the scheduler.
    pub overlay: Arc<ClockOverlayReader>,
    /// Ring (for the live effective latency).
    pub ring: Arc<Ring>,
    /// Daemon-assigned session id.
    pub session_id: u64,
    /// Sample rate granted.
    pub rate: u32,
}

/// A freshly attached session.
#[derive(Debug)]
pub struct Session {
    /// Control-thread half.
    pub link: Link,
    /// Wake socket (client end): one byte per daemon engine tick.
    pub wake: WakeWait,
    /// Real-time half.
    pub rt: Box<RtSession>,
}

/// State of the audio socket after polling it.
#[derive(Debug, PartialEq, Eq)]
pub enum LinkState {
    /// Still connected.
    Open,
    /// The daemon sent `Detach`.
    Detached(DetachReason),
    /// The daemon closed the socket.
    Closed,
    /// The socket failed or the daemon violated the protocol.
    Failed,
}

impl Link {
    /// Non-blocking check of the audio socket; drains pending messages.
    pub fn poll(&mut self) -> LinkState {
        loop {
            match self.receiver.recv(&self.sock) {
                Ok(None) => return LinkState::Closed,
                Ok(Some(r)) => {
                    if let Message::Detach(d) = r.message {
                        return LinkState::Detached(d.reason);
                    }
                    // Anything else after the attach is ignored.
                }
                Err(tessera_audio_ipc::Error::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    return LinkState::Open;
                }
                Err(_) => return LinkState::Failed,
            }
        }
    }

    /// Tells the daemon we are going away (best effort).
    pub fn send_detach(&self) {
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

fn try_connect(
    path: &Path,
    req: &SessionRequest,
    format: SampleFormat,
    timeout: Duration,
) -> Result<Session, Error> {
    let deadline = Instant::now() + timeout;
    let sock = UnixStream::connect(path)?;
    sock.set_read_timeout(Some(timeout))?;
    sock.set_write_timeout(Some(timeout))?;
    let hello = Hello {
        direction: req.direction,
        format,
        first_channel: req.first_channel,
        channel_count: req.channels,
        sample_rate_hz: req.rate,
        max_period_frames: req.max_period,
        endpoint: req.endpoint.clone(),
        client: req.client.clone(),
    };
    send_message(&sock, &Message::Hello(hello), &[], timeout)?;
    let mut receiver = MessageReceiver::default();
    // Poll a non-blocking socket against one overall deadline: a daemon that
    // trickles bytes cannot stretch the handshake beyond `timeout`.
    sock.set_nonblocking(true)?;
    let received = loop {
        match receiver.recv(&sock) {
            Ok(Some(r)) => break r,
            Ok(None) => return Err(Error::Closed),
            Err(tessera_audio_ipc::Error::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(Error::Timeout("attach"));
                }
                std::thread::sleep(Duration::from_millis(1));
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
    if attach.params.direction != req.direction
        || attach.params.channels != req.channels
        || attach.params.first_channel != req.first_channel
    {
        return Err(Error::Protocol("attach does not match the request"));
    }
    if req.rate != 0 && attach.params.sample_rate_hz != req.rate {
        return Err(Error::Protocol("daemon granted a different sample rate"));
    }
    let Endpoint {
        session_id,
        params,
        effective_latency_frames: _,
        ring,
        clock,
        wake,
    } = Endpoint::from_attach(&attach, fds, &Limits::default())?;
    let ring = Arc::new(ring);
    let io = match req.direction {
        Direction::Rx => RingIo::Rx(ring.consumer()?),
        Direction::Tx => RingIo::Tx(ring.producer()?),
    };
    let overlay = Arc::new(clock);
    sock.set_nonblocking(true)?;
    sock.set_read_timeout(None)?;
    Ok(Session {
        link: Link {
            sock,
            receiver: MessageReceiver::default(),
            overlay: Arc::clone(&overlay),
            ring: Arc::clone(&ring),
            session_id,
            rate: params.sample_rate_hz,
        },
        wake,
        rt: Box::new(RtSession {
            ring,
            io,
            last_clock: overlay.read().ok(),
            overlay,
            format: params.format,
            channels: params.channels as usize,
            rate: params.sample_rate_hz,
        }),
    })
}

/// Connects to `audio.sock`, performs the handshake and maps the objects.
/// Asks for `f32` first (the PipeWire graph's native format) and falls back to
/// `i32` when the daemon refuses. Blocking; every step is bounded by
/// `timeout`.
pub fn connect_session(
    path: &Path,
    req: &SessionRequest,
    timeout: Duration,
) -> Result<Session, Error> {
    match try_connect(path, req, SampleFormat::F32, timeout) {
        Err(Error::Rejected {
            code: RejectCode::BadRequest,
            ..
        }) => try_connect(path, req, SampleFormat::I32, timeout),
        other => other,
    }
}
