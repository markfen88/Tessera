//! The real-time media worker (ADR 0004, ADR 0006).
//!
//! One thread owns every socket, flow engine and audio ring of the daemon's
//! media path. Its loop sleeps to an **absolute deadline**
//! (`clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME)`) that is the earlier of
//! the next *grid tick* and the instant the next TX packet becomes due (mapped
//! from media time to local time through the clock overlay), and then runs
//! [`Worker::tick`]:
//!
//! 1. apply the commands the control thread queued (flows, sessions, sockets
//!    arrive as pre-built boxed objects; retired ones go back the same way,
//!    so nothing is allocated or freed here);
//! 2. one atomic read of the media clock; on an **epoch change** the RX
//!    session rings are reset (ADR 0005: nothing from an earlier epoch may be
//!    heard);
//! 3. on a grid tick: **drain every RX socket** with `recvmmsg` (always, even
//!    with nobody listening), `RxFlow::ingest` each datagram with its kernel
//!    receive time, then `RxFlow::tick` (the epoch check, mandatory every
//!    tick) and deliver the span of positions that became due since the last
//!    tick (`RxFlow::read`) into the RX session rings; send a due receiver
//!    keepalive from the flow's socket to the source of its media (one
//!    `sendto` per flow per 250 ms, `keepalive` module);
//! 4. on every tick: `TxFlow::poll` each TX flow until nothing is due, packets
//!    collected per socket and sent with one `sendmmsg`; TX is suppressed
//!    (flows reset, nothing sent) unless the clock is `locked`, `leader` or in
//!    holdover, or `free_running_tx` is set (ADR 0005);
//! 5. on a grid tick: one wake byte to every attached client session.
//!
//! **Never bursts.** A grid tick that was missed by more than a tick skips
//! forward (counted); TX flows skip to the latest packet that is due
//! (`TxFlow` counts a late skip); per-tick work is bounded (receive batches,
//! TX packets, delivered span).
//!
//! **Real-time rules.** After start-up the loop makes no allocation, takes no
//! lock, logs nothing and makes no syscall other than the socket I/O it exists
//! for, the wake byte, `clock_gettime` (vDSO) and its own sleep. Everything it
//! has to report is a relaxed atomic add ([`EngineCounters`]) or a `Copy`
//! [`EngineEvent`] pushed into an SPSC ring. `tests/` proves the steady-state
//! tick allocation-free with the counting allocator. A panic aborts the
//! process ([`crate::rt::AbortOnPanic`]).

use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rtrb::{Consumer, Producer};
use tessera_audio_ipc::{RingConsumer, RingProducer, SampleFormat, WakeSignal, monotonic_ns};
use tessera_clock::platform::{OverlaySnapshot, realtime_ns};
use tessera_media::convert::{f32_to_i32, i32_to_f32};
use tessera_media::time::samples_to_ns;
use tessera_media::{FrameSource, IngestOutcome, MediaClock, RxFlow, RxMetrics, TxFlow, TxPoll};

use super::clock::{FrozenClock, OverlayClock};
use super::format::AnyFormat;
use super::keepalive::Keepalive;
use super::stats::{EngineCounters, MediaStats, RxFlowCounters, wake_bucket};
use super::udp::{RxSock, TxSock};
use crate::rtsys;

/// Which end of a client ring the daemon holds.
#[derive(Debug)]
pub enum SessionEnd {
    /// RX endpoint: the daemon writes network audio.
    Producer(RingProducer),
    /// TX endpoint: the daemon reads what the client wrote.
    Consumer(RingConsumer),
}

/// One attached audio client, as the worker sees it.
#[derive(Debug)]
pub struct SessionRt {
    /// Session id (from `audio.sock`).
    pub id: u64,
    /// Device index.
    pub device: u32,
    /// Device channel (0-based) of plane 0.
    pub first: u32,
    /// Planes in the ring.
    pub count: u32,
    /// Sample format of the ring.
    pub format: SampleFormat,
    /// The ring end the daemon holds.
    pub end: SessionEnd,
    /// Wake socket (daemon side); `None` for a session nobody waits on (a
    /// hardware-card bridge, which runs on its card's clock).
    pub wake: Option<WakeSignal>,
    /// Highest end position read from a TX ring (published at the end of
    /// the tick for the client's metrics).
    pub read_end: u64,
}

/// A running RX flow.
#[derive(Debug)]
pub struct RxRt {
    /// Flow key (unique within the process).
    pub key: u64,
    /// Device index.
    pub device: u32,
    /// Sample rate.
    pub rate: u32,
    /// Socket.
    pub sock: RxSock,
    /// The flow engine.
    pub flow: RxFlow<AnyFormat>,
    /// Device channel (0-based) of each flow channel.
    pub chan: Vec<u32>,
    /// Only accept datagrams from this address.
    pub peer: Option<Ipv4Addr>,
    /// Next media position to deliver to the rings.
    pub cursor: Option<u64>,
    /// Scratch for one chunk of one channel.
    pub scratch: Vec<i32>,
    /// Scratch for `f32` rings.
    pub scratch_f: Vec<f32>,
    /// Counters (shared with the control plane).
    pub metrics: Arc<RxMetrics>,
    /// Local time of the last datagram.
    pub last_rx: u64,
    /// At least one datagram arrived.
    pub seen: bool,
    /// Reported as stalled.
    pub stalled: bool,
    /// Receiver keepalive scheduler (`keepalive` module).
    pub keepalive: Keepalive,
    /// Keepalive counters (shared with the control plane).
    pub counters: Arc<RxFlowCounters>,
}

/// A running TX flow.
#[derive(Debug)]
pub struct TxRt {
    /// Flow key.
    pub key: u64,
    /// Device index.
    pub device: u32,
    /// Key of the socket it sends through.
    pub sock_key: u64,
    /// Index into the worker's socket table (maintained by the worker).
    pub sock: usize,
    /// The flow engine.
    pub flow: TxFlow<AnyFormat>,
    /// Device channel (0-based) of each wire position.
    pub chan: Vec<u32>,
    /// Destination.
    pub dest: SocketAddrV4,
    /// Datagrams of this flow the kernel refused (shared with the control
    /// plane).
    pub send_failures: Arc<tessera_media::Counter>,
    /// Scratch for `f32` rings (frames per packet long).
    pub scratch_f: Vec<f32>,
}

/// A shared TX socket.
#[derive(Debug)]
pub struct TxSockRt {
    /// Socket key.
    pub key: u64,
    /// The socket.
    pub sock: TxSock,
    /// Datagrams queued for the next flush.
    pub queued: usize,
}

/// The worker's tables. Capacities are reserved by the control thread; the
/// worker never grows them (it swaps in larger, empty ones on request).
#[derive(Debug, Default)]
pub struct Tables {
    /// RX flows.
    pub rx: Vec<Box<RxRt>>,
    /// TX flows.
    pub tx: Vec<Box<TxRt>>,
    /// TX sockets.
    pub socks: Vec<Box<TxSockRt>>,
    /// Sessions.
    pub sessions: Vec<Box<SessionRt>>,
}

impl Tables {
    /// Empty tables with room for `n` entries each.
    #[must_use]
    pub fn with_capacity(n: usize) -> Self {
        Self {
            rx: Vec::with_capacity(n),
            tx: Vec::with_capacity(n),
            socks: Vec::with_capacity(n),
            sessions: Vec::with_capacity(n),
        }
    }

    /// The smallest capacity of the four tables.
    #[must_use]
    pub fn min_capacity(&self) -> usize {
        self.rx
            .capacity()
            .min(self.tx.capacity())
            .min(self.socks.capacity())
            .min(self.sessions.capacity())
    }
}

/// Control thread to worker.
#[derive(Debug)]
pub enum Cmd {
    /// Start an RX flow.
    AddRx(Box<RxRt>),
    /// Start a TX flow.
    AddTx(Box<TxRt>),
    /// Add a TX socket.
    AddSock(Box<TxSockRt>),
    /// Attach a client session.
    AddSession(Box<SessionRt>),
    /// Stop a flow (rx or tx) by key.
    RemoveFlow(u64),
    /// Remove a TX socket by key (no flow may use it).
    RemoveSock(u64),
    /// Detach a session by id.
    RemoveSession(u64),
    /// Swap in larger, empty tables (their contents move over).
    Grow(Box<Tables>),
    /// `[clock] free_running_tx`.
    SetFreeRunningTx(bool),
    /// Leave the loop after this tick.
    Stop,
}

/// Worker to control thread: objects to drop on the control side.
#[derive(Debug)]
pub enum Retired {
    /// An RX flow.
    Rx(Box<RxRt>),
    /// A TX flow.
    Tx(Box<TxRt>),
    /// A TX socket.
    Sock(Box<TxSockRt>),
    /// A session.
    Session(Box<SessionRt>),
    /// Replaced tables.
    Tables(Box<Tables>),
}

/// A `Copy` event for the control thread (which logs it, rate-limited).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineEvent {
    /// First datagram of an rx flow.
    RxActive {
        /// Flow key.
        flow: u64,
    },
    /// An rx flow has been silent for the stall time.
    RxStalled {
        /// Flow key.
        flow: u64,
    },
    /// Datagrams arrive again.
    RxResumed {
        /// Flow key.
        flow: u64,
    },
    /// A tx flow skipped packets instead of catching up.
    TxLateSkip {
        /// Flow key.
        flow: u64,
        /// Packets skipped.
        packets: u32,
    },
    /// TX started or stopped being suppressed (clock not usable).
    TxSuppressed(bool),
    /// The media clock epoch changed (media time was stepped).
    EpochChanged {
        /// New epoch.
        epoch: u64,
    },
    /// The grid tick was missed by more than a tick.
    TickSkipped {
        /// Lateness, microseconds.
        late_us: u32,
    },
    /// A command could not be applied (table full or unknown key).
    CommandRejected,
}

/// Worker settings (all from configuration; rule 7).
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Grid tick, nanoseconds.
    pub tick_ns: u64,
    /// Sleep when there is nothing to do, nanoseconds.
    pub idle_ns: u64,
    /// Receive batches per socket per tick.
    pub rx_max_batches: usize,
    /// TX packets per flow per tick.
    pub tx_max_per_tick: usize,
    /// Send while the clock is not locked.
    pub free_running_tx: bool,
    /// An overlay not refreshed for this long means a dead clock thread.
    pub clock_stale_ns: u64,
    /// An rx flow with no datagram for this long is reported stalled.
    pub rx_stall_ns: u64,
    /// Largest span (frames) delivered to rings in one tick.
    pub max_span_frames: u64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            tick_ns: 1_000_000,
            idle_ns: 20_000_000,
            rx_max_batches: 8,
            tx_max_per_tick: 32,
            free_running_tx: false,
            clock_stale_ns: 5_000_000_000,
            rx_stall_ns: 500_000_000,
            max_span_frames: 4096,
        }
    }
}

/// Capacity of the retire backlog (objects that could not be queued yet).
const BACKLOG: usize = 64;
/// Minimum sleep when a flow is due but was not sent this tick.
const MIN_GAP_NS: u64 = 50_000;

/// The worker. Build it on the control thread, then move it to its thread and
/// call [`run`](Self::run) (or drive [`tick`](Self::tick) directly in tests).
pub struct Worker {
    cfg: WorkerConfig,
    clock: OverlayClock,
    t: Tables,
    cmd: Consumer<Cmd>,
    retire: Producer<Retired>,
    backlog: Vec<Retired>,
    events: Producer<EngineEvent>,
    stats: Arc<MediaStats>,
    last_epoch: Option<u64>,
    next_grid: u64,
    slept_to: u64,
    was_idle: bool,
    tx_suppressed: bool,
    stopping: bool,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker").finish_non_exhaustive()
    }
}

/// `FrameSource` over the TX client rings of one device.
struct TxSource<'a> {
    sessions: &'a mut [Box<SessionRt>],
    device: u32,
    chan: &'a [u32],
    scratch_f: &'a mut [f32],
}

impl FrameSource for TxSource<'_> {
    fn read(&mut self, channel: usize, pos: u64, out: &mut [i32]) -> usize {
        let Some(&dev_ch) = self.chan.get(channel) else {
            out.fill(0);
            return 0;
        };
        for s in self.sessions.iter_mut() {
            if s.device != self.device || dev_ch < s.first || dev_ch >= s.first + s.count {
                continue;
            }
            let SessionEnd::Consumer(c) = &mut s.end else {
                continue;
            };
            let plane = dev_ch - s.first;
            let valid = match s.format {
                SampleFormat::F32 => match self.scratch_f.get_mut(..out.len()) {
                    Some(tmp) => c.read::<f32>(plane, pos, tmp).map(|r| {
                        for (o, v) in out.iter_mut().zip(tmp.iter()) {
                            *o = f32_to_i32(*v);
                        }
                        r.valid
                    }),
                    None => {
                        out.fill(0);
                        return 0;
                    }
                },
                _ => c.read::<i32>(plane, pos, out).map(|r| r.valid),
            };
            s.read_end = s.read_end.max(pos.saturating_add(out.len() as u64));
            return valid.unwrap_or_else(|_| {
                out.fill(0);
                0
            });
        }
        out.fill(0);
        0
    }
}

fn flush(sock: &mut TxSockRt, st: &EngineCounters) {
    if sock.queued == 0 {
        return;
    }
    let r = sock.sock.flush(sock.queued);
    st.tx_batches.inc();
    st.tx_datagrams.add(r.sent as u64);
    st.tx_send_failures.add(r.failed as u64);
    sock.queued = 0;
}

fn emit(events: &mut Producer<EngineEvent>, st: &EngineCounters, ev: EngineEvent) {
    if events.push(ev).is_err() {
        st.events_dropped.inc();
    }
}

impl Worker {
    /// Builds a worker around its channels and tables.
    #[must_use]
    pub fn new(
        cfg: WorkerConfig,
        clock: OverlayClock,
        tables: Tables,
        cmd: Consumer<Cmd>,
        retire: Producer<Retired>,
        events: Producer<EngineEvent>,
        stats: Arc<MediaStats>,
    ) -> Self {
        let now = monotonic_ns();
        Self {
            next_grid: now + cfg.tick_ns,
            cfg,
            clock,
            t: tables,
            cmd,
            retire,
            backlog: Vec::with_capacity(BACKLOG),
            events,
            stats,
            last_epoch: None,
            slept_to: 0,
            was_idle: true,
            tx_suppressed: false,
            stopping: false,
        }
    }

    /// Runs until `stop` is set or a [`Cmd::Stop`] arrives.
    pub fn run(&mut self, stop: &AtomicBool) {
        let mut deadline = monotonic_ns();
        while !self.stopping && !stop.load(Ordering::Relaxed) {
            rtsys::sleep_until(deadline);
            self.slept_to = deadline;
            deadline = self.tick();
        }
    }

    fn retire(&mut self, r: Retired) {
        match self.retire.push(r) {
            Ok(()) => {}
            Err(rtrb::PushError::Full(r)) => {
                if self.backlog.len() < self.backlog.capacity() {
                    self.backlog.push(r);
                } else {
                    // Freeing here would break the RT rules; leaking is the
                    // lesser evil and is counted.
                    self.stats.engine.retire_leaks.inc();
                    std::mem::forget(r);
                }
            }
        }
    }

    fn drain_backlog(&mut self) {
        while let Some(r) = self.backlog.pop() {
            if let Err(rtrb::PushError::Full(r)) = self.retire.push(r) {
                self.backlog.push(r);
                break;
            }
        }
    }

    fn reindex_socks(&mut self) {
        for f in &mut self.t.tx {
            f.sock = self
                .t
                .socks
                .iter()
                .position(|s| s.key == f.sock_key)
                .unwrap_or(usize::MAX);
        }
    }

    fn reject(&mut self) {
        emit(
            &mut self.events,
            &self.stats.engine,
            EngineEvent::CommandRejected,
        );
    }

    fn apply_commands(&mut self) {
        while let Ok(cmd) = self.cmd.pop() {
            match cmd {
                Cmd::AddRx(b) => {
                    if self.t.rx.len() < self.t.rx.capacity() {
                        self.t.rx.push(b);
                    } else {
                        self.reject();
                        self.retire(Retired::Rx(b));
                    }
                }
                Cmd::AddTx(b) => {
                    if self.t.tx.len() < self.t.tx.capacity() {
                        self.t.tx.push(b);
                        self.reindex_socks();
                    } else {
                        self.reject();
                        self.retire(Retired::Tx(b));
                    }
                }
                Cmd::AddSock(b) => {
                    if self.t.socks.len() < self.t.socks.capacity() {
                        self.t.socks.push(b);
                        self.reindex_socks();
                    } else {
                        self.reject();
                        self.retire(Retired::Sock(b));
                    }
                }
                Cmd::AddSession(b) => {
                    if self.t.sessions.len() < self.t.sessions.capacity() {
                        self.t.sessions.push(b);
                    } else {
                        self.reject();
                        self.retire(Retired::Session(b));
                    }
                }
                Cmd::RemoveFlow(key) => {
                    if let Some(i) = self.t.rx.iter().position(|f| f.key == key) {
                        let f = self.t.rx.swap_remove(i);
                        self.retire(Retired::Rx(f));
                    } else if let Some(i) = self.t.tx.iter().position(|f| f.key == key) {
                        let f = self.t.tx.swap_remove(i);
                        self.retire(Retired::Tx(f));
                    } else {
                        self.reject();
                    }
                }
                Cmd::RemoveSock(key) => {
                    if let Some(i) = self.t.socks.iter().position(|s| s.key == key) {
                        let s = self.t.socks.swap_remove(i);
                        self.retire(Retired::Sock(s));
                        self.reindex_socks();
                    } else {
                        self.reject();
                    }
                }
                Cmd::RemoveSession(id) => {
                    if let Some(i) = self.t.sessions.iter().position(|s| s.id == id) {
                        let s = self.t.sessions.swap_remove(i);
                        self.retire(Retired::Session(s));
                    }
                }
                Cmd::Grow(mut b) => {
                    // Swap contents: the box (now holding the old tables)
                    // goes back to be freed on the control side.
                    std::mem::swap(&mut self.t, &mut *b);
                    if b.rx.len() <= self.t.rx.capacity()
                        && b.tx.len() <= self.t.tx.capacity()
                        && b.socks.len() <= self.t.socks.capacity()
                        && b.sessions.len() <= self.t.sessions.capacity()
                    {
                        self.t.rx.append(&mut b.rx);
                        self.t.tx.append(&mut b.tx);
                        self.t.socks.append(&mut b.socks);
                        self.t.sessions.append(&mut b.sessions);
                    } else {
                        // Too small to take the contents: undo.
                        std::mem::swap(&mut self.t, &mut *b);
                        self.reject();
                    }
                    self.retire(Retired::Tables(b));
                }
                Cmd::SetFreeRunningTx(v) => self.cfg.free_running_tx = v,
                Cmd::Stop => self.stopping = true,
            }
        }
        let st = &self.stats.engine;
        st.sessions
            .store(self.t.sessions.len() as u64, Ordering::Relaxed);
        st.flows.store(
            (self.t.rx.len() + self.t.tx.len()) as u64,
            Ordering::Relaxed,
        );
    }

    /// One engine tick. Returns the local monotonic time to sleep until.
    pub fn tick(&mut self) -> u64 {
        let t0 = monotonic_ns();
        self.stats.engine.ticks.inc();
        if self.slept_to != 0 {
            let late_us = t0.saturating_sub(self.slept_to) / 1_000;
            self.stats.engine.max_wake_lateness_us.record_max(late_us);
            if let Some(c) = self
                .stats
                .engine
                .wake_lateness_hist
                .0
                .get(wake_bucket(late_us))
            {
                c.inc();
            }
        }
        self.apply_commands();
        self.drain_backlog();

        let idle = self.t.rx.is_empty() && self.t.tx.is_empty() && self.t.sessions.is_empty();
        if self.was_idle && !idle {
            self.next_grid = t0;
        }
        self.was_idle = idle;
        let grid = !idle && t0 >= self.next_grid;

        // One atomic read of the clock per tick for the decisions below.
        let snap = self.clock.snapshot();
        let frozen = FrozenClock::new(snap);
        if let Some(s) = snap {
            if self.last_epoch != Some(s.epoch) {
                if self.last_epoch.is_some() {
                    for sess in &mut self.t.sessions {
                        if let SessionEnd::Producer(p) = &mut sess.end {
                            p.reset();
                        }
                    }
                    emit(
                        &mut self.events,
                        &self.stats.engine,
                        EngineEvent::EpochChanged { epoch: s.epoch },
                    );
                }
                self.last_epoch = Some(s.epoch);
            }
        }

        if grid {
            self.rx_phase(t0, &frozen);
        }
        let tx_allowed = self.tx_phase(t0, snap.as_ref(), &frozen);
        if grid {
            for s in &self.t.sessions {
                if s.wake.as_ref().is_some_and(|w| w.signal().is_ok()) {
                    self.stats.engine.wake_signals.inc();
                }
            }
            let tick = self.cfg.tick_ns.max(1);
            let mut next = self.next_grid + tick;
            if next <= t0 {
                // Missed by at least a tick: skip forward, never replay.
                let behind = (t0 - self.next_grid) / tick;
                next = self.next_grid + (behind + 1) * tick;
                self.stats.engine.tick_skips.inc();
                emit(
                    &mut self.events,
                    &self.stats.engine,
                    EngineEvent::TickSkipped {
                        late_us: u32::try_from((t0 - self.next_grid) / 1_000).unwrap_or(u32::MAX),
                    },
                );
            }
            self.next_grid = next;
        }
        for s in &mut self.t.sessions {
            if let SessionEnd::Consumer(c) = &mut s.end {
                c.publish_read_pos(s.read_end);
            }
        }

        // When to wake next.
        let now = monotonic_ns();
        let mut next = if idle {
            t0 + self.cfg.idle_ns
        } else {
            self.next_grid
        };
        if tx_allowed {
            if let Some(s) = snap.as_ref() {
                for f in &self.t.tx {
                    if let Some(due) = f.flow.next_due_ns() {
                        // +1 us: rounding in the inverse map must not wake us
                        // a hair before the packet is due.
                        next = next.min(self.clock.local_for_media(s, due) + 1_000);
                    }
                }
            }
        }
        if next <= now {
            next = now + MIN_GAP_NS;
        }
        self.stats
            .engine
            .max_tick_work_us
            .record_max(now.saturating_sub(t0) / 1_000);
        next
    }

    fn rx_phase(&mut self, now: u64, frozen: &FrozenClock) {
        let st = &self.stats.engine;
        // Kernel stamps are CLOCK_REALTIME; one paired reading maps them.
        let (m1, r, m2) = (monotonic_ns(), realtime_ns(), monotonic_ns());
        let real_off = i128::from(m1 / 2 + m2 / 2) - i128::from(r);
        let Self { t, cfg, events, .. } = self;
        for f in &mut t.rx {
            let mut got = false;
            let mut batches = 0;
            loop {
                let n = match f.sock.recv() {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => {
                        st.rx_errors.inc();
                        break;
                    }
                };
                st.rx_batches.inc();
                st.rx_datagrams.add(n as u64);
                for i in 0..n {
                    let Some(d) = f.sock.datagram(i) else {
                        continue;
                    };
                    if d.truncated {
                        st.rx_truncated.inc();
                        f.metrics.malformed_packets.inc();
                        continue;
                    }
                    if f.peer.is_some() && d.src.map(|a| *a.ip()) != f.peer {
                        st.rx_filtered.inc();
                        continue;
                    }
                    let local = match d.stamp_realtime_ns {
                        Some(s) => {
                            let m = i128::from(s) + real_off;
                            u64::try_from(m)
                                .unwrap_or(now)
                                .clamp(now.saturating_sub(1_000_000_000), now)
                        }
                        None => now,
                    };
                    let outcome = f.flow.ingest(d.data, local, frozen);
                    if let IngestOutcome::Accepted {
                        timestamp, frames, ..
                    } = outcome
                    {
                        let sendable =
                            samples_to_ns(timestamp.saturating_add(frames as u64), f.rate);
                        let transit =
                            i128::from(frozen.at_local(local).media_ns) - i128::from(sendable);
                        st.record_transit(i64::try_from(transit).unwrap_or(i64::MAX));
                    }
                    // Media that decoded (even if too late or too early to
                    // play) proves the sender alive: keepalives go to it.
                    // Anything that does not decode never moves them.
                    if outcome != IngestOutcome::Malformed {
                        if let Some(src) = d.src {
                            f.keepalive.on_media(src, now);
                        }
                    }
                    got = true;
                }
                batches += 1;
                if n < f.sock.batch() {
                    break;
                }
                if batches >= cfg.rx_max_batches {
                    st.rx_batch_limit_hits.inc();
                    break;
                }
            }
            if got {
                f.last_rx = now;
                if !f.seen {
                    f.seen = true;
                    emit(events, st, EngineEvent::RxActive { flow: f.key });
                } else if f.stalled {
                    emit(events, st, EngineEvent::RxResumed { flow: f.key });
                }
                f.stalled = false;
            } else if f.seen && !f.stalled && now.saturating_sub(f.last_rx) > cfg.rx_stall_ns {
                f.stalled = true;
                emit(events, st, EngineEvent::RxStalled { flow: f.key });
            }
            if let Some(dest) = f.keepalive.poll(now) {
                // Receiver keepalive (`keepalive` module): one small
                // non-blocking `sendto` per flow per interval (250 ms) on the
                // socket this worker owns, i.e. the socket I/O the worker
                // exists for. It neither allocates nor blocks; a refusal is
                // counted, never retried.
                if f.sock.send_to(f.keepalive.payload(), dest).is_ok() {
                    f.counters.keepalives_sent.inc();
                } else {
                    f.counters.keepalive_failures.inc();
                }
            }
            deliver(f, &mut t.sessions, frozen, cfg.max_span_frames);
        }
    }

    /// Polls the TX flows and sends. Returns whether TX is allowed now.
    fn tx_phase(&mut self, now: u64, snap: Option<&OverlaySnapshot>, frozen: &FrozenClock) -> bool {
        let st = &self.stats.engine;
        let allowed = match snap {
            Some(s) => {
                (OverlayClock::state_allows_tx(s) || self.cfg.free_running_tx)
                    && now.saturating_sub(s.updated_mono_ns) <= self.cfg.clock_stale_ns
            }
            None => self.cfg.free_running_tx,
        };
        if self.t.tx.is_empty() {
            return allowed;
        }
        if allowed == self.tx_suppressed {
            self.tx_suppressed = !allowed;
            emit(
                &mut self.events,
                st,
                EngineEvent::TxSuppressed(self.tx_suppressed),
            );
        }
        let Self { t, cfg, events, .. } = self;
        let Tables {
            tx,
            socks,
            sessions,
            ..
        } = t;
        if !allowed {
            st.tx_suppressed_ticks.inc();
            for f in tx.iter_mut() {
                f.flow.reset();
            }
            return false;
        }
        for f in tx.iter_mut() {
            let Some(sock) = socks.get_mut(f.sock) else {
                // No socket: never leave a due packet that would keep
                // waking the worker.
                f.flow.reset();
                continue;
            };
            for _ in 0..cfg.tx_max_per_tick {
                if sock.queued >= sock.sock.batch() {
                    flush(sock, st);
                }
                let i = sock.queued;
                let Some(slot) = sock.sock.slot(i) else {
                    break;
                };
                let mut src = TxSource {
                    sessions: sessions.as_mut_slice(),
                    device: f.device,
                    chan: &f.chan,
                    scratch_f: &mut f.scratch_f,
                };
                match f.flow.poll(frozen, &mut src, slot) {
                    Ok(TxPoll::Sent(p)) => {
                        sock.sock.queue(i, p.len, f.dest, &f.send_failures);
                        sock.queued += 1;
                        if p.skipped > 0 {
                            emit(
                                events,
                                st,
                                EngineEvent::TxLateSkip {
                                    flow: f.key,
                                    packets: u32::try_from(p.skipped).unwrap_or(u32::MAX),
                                },
                            );
                        }
                    }
                    Ok(TxPoll::Skipped { .. }) => {}
                    Ok(TxPoll::NotDue) => break,
                    Err(_) => {
                        // An encoding problem would otherwise leave the packet
                        // due forever and wake the worker in a loop: count it
                        // and start over at the next tick.
                        st.tx_send_failures.inc();
                        f.flow.reset();
                        break;
                    }
                }
            }
        }
        for s in socks.iter_mut() {
            flush(s, st);
        }
        true
    }
}

/// Deliver the span of positions that became due since the last tick into the
/// RX rings of the sessions that cover each channel.
fn deliver<C: MediaClock>(f: &mut RxRt, sessions: &mut [Box<SessionRt>], clock: &C, max_span: u64) {
    // The epoch check, every tick (also resets the flow's buffer).
    let po = f.flow.tick(clock);
    let pos = po.position;
    let cursor = match f.cursor {
        Some(c) if !po.discontinuity && c <= pos => c,
        _ => {
            f.cursor = Some(pos);
            return;
        }
    };
    let mut p = if pos - cursor > max_span {
        pos - max_span
    } else {
        cursor
    };
    while p < pos {
        let n = usize::try_from(pos - p)
            .unwrap_or(usize::MAX)
            .min(f.scratch.len());
        if n == 0 {
            break;
        }
        for (c, &dev_ch) in f.chan.iter().enumerate() {
            let Some(out) = f.scratch.get_mut(..n) else {
                break;
            };
            f.flow.read(clock, c, p, out);
            for s in sessions.iter_mut() {
                if s.device != f.device || dev_ch < s.first || dev_ch >= s.first + s.count {
                    continue;
                }
                let SessionEnd::Producer(prod) = &mut s.end else {
                    continue;
                };
                let plane = dev_ch - s.first;
                match s.format {
                    SampleFormat::F32 => {
                        if let Some(tmp) = f.scratch_f.get_mut(..n) {
                            for (d, v) in tmp.iter_mut().zip(out.iter()) {
                                *d = i32_to_f32(*v);
                            }
                            let _ = prod.write::<f32>(plane, p, tmp);
                        }
                    }
                    _ => {
                        let _ = prod.write::<i32>(plane, p, out);
                    }
                }
            }
        }
        p += n as u64;
    }
    f.cursor = Some(pos);
}
