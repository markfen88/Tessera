//! `audio.sock`: the audio client attach protocol (ADR 0007).
//!
//! A client (PipeWire node, ALSA plugin, JACK client) connects, sends a
//! `Hello` naming a logical device and a direction, and receives an `Attach`
//! carrying three descriptors by `SCM_RIGHTS`: the ring memfd, the clock
//! overlay memfd and its end of a per-session wake socketpair
//! (`tessera-audio-ipc` documents the ABI). Or it receives a `Reject`.
//!
//! * **One connection, one session**, one ring (`rx` or `tx`) per `Hello`. A
//!   client that wants both directions opens two connections. A second
//!   `Hello` on a connection is a protocol error.
//! * **Endpoint** = the device key (or, failing that, the device name); empty
//!   means the first device. `first_channel` is a 1-based endpoint channel
//!   (0 is rejected; see the open question in `PROGRESS.md`),
//!   and `first_channel + channel_count - 1` must not exceed the device's
//!   channel count in that direction. `sample_rate_hz` 0 means the device
//!   rate; any other value must equal it.
//! * **Ring size** follows ADR 0006: next power of two of
//!   `max latency + reorder window + client period`, from the device's
//!   latency bound and the options in [`AudioConfig`].
//! * **Contents.** Until the media engine exists nothing is ever written to an
//!   RX ring, so it reads as silence ("unwritten"); a TX ring is never read.
//! * **Clock overlay**: one per logical device, owned by the PTP thread stub
//!   ([`crate::ptp`]).
//! * **Lifetime.** The session ends on `Detach`, EOF, a protocol error, the
//!   removal or restart of the device (`Detach(EndpointRemoved)`) or daemon
//!   shutdown (`Detach(DaemonShutdown)`). The daemon never depends on the
//!   client being alive: the wake signal never blocks and nothing waits for
//!   the client.
//! * **Who may attach.** Every member of the socket group may attach rx and
//!   tx endpoints of any device (ADR 0010 amendment): audio clients are
//!   trusted as far as the group is. What they cannot do is exhaust the
//!   daemon: a cap per uid and in total, a cap on `max_period_frames`, and a
//!   total ring-memory budget ([`AudioConfig`]).
//! * **Hostile peers.** Handshake bytes go through the resumable, total
//!   `MessageReceiver`; the first `Hello` must arrive within the handshake
//!   timeout; connection count is capped.

use std::collections::HashMap;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream as StdStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use tessera_audio_ipc::proto::{
    Attach, DEFAULT_MAX_FRAME_LEN, Detach, DetachReason, Hello, Message, Reject, RejectCode,
};
use tessera_audio_ipc::{
    Direction, Error as IpcError, Limits, MessageReceiver, Received as IpcReceived, Ring,
    RingParams, WakeSignal, send_message, wake_pair,
};
use tokio::io::Interest;
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::admission::Admission;
use crate::engine_thread::{DeviceInfo, EngineHandle};
use crate::media::worker::SessionEnd;
use crate::media::{MediaHandle, SessionSpec};
use crate::metrics::{Counters, inc};
use crate::peerlog::PeerLog;
use crate::ptp::ClockHandle;
use crate::sanitize;

/// Audio endpoint options.
#[derive(Debug, Clone)]
pub struct AudioConfig {
    /// Most simultaneous client connections.
    pub max_clients: usize,
    /// Most connections per uid.
    pub max_clients_per_uid: usize,
    /// Largest `max_period_frames` a client may ask for.
    pub max_period_frames: u32,
    /// Total bytes of ring memory all sessions together may reserve.
    pub ring_budget_bytes: u64,
    /// Time allowed for the `Hello` to arrive.
    pub handshake_timeout: Duration,
    /// Time allowed for a send to a client.
    pub send_timeout: Duration,
    /// Sanity limits for rings.
    pub limits: Limits,
    /// Reorder window added to the ring capacity, microseconds (ADR 0011: 10 ms).
    pub reorder_window_us: u32,
    /// Client period assumed when the `Hello` says 0, frames (ADR 0011: 1024).
    pub default_period_frames: u32,
    /// Added to the latency published to **rx** clients: the engine delivers
    /// received audio once per tick (plus wake-up jitter), so a client that
    /// reads at `now - latency` must read that much later to find it there.
    pub rx_margin_us: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            max_clients: 64,
            max_clients_per_uid: 8,
            max_period_frames: 16_384,
            ring_budget_bytes: 256 << 20,
            handshake_timeout: Duration::from_secs(5),
            send_timeout: Duration::from_secs(2),
            limits: Limits {
                max_ring_bytes: 64 << 20,
                ..Limits::default()
            },
            reorder_window_us: 10_000,
            default_period_frames: 1024,
            rx_margin_us: 0,
        }
    }
}

/// The audio options in effect, shared by the audio endpoint and the
/// hardware-card bridges. A reload of `[audio]` replaces them; each new
/// session (and each bridge open) takes a snapshot.
///
/// Never touched on a real-time path: written by the control runtime after
/// a reload, read by audio sessions (tokio tasks) when a client arrives and
/// by the bridge manager (a normal-priority thread) when it opens a card.
/// The media worker and the bridge threads never see it.
#[derive(Debug)]
pub struct SharedAudioConfig(RwLock<Arc<AudioConfig>>);

impl SharedAudioConfig {
    /// Shared options, starting with `cfg`.
    #[must_use]
    pub fn new(cfg: AudioConfig) -> Arc<Self> {
        Arc::new(Self(RwLock::new(Arc::new(cfg))))
    }

    /// The options in effect now.
    #[must_use]
    pub fn get(&self) -> Arc<AudioConfig> {
        Arc::clone(&self.0.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Replace the options (sessions already attached keep theirs).
    pub fn set(&self, cfg: AudioConfig) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(cfg);
    }
}

/// The total ring memory all sessions may hold.
#[derive(Debug)]
pub struct Budget {
    limit: AtomicU64,
    used: AtomicU64,
    counters: Arc<Counters>,
}

/// Bytes held from the [`Budget`]; returned when dropped.
#[derive(Debug)]
pub struct Reservation {
    budget: Arc<Budget>,
    bytes: u64,
}

impl Budget {
    /// A budget of `limit` bytes.
    #[must_use]
    pub fn new(limit: u64, counters: Arc<Counters>) -> Arc<Self> {
        Arc::new(Self {
            limit: AtomicU64::new(limit),
            used: AtomicU64::new(0),
            counters,
        })
    }

    /// Change the limit (a reload of `[audio] ring_budget_mib`). Rings that
    /// are already reserved stay; a lower limit refuses new ones until enough
    /// are returned.
    pub fn set_limit(&self, limit: u64) {
        self.limit.store(limit, Ordering::Relaxed);
    }

    /// The limit in effect, bytes.
    #[must_use]
    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Relaxed)
    }

    /// Reserve `bytes`, or `None` if that would exceed the limit.
    pub fn reserve(self: &Arc<Self>, bytes: u64) -> Option<Reservation> {
        let mut cur = self.used.load(Ordering::Relaxed);
        let limit = self.limit();
        loop {
            let next = cur.checked_add(bytes).filter(|n| *n <= limit)?;
            match self
                .used
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(seen) => cur = seen,
            }
        }
        self.counters
            .audio_ring_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        Some(Reservation {
            budget: Arc::clone(self),
            bytes,
        })
    }

    /// Bytes reserved now.
    #[must_use]
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
        self.budget
            .counters
            .audio_ring_bytes
            .fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct SessionEntry {
    device: String,
    removed: CancellationToken,
}

/// Shared state of the audio endpoint.
#[derive(Debug)]
pub struct AudioCtx {
    /// The state engine (device lookup).
    pub engine: EngineHandle,
    /// Clock overlays.
    pub clock: ClockHandle,
    /// Wake-up ticks.
    pub media: MediaHandle,
    /// Options (replaced by a reload; see [`AudioCtx::limits_changed`]).
    pub config: Arc<SharedAudioConfig>,
    /// Daemon shutdown.
    pub shutdown: CancellationToken,
    /// Connection caps.
    pub admission: Arc<Admission>,
    /// Counters.
    pub counters: Arc<Counters>,
    /// Ring memory budget.
    pub budget: Arc<Budget>,
    peerlog: PeerLog,
    sessions: Mutex<HashMap<u64, SessionEntry>>,
    next_id: AtomicU64,
}

impl AudioCtx {
    /// A new context.
    #[must_use]
    pub fn new(
        engine: EngineHandle,
        clock: ClockHandle,
        media: MediaHandle,
        config: Arc<SharedAudioConfig>,
        shutdown: CancellationToken,
        counters: Arc<Counters>,
    ) -> Self {
        let cfg = config.get();
        let admission = Admission::new(cfg.max_clients, 0, cfg.max_clients_per_uid);
        let budget = Budget::new(cfg.ring_budget_bytes, Arc::clone(&counters));
        Self {
            admission,
            budget,
            counters,
            peerlog: PeerLog::new(Duration::from_secs(5)),
            engine,
            clock,
            media,
            config,
            shutdown,
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// The options in effect now (one snapshot per session).
    #[must_use]
    pub fn cfg(&self) -> Arc<AudioConfig> {
        self.config.get()
    }

    /// Apply the caps and the budget of the options now in
    /// [`config`](Self::config) (after a reload replaced them).
    pub fn limits_changed(&self) {
        let cfg = self.cfg();
        self.admission
            .set_caps(cfg.max_clients, 0, cfg.max_clients_per_uid);
        self.budget.set_limit(cfg.ring_budget_bytes);
    }

    /// Detach every session of `device` (it was removed or must restart).
    pub fn detach_device(&self, device: &str) {
        let sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        for s in sessions.values().filter(|s| s.device == device) {
            s.removed.cancel();
        }
    }

    /// Number of attached sessions.
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// Accept clients until shutdown.
pub async fn serve(listener: Arc<UnixListener>, ctx: Arc<AudioCtx>, tracker: TaskTracker) {
    loop {
        tokio::select! {
            () = ctx.shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let ctx = Arc::clone(&ctx);
                    tracker.spawn(async move { session(stream, ctx).await });
                }
                Err(e) => {
                    tracing::warn!("audio: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }
}

/// Receive one message with its descriptors, waiting asynchronously.
async fn recv_msg(
    stream: &UnixStream,
    std: &StdStream,
    rx: &mut MessageReceiver,
) -> Result<Option<IpcReceived>, IpcError> {
    loop {
        stream.readable().await?;
        let r = stream.try_io(Interest::READABLE, || match rx.recv(std) {
            Err(IpcError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => Err(e),
            other => Ok(other),
        });
        match r {
            Ok(done) => return done,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(IpcError::Io(e)),
        }
    }
}

/// Send a control message without fds from a blocking thread.
async fn send_plain(std: &StdStream, msg: Message, timeout: Duration) {
    let Ok(sock) = std.try_clone() else {
        return;
    };
    let _ = tokio::task::spawn_blocking(move || send_message(&sock, &msg, &[], timeout)).await;
}

fn reject(code: RejectCode, message: impl Into<String>) -> Reject {
    let mut message = message.into();
    message.truncate(200);
    Reject { code, message }
}

struct Attached {
    id: u64,
    // Keep the mapping alive until the session ends.
    _ring: Ring,
    /// The ring end the daemon holds; handed to the media engine.
    role: Option<SessionEnd>,
}

/// Validate a `Hello` against a device and compute the ring parameters.
///
/// Pure: unit-tested without sockets.
///
/// # Errors
/// A `Reject` describing what is wrong.
pub fn plan_ring(
    hello: &Hello,
    dev: &DeviceInfo,
    cfg: &AudioConfig,
) -> Result<(RingParams, u64), Reject> {
    hello
        .validate(&cfg.limits)
        .map_err(|e| reject(RejectCode::BadRequest, e.to_string()))?;
    if hello.first_channel == 0 {
        return Err(reject(
            RejectCode::BadRequest,
            "channels are numbered from 1; first_channel 0 is not valid",
        ));
    }
    if hello.max_period_frames > cfg.max_period_frames {
        return Err(reject(
            RejectCode::BadRequest,
            format!(
                "max_period_frames {} is above the limit of {}",
                hello.max_period_frames, cfg.max_period_frames
            ),
        ));
    }
    let available = match hello.direction {
        Direction::Rx => dev.rx_channels,
        Direction::Tx => dev.tx_channels,
    };
    let first = u64::from(hello.first_channel);
    let last = first + u64::from(hello.channel_count) - 1;
    if last > u64::from(available) {
        return Err(reject(
            RejectCode::BadRequest,
            format!(
                "channels {first}..={last} requested, device `{}` has {available} in that direction",
                dev.key
            ),
        ));
    }
    if hello.sample_rate_hz != 0 && hello.sample_rate_hz != dev.sample_rate {
        return Err(reject(
            RejectCode::BadRequest,
            format!(
                "device `{}` runs at {} Hz, {} Hz requested",
                dev.key, dev.sample_rate, hello.sample_rate_hz
            ),
        ));
    }
    let rate = u64::from(dev.sample_rate);
    let frames = |us: u32| u64::from(us) * rate / 1_000_000;
    let period = if hello.max_period_frames == 0 {
        cfg.default_period_frames
    } else {
        hello.max_period_frames
    };
    let capacity = RingParams::capacity_for(
        frames(dev.latency_max_us),
        frames(cfg.reorder_window_us),
        u64::from(period),
    );
    // An rx client reads at `now - latency`; the engine writes received
    // audio once per tick, so the published latency includes the margin.
    let latency = match hello.direction {
        Direction::Rx => frames(dev.latency_us) + frames(cfg.rx_margin_us),
        Direction::Tx => frames(dev.latency_us),
    };
    Ok((hello.ring_params(dev.sample_rate, capacity), latency))
}

/// Blocking part of the attach: create the objects and send the `Attach`,
/// with the session's own snapshot of the options (`cfg`).
fn attach_blocking(
    ctx: &AudioCtx,
    cfg: &AudioConfig,
    std: &StdStream,
    hello: &Hello,
    dev: &DeviceInfo,
    params: &RingParams,
    latency_frames: u64,
) -> Result<(Attached, WakeSignal), Reject> {
    let internal = |what: &str, e: &dyn std::fmt::Display| {
        tracing::warn!("audio: {what}: {e}");
        reject(RejectCode::Internal, what)
    };
    let ring = Ring::create(params, &cfg.limits).map_err(|e| {
        reject(
            RejectCode::BadRequest,
            format!("cannot create the ring: {e}"),
        )
    })?;
    ring.set_effective_latency(latency_frames)
        .map_err(|e| internal("cannot set the latency", &e))?;
    let role = match hello.direction {
        Direction::Rx => ring.producer().map(SessionEnd::Producer),
        Direction::Tx => ring.consumer().map(SessionEnd::Consumer),
    }
    .map_err(|e| internal("cannot take the ring role", &e))?;
    let overlay = ctx
        .clock
        .overlay_fd(&dev.key)
        .map_err(|e| internal("no clock overlay", &e))?;
    let (wake, wake_client) =
        wake_pair().map_err(|e| internal("cannot create the wake pair", &e))?;
    let id = ctx.next_id.fetch_add(1, Ordering::Relaxed);
    let attach = Attach {
        session_id: id,
        params: *params,
        effective_latency_frames: latency_frames,
    };
    send_message(
        std,
        &Message::Attach(attach),
        &[ring.fd(), overlay.as_fd(), wake_client.as_fd()],
        cfg.send_timeout,
    )
    .map_err(|e| internal("cannot send the attach", &e))?;
    Ok((
        Attached {
            id,
            _ring: ring,
            role: Some(role),
        },
        wake,
    ))
}

async fn reject_and_close(ctx: &AudioCtx, cfg: &AudioConfig, std: &StdStream, r: Reject) {
    inc(&ctx.counters.audio_rejects);
    send_plain(std, Message::Reject(r), cfg.send_timeout).await;
}

async fn session(stream: UnixStream, ctx: Arc<AudioCtx>) {
    let uid = stream.peer_cred().map(|c| c.uid()).ok();
    let uid_key = uid.unwrap_or(u32::MAX);
    let _permit = match ctx.admission.admit(uid_key, false) {
        Ok(p) => p,
        Err(r) => {
            inc(&ctx.counters.audio_connections_refused);
            ctx.peerlog.note(
                uid_key,
                "audio connection refused",
                match r {
                    crate::admission::Refusal::PerUid => "per-uid connection cap",
                    crate::admission::Refusal::Full => "audio client table full",
                },
            );
            return;
        }
    };
    let Ok(owned) = stream.as_fd().try_clone_to_owned() else {
        return;
    };
    // Same open file description as `stream` (non-blocking); the audio-ipc
    // functions want a std stream, tokio provides readiness.
    let std = StdStream::from(owned);
    let mut rx = MessageReceiver::new(DEFAULT_MAX_FRAME_LEN);
    // The options as they are when this client arrives (a reload applies
    // to the clients that come after it).
    let cfg = ctx.cfg();
    let timeout = cfg.send_timeout;

    let first = tokio::time::timeout(cfg.handshake_timeout, recv_msg(&stream, &std, &mut rx)).await;
    let hello = match first {
        Ok(Ok(Some(r))) => match r.message {
            Message::Hello(h) => h,
            _ => {
                reject_and_close(
                    &ctx,
                    &cfg,
                    &std,
                    reject(RejectCode::BadRequest, "expected hello"),
                )
                .await;
                return;
            }
        },
        Ok(Ok(None)) | Err(_) => return,
        Ok(Err(e)) => {
            ctx.peerlog
                .note(uid_key, "bad audio handshake", "protocol error");
            tracing::trace!("audio: bad handshake: {e}");
            send_plain(
                &std,
                Message::Detach(Detach {
                    reason: DetachReason::ProtocolError,
                }),
                timeout,
            )
            .await;
            return;
        }
    };

    let key = (!hello.endpoint.is_empty()).then(|| hello.endpoint.clone());
    let dev = match ctx.engine.device_info(key).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            let msg = reject(
                RejectCode::UnknownEndpoint,
                format!("no device `{}`", sanitize::log_safe(&hello.endpoint, 64)),
            );
            reject_and_close(&ctx, &cfg, &std, msg).await;
            return;
        }
        Err(e) => {
            reject_and_close(
                &ctx,
                &cfg,
                &std,
                reject(RejectCode::Internal, e.to_string()),
            )
            .await;
            return;
        }
    };
    let (params, latency_frames) = match plan_ring(&hello, &dev, &cfg) {
        Ok(p) => p,
        Err(r) => {
            reject_and_close(&ctx, &cfg, &std, r).await;
            return;
        }
    };
    let ring_bytes = match params.validate(&cfg.limits) {
        Ok(g) => g.total_len,
        Err(e) => {
            let msg = reject(RejectCode::BadRequest, format!("ring too large: {e}"));
            reject_and_close(&ctx, &cfg, &std, msg).await;
            return;
        }
    };
    let Some(reservation) = ctx.budget.reserve(ring_bytes) else {
        ctx.peerlog.note(
            uid_key,
            "audio request refused",
            "ring memory budget exhausted",
        );
        let msg = reject(
            RejectCode::Busy,
            "the daemon's ring memory budget is used up",
        );
        reject_and_close(&ctx, &cfg, &std, msg).await;
        return;
    };

    let (mut attached, wake) = {
        let ctx2 = Arc::clone(&ctx);
        let cfg2 = Arc::clone(&cfg);
        let sock = match std.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        };
        let (hello, dev) = (hello.clone(), dev.clone());
        let r = tokio::task::spawn_blocking(move || {
            attach_blocking(&ctx2, &cfg2, &sock, &hello, &dev, &params, latency_frames)
        })
        .await;
        match r {
            Ok(Ok(v)) => v,
            Ok(Err(rej)) => {
                reject_and_close(&ctx, &cfg, &std, rej).await;
                return;
            }
            Err(_) => return,
        }
    };
    let id = attached.id;
    let removed = CancellationToken::new();
    ctx.sessions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            id,
            SessionEntry {
                device: dev.key.clone(),
                removed: removed.clone(),
            },
        );
    let Some(end) = attached.role.take() else {
        return;
    };
    let spec = SessionSpec {
        id,
        device: dev.key.clone(),
        first_channel: hello.first_channel,
        channel_count: hello.channel_count,
        format: params.format,
        end,
        wake: Some(wake),
    };
    if !ctx.media.add_session(spec) {
        ctx.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        send_plain(
            &std,
            Message::Detach(Detach {
                reason: DetachReason::EndpointRemoved,
            }),
            timeout,
        )
        .await;
        return;
    }
    inc(&ctx.counters.audio_sessions);
    inc(&ctx.counters.audio_sessions_total);
    let _gauge = crate::metrics::GaugeGuard(&ctx.counters.audio_sessions);
    tracing::debug!(
        ?uid,
        session = id,
        device = %dev.key,
        direction = ?hello.direction,
        channels = hello.channel_count,
        client = %sanitize::log_safe(&hello.client, 64),
        "audio: client attached"
    );

    let end = tokio::select! {
        () = ctx.shutdown.cancelled() => Some(DetachReason::DaemonShutdown),
        () = removed.cancelled() => Some(DetachReason::EndpointRemoved),
        m = recv_msg(&stream, &std, &mut rx) => match m {
            Ok(Some(IpcReceived { message: Message::Detach(_), .. })) | Ok(None) => None,
            Ok(Some(_)) => Some(DetachReason::ProtocolError),
            Err(e) => {
                tracing::trace!(session = id, "audio: {e}");
                Some(DetachReason::ProtocolError)
            }
        },
    };
    if let Some(reason) = end {
        send_plain(&std, Message::Detach(Detach { reason }), timeout).await;
    }
    ctx.media.remove_session(id);
    ctx.sessions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&id);
    tracing::debug!(session = id, "audio: client detached");
    drop(attached);
    drop(reservation);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_audio_ipc::SampleFormat;

    fn dev() -> DeviceInfo {
        DeviceInfo {
            key: "main".into(),
            rx_channels: 8,
            tx_channels: 4,
            sample_rate: 48_000,
            latency_us: 4_000,
            latency_max_us: 40_000,
        }
    }

    fn hello() -> Hello {
        Hello {
            direction: Direction::Rx,
            format: SampleFormat::I32,
            first_channel: 1,
            channel_count: 2,
            sample_rate_hz: 0,
            max_period_frames: 0,
            endpoint: String::new(),
            client: "t".into(),
        }
    }

    #[test]
    fn capacity_follows_adr_0011_defaults() {
        let (p, lat) = plan_ring(&hello(), &dev(), &AudioConfig::default()).unwrap();
        // 40 ms + 10 ms + 1024 frames at 48 kHz = 1920 + 480 + 1024 -> 4096.
        assert_eq!(p.capacity_frames, 4096);
        assert_eq!(p.sample_rate_hz, 48_000);
        assert_eq!(lat, 192);
    }

    #[test]
    fn channel_range_is_checked_per_direction() {
        let mut h = hello();
        h.direction = Direction::Tx;
        h.first_channel = 3;
        h.channel_count = 2;
        assert!(plan_ring(&h, &dev(), &AudioConfig::default()).is_ok());
        h.channel_count = 3;
        let r = plan_ring(&h, &dev(), &AudioConfig::default()).unwrap_err();
        assert_eq!(r.code, RejectCode::BadRequest);
        h.direction = Direction::Rx;
        assert!(plan_ring(&h, &dev(), &AudioConfig::default()).is_ok());
    }

    #[test]
    fn first_channel_zero_is_rejected() {
        let mut h = hello();
        h.first_channel = 0;
        let r = plan_ring(&h, &dev(), &AudioConfig::default()).unwrap_err();
        assert_eq!(r.code, RejectCode::BadRequest);
        h.first_channel = 1;
        h.channel_count = 8;
        assert!(plan_ring(&h, &dev(), &AudioConfig::default()).is_ok());
        h.channel_count = 9;
        assert!(plan_ring(&h, &dev(), &AudioConfig::default()).is_err());
    }

    #[test]
    fn the_period_is_capped() {
        let mut h = hello();
        h.max_period_frames = AudioConfig::default().max_period_frames;
        assert!(plan_ring(&h, &dev(), &AudioConfig::default()).is_ok());
        h.max_period_frames += 1;
        assert_eq!(
            plan_ring(&h, &dev(), &AudioConfig::default())
                .unwrap_err()
                .code,
            RejectCode::BadRequest
        );
    }

    #[test]
    fn the_budget_never_overcommits_and_gives_back() {
        let c = Arc::new(Counters::new());
        let b = Budget::new(100, Arc::clone(&c));
        let a = b.reserve(60).unwrap();
        assert!(b.reserve(41).is_none());
        let b2 = b.reserve(40).unwrap();
        assert_eq!(b.used(), 100);
        assert_eq!(c.audio_ring_bytes.load(Ordering::Relaxed), 100);
        drop(a);
        assert_eq!(b.used(), 40);
        assert!(b.reserve(60).is_some());
        drop(b2);
        assert!(b.reserve(u64::MAX).is_none(), "no overflow");
    }

    #[test]
    fn rate_must_match_the_device() {
        let mut h = hello();
        h.sample_rate_hz = 44_100;
        assert!(plan_ring(&h, &dev(), &AudioConfig::default()).is_err());
        h.sample_rate_hz = 48_000;
        assert!(plan_ring(&h, &dev(), &AudioConfig::default()).is_ok());
    }

    #[test]
    fn hostile_numbers_do_not_overflow() {
        let mut h = hello();
        h.first_channel = u32::MAX;
        h.channel_count = u32::MAX;
        assert!(plan_ring(&h, &dev(), &AudioConfig::default()).is_err());
        let mut h = hello();
        h.max_period_frames = u32::MAX;
        assert!(plan_ring(&h, &dev(), &AudioConfig::default()).is_err());
        let cfg = AudioConfig {
            max_period_frames: u32::MAX,
            ..AudioConfig::default()
        };
        // Capacity becomes huge; the ring limit decides, not an overflow.
        let (p, _) = plan_ring(&h, &dev(), &cfg).unwrap();
        assert!(p.capacity_frames.is_power_of_two());
    }
}
