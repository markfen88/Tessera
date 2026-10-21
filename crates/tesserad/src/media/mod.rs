//! The media engine: one real-time worker and its control thread (ADR 0004,
//! ADR 0006).
//!
//! | Part | Thread | Does |
//! |---|---|---|
//! | [`worker::Worker`] | `tessera-media` (`SCHED_FIFO` when permitted) | the tick loop: sockets, flow engines, client rings |
//! | control thread (this module) | `tessera-media-ctl` (normal priority) | builds flows, sessions and sockets (all allocation), hands them to the worker through an SPSC ring, takes retired objects back and drops them, drains events and logs them |
//! | [`MediaHandle`] | anyone | sends requests to the control thread over a channel |
//!
//! The control thread is the **only** producer into the worker's command ring,
//! so that ring needs no lock; everything else talks to the control thread.
//!
//! What flows exist is decided by [`MediaHandle::set_flows`] with fully
//! resolved [`plan::FlowPlan`]s; the call is idempotent. Flows whose plan did
//! not change are left alone, changed or removed flows are replaced, and flows
//! that cannot start (an interface without an address, a port in use) are
//! retried every few seconds while the error is logged once.
//!
//! **PROVISIONAL**: static flows are tessera-only scaffolding until ARC/CMC
//! flow control exists.
//!
//! Metrics: the worker only does relaxed atomic adds ([`stats::EngineCounters`],
//! the per-flow `tessera-media` counters); [`stats::MediaStats`] turns them into
//! the `Metrics` reply.

pub mod clock;
pub mod format;
pub mod keepalive;
pub mod plan;
pub mod stats;
pub mod udp;
pub mod worker;

use std::collections::{HashMap, VecDeque};
use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tessera_audio_ipc::{ClockOverlayReader, SampleFormat, WakeSignal};
use tessera_control_types::Direction;

use self::clock::OverlayClock;
use self::plan::{FlowPlan, Tuning, TxBuilt, build_rx, build_sock, build_tx, sock_key};
use self::stats::MediaStats;
use self::worker::{
    Cmd, EngineEvent, Retired, SessionEnd, SessionRt, Tables, Worker, WorkerConfig,
};

/// Settings of the media engine.
#[derive(Debug, Clone)]
pub struct MediaConfig {
    /// `SCHED_FIFO` priority of the worker (`None`: normal scheduling).
    pub rt_priority: Option<u8>,
    /// CPU to pin the worker to.
    pub cpu: Option<usize>,
    /// `mlockall` at worker start.
    pub mlock: bool,
    /// Worker loop settings (grid tick, batch bounds, ...).
    pub worker: WorkerConfig,
    /// Data path knobs.
    pub tuning: Tuning,
    /// Margin added to the RX latency published to clients on top of one
    /// engine tick, nanoseconds (wake-up jitter allowance).
    pub rx_margin_ns: u64,
    /// Initial capacity of the worker's tables (they grow on demand).
    pub initial_capacity: usize,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            rt_priority: None,
            cpu: None,
            mlock: false,
            worker: WorkerConfig::default(),
            tuning: Tuning::default(),
            rx_margin_ns: 250_000,
            initial_capacity: 64,
        }
    }
}

/// An audio client session to serve.
#[derive(Debug)]
pub struct SessionSpec {
    /// Session id.
    pub id: u64,
    /// Device key.
    pub device: String,
    /// First device channel the ring carries (1-based).
    pub first_channel: u32,
    /// Channels in the ring.
    pub channel_count: u32,
    /// Sample format of the ring.
    pub format: SampleFormat,
    /// The daemon's end of the ring.
    pub end: SessionEnd,
    /// The daemon's end of the wake socket; `None`: no wake-ups (a session
    /// whose consumer does not wait on the engine, such as a hardware-card
    /// bridge).
    pub wake: Option<WakeSignal>,
}

enum Ctl {
    AddSession(Box<SessionSpec>),
    RemoveSession(u64),
    SetFlows(Vec<FlowPlan>),
    SetFreeRunningTx(bool),
    Stop,
}

/// Queue of requests to the control thread.
const QUEUE: usize = 4096;

/// Handle to the media engine.
#[derive(Clone)]
pub struct MediaHandle {
    tx: SyncSender<Ctl>,
    margin_ns: u64,
    stats: Arc<MediaStats>,
    pending_removals: Arc<std::sync::Mutex<Vec<u64>>>,
}

impl std::fmt::Debug for MediaHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MediaHandle")
    }
}

impl MediaHandle {
    /// Start serving an audio session. `false` if the queue is full or the
    /// engine is gone (the caller should refuse the session).
    #[must_use]
    pub fn add_session(&self, spec: SessionSpec) -> bool {
        !matches!(
            self.tx.try_send(Ctl::AddSession(Box::new(spec))),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_))
        )
    }

    /// Stop serving a session (closes the daemon's ring end and wake socket).
    pub fn remove_session(&self, id: u64) {
        // A removal must never be lost (the worker would keep serving a
        // client that is gone): if the queue is full it is parked and the
        // control thread retries it on its next pass.
        if self.tx.try_send(Ctl::RemoveSession(id)).is_err() {
            self.pending_removals
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(id);
        }
    }

    /// Make exactly these flows run (see the module documentation).
    pub fn set_flows(&self, plans: Vec<FlowPlan>) {
        let _ = self.tx.send(Ctl::SetFlows(plans));
    }

    /// `[clock] free_running_tx`.
    pub fn set_free_running_tx(&self, on: bool) {
        let _ = self.tx.send(Ctl::SetFreeRunningTx(on));
    }

    /// How much later than the flow latency an RX client has to read: one
    /// engine tick (data is delivered once per tick) plus the configured
    /// margin. Published to clients as part of the effective latency.
    #[must_use]
    pub fn client_margin_ns(&self) -> u64 {
        self.margin_ns
    }

    /// Counters for `Query::Metrics`.
    #[must_use]
    pub fn stats(&self) -> &Arc<MediaStats> {
        &self.stats
    }

    /// Ask the engine to stop.
    pub fn stop(&self) {
        let _ = self.tx.send(Ctl::Stop);
    }
}

struct Applied {
    /// A second descriptor of an rx flow's socket (see [`plan::RxBuilt::dup`]).
    rx_dup: Option<UdpSocket>,
    plan: FlowPlan,
    key: u64,
    sock: Option<(Ipv4AddrKey, u64)>,
}

type Ipv4AddrKey = (std::net::Ipv4Addr, String, u32, u8, u32);

struct Failed {
    plan: FlowPlan,
    retry_at: Instant,
    error: String,
}

const RETRY: Duration = Duration::from_secs(3);
const LOG_EVERY: Duration = Duration::from_secs(1);

struct Control {
    cfg: MediaConfig,
    stats: Arc<MediaStats>,
    cmd: rtrb::Producer<Cmd>,
    retire: rtrb::Consumer<Retired>,
    events: rtrb::Consumer<EngineEvent>,
    pending: VecDeque<Cmd>,
    devices: HashMap<String, u32>,
    applied: HashMap<String, Applied>,
    /// Sockets of rx flows stopped in the current pass, by (address,
    /// interface, interface index, port), for a replacement to take over. The
    /// index is part of the key: a socket bound to a removed interface is
    /// dead even when a new one has the same name.
    handover: HashMap<(std::net::Ipv4Addr, String, u32, u16), UdpSocket>,
    socks: HashMap<Ipv4AddrKey, (u64, usize)>,
    failed: HashMap<String, Failed>,
    sessions: usize,
    next_key: u64,
    cap: usize,
    counts: [usize; 4],
    names: HashMap<u64, String>,
    last_log: HashMap<&'static str, Instant>,
}

#[derive(Clone, Copy)]
enum Table {
    Rx = 0,
    Tx = 1,
    Sock = 2,
    Session = 3,
}

impl Control {
    fn push(&mut self, c: Cmd) {
        if !self.pending.is_empty() {
            self.pending.push_back(c);
            return;
        }
        if let Err(rtrb::PushError::Full(c)) = self.cmd.push(c) {
            self.pending.push_back(c);
        }
    }

    fn pump(&mut self) {
        while let Some(c) = self.pending.pop_front() {
            if let Err(rtrb::PushError::Full(c)) = self.cmd.push(c) {
                self.pending.push_front(c);
                break;
            }
        }
        while let Ok(r) = self.retire.pop() {
            drop(r); // freed here, never on the worker
        }
        while let Ok(ev) = self.events.pop() {
            self.log_event(ev);
        }
    }

    fn limited(&mut self, what: &'static str) -> bool {
        let now = Instant::now();
        match self.last_log.get(what) {
            Some(t) if now.duration_since(*t) < LOG_EVERY => false,
            _ => {
                self.last_log.insert(what, now);
                true
            }
        }
    }

    fn name(&self, key: u64) -> String {
        self.names
            .get(&key)
            .cloned()
            .unwrap_or_else(|| format!("#{key}"))
    }

    fn log_event(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::RxActive { flow } => {
                tracing::info!("flow `{}`: first packets received", self.name(flow));
            }
            EngineEvent::RxStalled { flow } => {
                tracing::warn!("flow `{}`: no packets (stalled)", self.name(flow));
            }
            EngineEvent::RxResumed { flow } => {
                tracing::info!("flow `{}`: packets again", self.name(flow));
            }
            EngineEvent::TxLateSkip { flow, packets } => {
                if self.limited("tx_late_skip") {
                    tracing::warn!(
                        "flow `{}`: skipped {packets} packet(s) instead of catching up",
                        self.name(flow)
                    );
                }
            }
            EngineEvent::TxSuppressed(on) => {
                if on {
                    tracing::warn!("transmit suppressed: the media clock is not locked");
                } else {
                    tracing::info!("transmit enabled: the media clock is usable");
                }
            }
            EngineEvent::EpochChanged { epoch } => {
                tracing::info!("media clock stepped (epoch {epoch}): client rings were reset");
            }
            EngineEvent::TickSkipped { late_us } => {
                if self.limited("tick_skipped") {
                    tracing::warn!("media engine tick missed by {late_us} us; skipped forward");
                }
            }
            EngineEvent::CommandRejected => {
                tracing::error!("the media worker rejected a command (table full or unknown key)");
            }
        }
    }

    fn reserve(&mut self, t: Table) {
        let i = t as usize;
        if self.counts[i] >= self.cap {
            let new = self.cap.saturating_mul(2).max(self.cap + 1);
            self.push(Cmd::Grow(Box::new(Tables::with_capacity(new))));
            self.cap = new;
        }
        self.counts[i] += 1;
    }

    fn device_index(&mut self, key: &str) -> u32 {
        let next = u32::try_from(self.devices.len()).unwrap_or(u32::MAX);
        *self.devices.entry(key.to_owned()).or_insert(next)
    }

    fn add_session(&mut self, s: SessionSpec) {
        let device = self.device_index(&s.device);
        let first = s.first_channel.saturating_sub(1);
        self.reserve(Table::Session);
        self.sessions += 1;
        self.push(Cmd::AddSession(Box::new(SessionRt {
            id: s.id,
            device,
            first,
            count: s.channel_count,
            format: s.format,
            end: s.end,
            wake: s.wake,
            read_end: 0,
        })));
    }

    fn remove_session(&mut self, id: u64) {
        self.counts[Table::Session as usize] =
            self.counts[Table::Session as usize].saturating_sub(1);
        self.sessions = self.sessions.saturating_sub(1);
        self.push(Cmd::RemoveSession(id));
    }

    fn start_flow(&mut self, p: &FlowPlan) -> Result<(), String> {
        let device = self.device_index(&p.device);
        let key = self.next_key;
        self.next_key += 1;
        match p.direction {
            Direction::Rx => {
                // A flow that replaces one on the same address and port takes
                // over its socket: the port is never unbound, so there is no
                // gap and no "address in use".
                let reuse =
                    self.handover
                        .remove(&(p.local, p.interface.clone(), p.ifindex, p.port));
                let built = build_rx(p, key, device, &self.cfg.tuning, reuse)?;
                self.note_buffer(
                    "receive",
                    &p.id,
                    built.rcvbuf,
                    self.cfg.tuning.socket_buffer,
                );
                self.reserve(Table::Rx);
                self.push(Cmd::AddRx(built.rt));
                self.stats
                    .add_rx(&p.id, &p.device, &p.channels, built.metrics, built.counters);
                self.applied.insert(
                    p.id.clone(),
                    Applied {
                        rx_dup: Some(built.dup),
                        plan: p.clone(),
                        key,
                        sock: None,
                    },
                );
            }
            Direction::Tx => {
                let sk = sock_key(p);
                let skey = match self.socks.get(&sk) {
                    Some(&(k, _)) => k,
                    None => {
                        let k = self.next_key;
                        self.next_key += 1;
                        let s = build_sock(p, k, &self.cfg.tuning)?;
                        self.note_buffer(
                            "send",
                            &p.id,
                            s.sock.effective_sndbuf(),
                            self.cfg.tuning.socket_buffer,
                        );
                        self.reserve(Table::Sock);
                        self.push(Cmd::AddSock(s));
                        self.socks.insert(sk.clone(), (k, 0));
                        k
                    }
                };
                let TxBuilt {
                    rt,
                    metrics,
                    send_failures,
                } = match build_tx(p, key, device, skey) {
                    Ok(v) => v,
                    Err(e) => {
                        self.release_sock(sk);
                        return Err(e);
                    }
                };
                if let Some(e) = self.socks.get_mut(&sk) {
                    e.1 += 1;
                }
                self.reserve(Table::Tx);
                self.push(Cmd::AddTx(rt));
                self.stats.add_tx(&p.id, &p.device, metrics, send_failures);
                self.applied.insert(
                    p.id.clone(),
                    Applied {
                        rx_dup: None,
                        plan: p.clone(),
                        key,
                        sock: Some((sk, skey)),
                    },
                );
            }
        }
        self.names.insert(key, p.id.clone());
        Ok(())
    }

    /// Drop a socket entry that no flow uses.
    fn release_sock(&mut self, sk: Ipv4AddrKey) {
        if let Some(&(key, refs)) = self.socks.get(&sk) {
            if refs == 0 {
                self.socks.remove(&sk);
                self.counts[Table::Sock as usize] =
                    self.counts[Table::Sock as usize].saturating_sub(1);
                self.push(Cmd::RemoveSock(key));
            }
        }
    }

    fn stop_flow(&mut self, id: &str) {
        let Some(a) = self.applied.remove(id) else {
            return;
        };
        self.stats.remove(id);
        self.names.remove(&a.key);
        self.push(Cmd::RemoveFlow(a.key));
        if let Some(dup) = a.rx_dup {
            // Offered to a replacement in the same pass; whatever is not
            // taken is closed at the end of `set_flows`.
            self.handover.insert(
                (
                    a.plan.local,
                    a.plan.interface.clone(),
                    a.plan.ifindex,
                    a.plan.port,
                ),
                dup,
            );
        }
        let t = if a.plan.direction == Direction::Rx {
            Table::Rx
        } else {
            Table::Tx
        };
        self.counts[t as usize] = self.counts[t as usize].saturating_sub(1);
        if let Some((sk, _)) = a.sock {
            if let Some(e) = self.socks.get_mut(&sk) {
                e.1 = e.1.saturating_sub(1);
            }
            self.release_sock(sk);
        }
    }

    fn try_start(&mut self, p: &FlowPlan) {
        match self.start_flow(p) {
            Ok(()) => {
                self.failed.remove(&p.id);
                tracing::info!(
                    "flow `{}` started ({:?}, device `{}`, {} channel(s), port {})",
                    p.id,
                    p.direction,
                    p.device,
                    p.channels.len(),
                    p.port
                );
            }
            Err(e) => {
                let same = self.failed.get(&p.id).is_some_and(|f| f.error == e);
                if !same {
                    tracing::error!(
                        "flow `{}` cannot start (retrying every {} s): {e}",
                        p.id,
                        RETRY.as_secs()
                    );
                }
                self.failed.insert(
                    p.id.clone(),
                    Failed {
                        plan: p.clone(),
                        retry_at: Instant::now() + RETRY,
                        error: e,
                    },
                );
            }
        }
    }

    // Runs on `tessera-media-ctl` (normal priority), never on the real-time
    // worker: the plan clones below allocate, the worker only ever receives
    // built objects through its SPSC ring.
    fn set_flows(&mut self, plans: Vec<FlowPlan>) {
        let desired: HashMap<&str, &FlowPlan> = plans.iter().map(|p| (p.id.as_str(), p)).collect();
        let stale: Vec<String> = self
            .applied
            .iter()
            .filter(|(id, a)| desired.get(id.as_str()).is_none_or(|p| **p != a.plan))
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.stop_flow(&id);
            tracing::info!("flow `{id}` stopped");
        }
        self.failed
            .retain(|id, f| desired.get(id.as_str()).is_some_and(|p| **p == f.plan));
        // Kept flows take the new values of the fields that are not part of
        // their identity (the keepalive, the default lag), for when they
        // next (re)start: a retry of a failed flow, a rebuild.
        for (id, a) in &mut self.applied {
            if let Some(p) = desired.get(id.as_str()) {
                a.plan = (*p).clone();
            }
        }
        for (id, f) in &mut self.failed {
            if let Some(p) = desired.get(id.as_str()) {
                f.plan = (*p).clone();
            }
        }
        for p in &plans {
            if !self.applied.contains_key(&p.id) && !self.failed.contains_key(&p.id) {
                self.try_start(p);
            }
        }
        self.handover.clear();
    }

    /// Log the effective size of a socket buffer and count it if the kernel
    /// capped it below what was asked for (it reads back twice the value it
    /// accepted).
    fn note_buffer(&self, what: &str, flow: &str, effective: usize, wanted: usize) {
        let e = &self.stats.engine;
        // Only this thread writes it; 0 means "no socket yet".
        let cur = e.socket_buffer_min_bytes.load(Ordering::Relaxed);
        if cur == 0 || (effective as u64) < cur {
            e.socket_buffer_min_bytes
                .store(effective as u64, Ordering::Relaxed);
        }
        if effective < wanted.saturating_mul(2) {
            e.socket_buffers_capped.inc();
            tracing::warn!(
                "flow `{flow}`: the {what} buffer is {} KiB, capped below the {} KiB asked for; raise net.core.rmem_max / wmem_max (dist/sysctl.d/tessera.conf)",
                effective / 1024,
                wanted / 1024
            );
        } else {
            tracing::info!("flow `{flow}`: {what} buffer {} KiB", effective / 1024);
        }
    }

    fn retry_failed(&mut self) {
        let now = Instant::now();
        let due: Vec<FlowPlan> = self
            .failed
            .values()
            .filter(|f| f.retry_at <= now)
            .map(|f| f.plan.clone())
            .collect();
        for p in due {
            self.try_start(&p);
        }
    }
}

fn setup_rt(cfg: &MediaConfig, stats: &MediaStats) {
    crate::rtsys::prefault_stack();
    let e = &stats.engine;
    if let Some(prio) = cfg.rt_priority {
        if !crate::rtsys::set_fifo(prio) {
            e.rt_degraded.store(1, Ordering::Relaxed);
            tracing::warn!(
                "media engine: SCHED_FIFO {prio} was not granted (RLIMIT_RTPRIO / CAP_SYS_NICE); running at normal priority, expect jitter"
            );
        }
    }
    if let Some(cpu) = cfg.cpu {
        if !crate::rtsys::pin_cpu(cpu) {
            e.rt_degraded.store(1, Ordering::Relaxed);
            tracing::warn!("media engine: cannot pin to CPU {cpu}");
        }
    }
    if cfg.mlock && !crate::rtsys::lock_memory() {
        e.mlock_failed.store(1, Ordering::Relaxed);
        tracing::warn!(
            "media engine: mlockall refused (RLIMIT_MEMLOCK / CAP_IPC_LOCK); memory may be paged out"
        );
    }
}

/// Start the media engine.
///
/// # Errors
/// The clock overlay cannot be mapped or a thread cannot be spawned.
pub fn spawn(
    cfg: MediaConfig,
    reader: ClockOverlayReader,
    stats: Arc<MediaStats>,
) -> std::io::Result<(MediaHandle, JoinHandle<()>)> {
    let (cmd_tx, cmd_rx) = rtrb::RingBuffer::<Cmd>::new(1024);
    let (retire_tx, retire_rx) = rtrb::RingBuffer::<Retired>::new(1024);
    let (ev_tx, ev_rx) = rtrb::RingBuffer::<EngineEvent>::new(1024);
    let tables = Tables::with_capacity(cfg.initial_capacity.max(1));
    let worker = Worker::new(
        cfg.worker.clone(),
        OverlayClock::new(reader),
        tables,
        cmd_rx,
        retire_tx,
        ev_tx,
        Arc::clone(&stats),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let (stop_w, stats_w, cfg_w) = (Arc::clone(&stop), Arc::clone(&stats), cfg.clone());
    let worker_join = std::thread::Builder::new()
        .name("tessera-media".into())
        .spawn(move || {
            let _abort = crate::rt::AbortOnPanic;
            let mut worker = worker;
            setup_rt(&cfg_w, &stats_w);
            stats_w.engine.running.store(1, Ordering::Relaxed);
            worker.run(&stop_w);
            stats_w.engine.running.store(0, Ordering::Relaxed);
        })?;

    let (tx, rx) = mpsc::sync_channel::<Ctl>(QUEUE);
    let pending: Arc<std::sync::Mutex<Vec<u64>>> = Arc::default();
    let pending_for_handle = Arc::clone(&pending);
    let margin_ns = cfg.worker.tick_ns + cfg.rx_margin_ns;
    let cap = cfg.initial_capacity.max(1);
    let mut ctl = Control {
        cfg,
        stats: Arc::clone(&stats),
        cmd: cmd_tx,
        retire: retire_rx,
        events: ev_rx,
        pending: VecDeque::new(),
        devices: HashMap::new(),
        applied: HashMap::new(),
        handover: HashMap::new(),
        socks: HashMap::new(),
        failed: HashMap::new(),
        sessions: 0,
        next_key: 1,
        cap,
        counts: [0; 4],
        names: HashMap::new(),
        last_log: HashMap::new(),
    };
    let join = std::thread::Builder::new()
        .name("tessera-media-ctl".into())
        .spawn(move || {
            let _abort = crate::rt::AbortOnPanic;
            loop {
                match rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(Ctl::AddSession(s)) => ctl.add_session(*s),
                    Ok(Ctl::RemoveSession(id)) => ctl.remove_session(id),
                    Ok(Ctl::SetFlows(p)) => ctl.set_flows(p),
                    Ok(Ctl::SetFreeRunningTx(v)) => ctl.push(Cmd::SetFreeRunningTx(v)),
                    Ok(Ctl::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {}
                }
                let parked: Vec<u64> = std::mem::take(
                    &mut *pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                );
                for id in parked {
                    ctl.remove_session(id);
                }
                ctl.pump();
                ctl.retry_failed();
            }
            // Stop the worker, then free what it hands back.
            stop.store(true, Ordering::Relaxed);
            ctl.push(Cmd::Stop);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !worker_join.is_finished() && Instant::now() < deadline {
                ctl.pump();
                std::thread::sleep(Duration::from_millis(2));
            }
            if worker_join.is_finished() {
                let _ = worker_join.join();
            } else {
                tracing::error!("the media worker did not stop within 5 s");
            }
            ctl.pump();
        })?;
    Ok((
        MediaHandle {
            tx,
            margin_ns,
            stats,
            pending_removals: pending_for_handle,
        },
        join,
    ))
}
