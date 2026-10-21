//! The control socket: `control.sock` (ADR 0008, ADR 0010).
//!
//! Wire format: length-delimited frames ([`crate::frame`], `u32` big-endian)
//! carrying the JSON envelopes of `tessera-control-types`
//! (`Request` in, `Response` out).
//!
//! # Connection model
//!
//! One task per connection, **strictly sequential**: read a frame, handle it,
//! write the answer. There is no outbound queue, so a client that does not read
//! its answers can only stall its own task, and the write is bounded by a
//! timeout after which the connection is dropped. Other clients are never
//! affected: nothing is shared except the engine's bounded request queue,
//! the admission table and the per-uid rate limiter (short critical sections).
//!
//! * **Identification.** `SO_PEERCRED` and `SO_PEERGROUPS` at accept
//!   ([`crate::peer`]). Reading is open to every peer that can open the
//!   socket (0660, group `tessera`); mutating commands need uid 0 or the admin
//!   group, else `PermissionDenied`. Access is re-evaluated per request, so a
//!   reload that changes `admin_group` applies to open connections.
//! * **Admission** ([`crate::admission`]). A cap per uid, and slots reserved
//!   for administrators, so unprivileged group members cannot lock the
//!   administrators out. A request/response connection that sends nothing
//!   for the idle timeout is closed (subscribers are exempt).
//! * **Rate limiting.** Every request frame costs one token from the peer's
//!   per-uid bucket ([`crate::ratelimit`]). An empty bucket gets one
//!   `RateLimited` answer (id 0: the frame is not even parsed) and the daemon
//!   then *stops reading from that connection* until a token is available, so
//!   a flooding sender is slowed by its own socket buffer instead of being
//!   answered at wire speed.
//! * **Garbage.** A frame that is not a valid request gets an `Invalid`
//!   response and the connection continues (frame boundaries are intact);
//!   [`MAX_CONSECUTIVE_INVALID`] in a row close it. An oversized frame header
//!   gets one `Invalid` response and closes the connection (the stream cannot
//!   be re-synchronised). A frame must be *completed* within the frame
//!   timeout counted from its first byte (a deadline, not an inactivity
//!   timer, so trickling bytes does not help).
//! * **Subscriptions.** `SubscribeEvents` switches the connection to an event
//!   stream: an `Ok(Done)` acknowledgement, then one `Ok(Event(e))` response
//!   per event, all carrying the request id. The subscriber owns a bounded
//!   queue on the engine's event bus; a slow reader loses the oldest events
//!   (bus policy) and is told with `Ok(Lagged { dropped })`, after which the
//!   client should re-read `Status`. Requests sent on a streaming connection
//!   get an `Invalid` response.
//! * **Confirmation** (ADR 0014). `Command` goes to `StateEngine::apply`: a
//!   change that restarts a device or the daemon is refused with
//!   `ConfirmationRequired` carrying its impact, and nothing changes.
//!   `ConfirmedCommand { expect: None }` applies whatever the impact is
//!   (`apply_confirmed`); with `expect: Some(token)` it applies only if the
//!   impact computed now has that token, and otherwise asks again
//!   (`apply_expecting`). Replies: `Done` to a plain command (the shape every
//!   older client expects), `Impact` to a dry run (`SetDeviceChannels {
//!   dry_run: true }`) and to a confirmed command (what it did, including the
//!   `restart_keys` of a reload).
//! * **Who may do what** (ADR 0010). Queries, `SubscribeEvents` and dry runs
//!   are reading: open to every peer that can connect (a dry run changes
//!   nothing and shows no more than `Status` does). Commands, confirmed or
//!   not, need uid 0 or the admin group.
//! * **Size.** Requests and responses are capped by `[control]
//!   max_frame_bytes` (read when the connection opens). A reply that would
//!   exceed it is replaced by `ResponseTooLarge { size, limit, key }`, and
//!   the connection stays usable.
//! * **Logging.** Peer behaviour (refusals, garbage, rate limiting) is logged
//!   at `debug`, one summary line per uid per interval, and never contains
//!   peer-chosen payloads ([`crate::peerlog`], [`crate::sanitize`]). Accepted
//!   commands are logged at `info` by kind only.

use std::os::fd::AsFd;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tessera_control_types::{
    Command, CommandError, Event, Origin, Query, Reply, Request, RequestBody, Response,
    WIRE_VERSION,
};
use tessera_state::{Effect, EventBus, Received};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::time::timeout_at;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::admission::{Admission, Refusal};
use crate::engine_thread::{Confirm, EngineHandle, Settings};
use crate::frame::{DEFAULT_MAX_FRAME_LEN, FrameDecoder, FrameError, encode_frame};
use crate::metrics::{Counters, GaugeGuard, inc};
use crate::peer::{Access, PeerCred, classify};
use crate::peerlog::PeerLog;
use crate::ratelimit::PeerLimiter;
use crate::sanitize;

/// Consecutive undecodable frames after which a connection is closed.
pub const MAX_CONSECUTIVE_INVALID: u32 = 32;

/// Receives the follow-up work reported by the state engine (device restarts
/// and so on). Implemented by the daemon.
pub trait EffectSink: Send + Sync + std::fmt::Debug {
    /// Handle the effects of an applied command or reload.
    fn on_effects(&self, effects: &[Effect]);
}

/// The configuration key that caps control frames.
pub const MAX_FRAME_KEY: &str = "control.max_frame_bytes";

/// Control-plane limits (`[control]`, apply class `reload`: a connection
/// takes the values in effect when it opens).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlConfig {
    /// Largest accepted/produced frame payload.
    pub max_frame: usize,
    /// Events queued per subscriber.
    pub event_queue: usize,
    /// How long a peer may take to complete a frame, from its first byte.
    pub frame_timeout: Duration,
    /// How long a request/response connection may stay silent between frames.
    pub idle_timeout: Duration,
    /// How long a write to a request/response peer may take.
    pub write_timeout: Duration,
    /// How long a write to an event subscriber may take.
    pub stream_write_timeout: Duration,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            max_frame: DEFAULT_MAX_FRAME_LEN,
            event_queue: 256,
            frame_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(5),
            stream_write_timeout: Duration::from_secs(30),
        }
    }
}

/// Everything the connection tasks share.
#[derive(Debug)]
pub struct ControlCtx {
    /// The state engine.
    pub engine: EngineHandle,
    /// The event bus (subscribers read it directly).
    pub bus: EventBus,
    /// Per-uid limiter.
    pub limiter: Mutex<PeerLimiter>,
    /// Admin gid, limits and pending restarts, kept current across reloads.
    pub settings: watch::Receiver<Settings>,
    /// Follow-up work sink.
    pub effects: Arc<dyn EffectSink>,
    /// Shutdown request.
    pub shutdown: CancellationToken,
    /// Time origin for the limiter.
    pub started: Instant,
    /// Connection caps.
    pub admission: Arc<Admission>,
    /// Counters.
    pub counters: Arc<Counters>,
    /// Rate-limited peer-behaviour log.
    pub peerlog: PeerLog,
}

impl ControlCtx {
    fn take_token(&self, uid: u32) -> Result<(), Duration> {
        let now = self.started.elapsed();
        self.limiter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .try_take(uid, now)
    }

    fn admin_gid(&self) -> Option<u32> {
        self.settings.borrow().admin_gid
    }

    /// The limits in effect now (a new connection keeps them).
    #[must_use]
    pub fn limits(&self) -> ControlConfig {
        self.settings.borrow().tunables.control.clone()
    }
}

/// Accept connections until shutdown.
pub async fn serve(listener: Arc<UnixListener>, ctx: Arc<ControlCtx>, tracker: TaskTracker) {
    loop {
        tokio::select! {
            () = ctx.shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let ctx = Arc::clone(&ctx);
                    tracker.spawn(async move { handle_connection(stream, ctx).await });
                }
                Err(e) => {
                    tracing::warn!("control: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }
}

fn error_response(id: u64, e: CommandError) -> Response {
    Response {
        v: WIRE_VERSION,
        id,
        result: Err(e),
    }
}

fn invalid(field: &str, reason: impl AsRef<str>) -> CommandError {
    CommandError::Invalid {
        field: field.to_owned(),
        reason: sanitize::log_safe(reason.as_ref(), 200),
    }
}

/// Encode one response; a reply over `max` becomes the typed
/// `ResponseTooLarge` error for the same request id (a few hundred bytes,
/// always under the schema's 64 KiB floor of `max_frame_bytes`), so the
/// client learns the size and the key to raise, and the connection goes on.
fn encode_response(resp: &Response, max: usize) -> Result<Vec<u8>, String> {
    let json = serde_json::to_vec(resp).map_err(|e| e.to_string())?;
    match encode_frame(&json, max) {
        Ok(b) => Ok(b),
        Err(FrameError::TooLarge { len, .. }) => {
            let e = error_response(
                resp.id,
                CommandError::ResponseTooLarge {
                    size: len,
                    limit: max as u64,
                    key: MAX_FRAME_KEY.to_owned(),
                },
            );
            let j = serde_json::to_vec(&e).map_err(|e| e.to_string())?;
            // No cap for this one frame: it is a few hundred bytes (the key
            // is a constant, the sizes are numbers), far below the 64 KiB
            // floor of `max_frame_bytes`.
            encode_frame(&j, usize::MAX).map_err(|e| e.to_string())
        }
    }
}

/// Serialise and send one response. Returns `false` if the connection must
/// be closed (write failed or timed out).
async fn send(wr: &mut OwnedWriteHalf, resp: &Response, max: usize, timeout: Duration) -> bool {
    let bytes = match encode_response(resp, max) {
        Ok(b) => b,
        Err(reason) => {
            let fallback = error_response(
                resp.id,
                CommandError::Internal {
                    message: format!("response not sent: {reason}"),
                },
            );
            match serde_json::to_vec(&fallback)
                .ok()
                .and_then(|j| encode_frame(&j, usize::MAX).ok())
            {
                Some(b) => b,
                None => return false,
            }
        }
    };
    matches!(
        tokio::time::timeout(timeout, wr.write_all(&bytes)).await,
        Ok(Ok(()))
    )
}

enum Read {
    Frame(Vec<u8>),
    Closed,
    TooLarge(FrameError),
}

/// Read until one whole frame is available.
///
/// `frame_timeout` is a **deadline** for completing a frame, started when its
/// first byte is buffered: trickling one byte at a time does not extend it.
/// `idle` (if any) bounds the wait for the first byte.
async fn read_frame(
    rd: &mut OwnedReadHalf,
    dec: &mut FrameDecoder,
    buf: &mut [u8],
    frame_timeout: Duration,
    idle: Option<Duration>,
    shutdown: &CancellationToken,
) -> Read {
    let mut frame_deadline: Option<tokio::time::Instant> = None;
    loop {
        match dec.next_frame() {
            Ok(Some(f)) => return Read::Frame(f),
            Ok(None) => {}
            Err(e) => return Read::TooLarge(e),
        }
        if frame_deadline.is_none() && dec.is_partial() {
            frame_deadline = Some(tokio::time::Instant::now() + frame_timeout);
        }
        let read = async {
            let limit = match (frame_deadline, idle) {
                (Some(d), _) => Some(d),
                (None, Some(i)) => Some(tokio::time::Instant::now() + i),
                (None, None) => None,
            };
            match limit {
                Some(d) => match timeout_at(d, rd.read(buf)).await {
                    Ok(r) => r,
                    Err(_) => Err(std::io::ErrorKind::TimedOut.into()),
                },
                None => rd.read(buf).await,
            }
        };
        let n = tokio::select! {
            () = shutdown.cancelled() => return Read::Closed,
            r = read => match r {
                Ok(0) | Err(_) => return Read::Closed,
                Ok(n) => n,
            },
        };
        if let Some(chunk) = buf.get(..n) {
            dec.push(chunk);
        }
    }
}

async fn handle_connection(stream: UnixStream, ctx: Arc<ControlCtx>) {
    let cred = match stream.peer_cred() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("control: cannot read peer credentials: {e}");
            return;
        }
    };
    let groups = crate::platform::peer_groups(stream.as_fd()).unwrap_or_else(|e| {
        tracing::debug!("control: no SO_PEERGROUPS ({e}); only the effective gid counts");
        Vec::new()
    });
    let cred = PeerCred {
        uid: cred.uid(),
        gid: cred.gid(),
        pid: cred.pid(),
        groups,
    };
    let uid = cred.uid;
    let access_now = |ctx: &ControlCtx| classify(&cred, ctx.admin_gid());

    let _permit = match ctx.admission.admit(uid, access_now(&ctx).can_mutate()) {
        Ok(p) => p,
        Err(r) => {
            inc(&ctx.counters.control_connections_refused);
            ctx.peerlog.note(
                uid,
                "connection refused",
                match r {
                    Refusal::PerUid => "per-uid connection cap",
                    Refusal::Full => "no free slot for this peer class",
                },
            );
            return;
        }
    };
    inc(&ctx.counters.control_connections);
    inc(&ctx.counters.control_connections_total);
    let _gauge = GaugeGuard(&ctx.counters.control_connections);
    tracing::debug!(uid, gid = cred.gid, pid = ?cred.pid, access = ?access_now(&ctx), "control: connection");

    let (mut rd, mut wr) = stream.into_split();
    // The limits in effect when the connection opened (a reload applies to
    // the connections that come after it).
    let cfg = ctx.limits();
    let mut dec = FrameDecoder::new(cfg.max_frame);
    let mut buf = vec![0u8; 16 * 1024];
    let mut invalid_run = 0u32;
    let (max, wto) = (cfg.max_frame, cfg.write_timeout);

    loop {
        let frame = match read_frame(
            &mut rd,
            &mut dec,
            &mut buf,
            cfg.frame_timeout,
            Some(cfg.idle_timeout),
            &ctx.shutdown,
        )
        .await
        {
            Read::Frame(f) => f,
            Read::Closed => return,
            Read::TooLarge(e) => {
                inc(&ctx.counters.requests_invalid);
                ctx.peerlog.note(uid, "oversized frame", "closing");
                let _ = send(
                    &mut wr,
                    &error_response(0, invalid("frame", e.to_string())),
                    max,
                    wto,
                )
                .await;
                return;
            }
        };
        inc(&ctx.counters.requests_total);
        if let Err(wait) = ctx.take_token(uid) {
            inc(&ctx.counters.requests_rate_limited);
            ctx.peerlog.note(uid, "rate limited", "request frames");
            // The frame is not parsed (its id is unknown and parsing costs
            // CPU): one answer, then stop reading until a token is back.
            if !send(
                &mut wr,
                &error_response(0, CommandError::RateLimited),
                max,
                wto,
            )
            .await
            {
                return;
            }
            tokio::select! {
                () = ctx.shutdown.cancelled() => return,
                () = tokio::time::sleep(wait) => {}
            }
            continue;
        }
        let req: Request = match serde_json::from_slice(&frame) {
            Ok(r) => r,
            Err(e) => {
                invalid_run += 1;
                inc(&ctx.counters.requests_invalid);
                ctx.peerlog
                    .note(uid, "undecodable request", "invalid frame");
                let id = serde_json::from_slice::<serde_json::Value>(&frame)
                    .ok()
                    .and_then(|v| v.get("id").and_then(serde_json::Value::as_u64))
                    .unwrap_or(0);
                let r = error_response(id, invalid("request", e.to_string()));
                if !send(&mut wr, &r, max, wto).await || invalid_run >= MAX_CONSECUTIVE_INVALID {
                    return;
                }
                continue;
            }
        };
        invalid_run = 0;
        if req.v != WIRE_VERSION {
            let r = error_response(
                req.id,
                invalid(
                    "v",
                    format!(
                        "unsupported version {}, this daemon speaks {WIRE_VERSION}",
                        req.v
                    ),
                ),
            );
            if !send(&mut wr, &r, max, wto).await {
                return;
            }
            continue;
        }
        let id = req.id;
        let reply = match req.body {
            RequestBody::SubscribeEvents => {
                stream_events(rd, wr, dec, id, uid, &ctx, &cfg).await;
                return;
            }
            RequestBody::Query(q) => run_query(&ctx, q, max).await,
            RequestBody::Command(c) => {
                run_command(&ctx, uid, access_now(&ctx), c, Confirm::No).await
            }
            RequestBody::ConfirmedCommand {
                command, expect, ..
            } => {
                let confirm = expect.map_or(Confirm::Blind, Confirm::Expecting);
                run_command(&ctx, uid, access_now(&ctx), command, confirm).await
            }
            _ => Err(CommandError::Unsupported {
                what: "request".into(),
            }),
        };
        let resp = Response {
            v: WIRE_VERSION,
            id,
            result: reply,
        };
        if !send(&mut wr, &resp, max, wto).await {
            return;
        }
    }
}

/// Answer a query; `max_frame` is this connection's frame limit.
async fn run_query(ctx: &ControlCtx, q: Query, max_frame: usize) -> Result<Reply, CommandError> {
    if matches!(q, Query::Metrics) {
        // Clock and bridge state come from the engine, the rest is ours.
        return match ctx.engine.query(Query::Status).await? {
            Reply::Status(s) => Ok(Reply::Metrics(
                ctx.counters.snapshot(s.clocks).with_bridges(s.bridges),
            )),
            _ => Err(CommandError::Internal {
                message: "unexpected reply from the state engine".into(),
            }),
        };
    }
    match ctx.engine.query(q).await? {
        Reply::Status(mut s) => {
            // Restart-class keys changed since the start: say so wherever
            // the status is read (tesseractl status, the portal).
            if let Some(w) = ctx.settings.borrow().restart_warning() {
                s.warnings.push(w);
            }
            // The limit that applies to the connection asking (a reload
            // changes it for new connections only).
            if s.limits.is_some() {
                s.limits = Some(tessera_control_types::Limits::new(max_frame as u64));
            }
            Ok(Reply::Status(s))
        }
        other => Ok(other),
    }
}

/// Is `cmd` a dry run (it changes nothing whoever sends it)?
#[must_use]
pub fn is_dry_run(cmd: &Command) -> bool {
    matches!(cmd, Command::SetDeviceChannels { dry_run: true, .. })
}

async fn run_command(
    ctx: &ControlCtx,
    uid: u32,
    access: Access,
    cmd: Command,
    confirm: Confirm,
) -> Result<Reply, CommandError> {
    let dry_run = is_dry_run(&cmd);
    if !access.can_mutate() && !dry_run {
        inc(&ctx.counters.requests_denied);
        ctx.peerlog
            .note(uid, "refused a command from a read-only peer", cmd.kind());
        return Err(CommandError::PermissionDenied);
    }
    let confirmed = confirm != Confirm::No;
    if dry_run {
        tracing::debug!(uid, kind = cmd.kind(), "control: dry run");
    } else {
        tracing::info!(uid, kind = cmd.kind(), confirmed, "control: command");
    }
    // The shape of an empty impact, for the reply below.
    let empty = (dry_run || confirmed).then(|| tessera_state::impact::no_impact(&cmd));
    let applied = ctx
        .engine
        .apply_with(Origin::Cli { uid }, cmd, confirm)
        .await
        .inspect_err(|e| {
            if matches!(e, CommandError::ConfirmationRequired { .. }) {
                tracing::info!(
                    uid,
                    "control: the command needs confirmation; nothing changed"
                );
            }
        })?;
    if !applied.effects.is_empty() {
        ctx.effects.on_effects(&applied.effects);
    }
    // A dry run and a confirmed command always answer with an impact (an
    // empty one if the engine had nothing to report, a no-op for example),
    // so that clients can rely on the reply's shape; a plain command answers
    // `Done`, as before.
    Ok(match (applied.impact, empty) {
        (Some(i), Some(_)) => Reply::Impact(i),
        (None, Some(e)) => Reply::Impact(e),
        (_, None) => Reply::Done,
    })
}

fn event_response(id: u64, e: Event) -> Response {
    Response {
        v: WIRE_VERSION,
        id,
        result: Ok(Reply::Event(e)),
    }
}

/// Serve an event stream until the peer leaves, stalls or the daemon stops.
async fn stream_events(
    mut rd: OwnedReadHalf,
    mut wr: OwnedWriteHalf,
    mut dec: FrameDecoder,
    id: u64,
    uid: u32,
    ctx: &ControlCtx,
    cfg: &ControlConfig,
) {
    let max = cfg.max_frame;
    let wto = cfg.stream_write_timeout;
    // Subscribe before acknowledging so nothing between the two is missed.
    let mut sub = ctx.bus.subscribe(cfg.event_queue);
    inc(&ctx.counters.subscribers);
    let _gauge = GaugeGuard(&ctx.counters.subscribers);
    let ack = Response {
        v: WIRE_VERSION,
        id,
        result: Ok(Reply::Done),
    };
    if !send(&mut wr, &ack, max, wto).await {
        return;
    }
    let mut buf = vec![0u8; 4096];
    loop {
        enum Step {
            Stop,
            Item(Result<Received, tessera_state::RecvError>),
            Incoming(usize),
        }
        let step = tokio::select! {
            biased;
            () = ctx.shutdown.cancelled() => Step::Stop,
            item = sub.recv_async() => Step::Item(item),
            r = rd.read(&mut buf) => match r {
                Ok(0) | Err(_) => Step::Stop,
                Ok(n) => Step::Incoming(n),
            },
        };
        match step {
            Step::Stop | Step::Item(Err(_)) => return,
            Step::Item(Ok(Received::Event(e))) => {
                if !send(&mut wr, &event_response(id, e), max, wto).await {
                    return;
                }
            }
            Step::Item(Ok(Received::Lagged(dropped))) => {
                inc(&ctx.counters.subscriber_lags);
                ctx.peerlog.note(uid, "subscriber lagged", "events dropped");
                let resp = Response {
                    v: WIRE_VERSION,
                    id,
                    result: Ok(Reply::Lagged { dropped }),
                };
                if !send(&mut wr, &resp, max, wto).await {
                    return;
                }
            }
            Step::Incoming(n) => {
                if let Some(chunk) = buf.get(..n) {
                    dec.push(chunk);
                }
                loop {
                    match dec.next_frame() {
                        Ok(Some(_)) => {
                            if ctx.take_token(uid).is_err() {
                                continue;
                            }
                            let r = error_response(
                                0,
                                invalid("request", "this connection is an event stream"),
                            );
                            if !send(&mut wr, &r, max, cfg.write_timeout).await {
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(_) => return,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::HEADER_LEN;

    fn decode(bytes: &[u8]) -> Response {
        serde_json::from_slice(&bytes[HEADER_LEN..]).unwrap()
    }

    #[test]
    fn a_reply_over_the_cap_becomes_response_too_large_for_the_same_id() {
        let big = Response {
            v: WIRE_VERSION,
            id: 42,
            result: Ok(Reply::EffectiveConfig("x".repeat(100_000))),
        };
        let max = 64 * 1024;
        let bytes = encode_response(&big, max).unwrap();
        assert!(bytes.len() < 1024, "the error frame is small");
        let r = decode(&bytes);
        assert_eq!(r.id, 42);
        let Err(CommandError::ResponseTooLarge { size, limit, key }) = r.result else {
            panic!("{:?}", r.result)
        };
        assert!(size > 100_000);
        assert_eq!(limit, max as u64);
        assert_eq!(key, "control.max_frame_bytes");
        // Under the cap: unchanged.
        let small = Response {
            v: WIRE_VERSION,
            id: 1,
            result: Ok(Reply::Done),
        };
        assert_eq!(decode(&encode_response(&small, max).unwrap()), small);
    }

    #[test]
    fn only_set_device_channels_with_dry_run_is_a_dry_run() {
        use tessera_control_types::DeviceKey;
        let c = |dry_run| Command::SetDeviceChannels {
            device: DeviceKey("main".into()),
            rx: 4,
            tx: 4,
            dry_run,
        };
        assert!(is_dry_run(&c(true)));
        assert!(!is_dry_run(&c(false)));
        assert!(!is_dry_run(&Command::ReloadConfig));
    }
}
