//! The real-time side: the per-cycle processing of the RX (source) and TX
//! (sink) nodes as plain functions over slices.
//!
//! **Real-time rules (ADR 0004):** nothing in `begin`/`channel`/`end` of
//! [`RxEngine`] and [`TxEngine`] allocates, locks, makes a syscall (apart from
//! `clock_gettime` through the vDSO) or logs. New sessions and the position of
//! every graph cycle arrive through single-producer/single-consumer rings; a
//! retired session (whose drop unmaps and closes descriptors) is dropped by
//! the control thread. If the retire ring were ever full the session is leaked
//! rather than dropped here.
//!
//! # Cycle queue
//!
//! The scheduler (control thread) pushes one [`Fire`] per graph cycle it
//! triggers: start position, period, clock epoch, session generation and the
//! time it was fired. A driven `process` pops them in order: if several are
//! queued (the graph ran late) it uses the newest and counts the others as
//! skipped; if none is queued (a `process` without a trigger) it outputs
//! silence instead of re-reading an old position. A cycle is dropped when its
//! session generation or clock epoch is not the current one, or when it is too
//! old, so a position computed before a clock step can never reach a ring that
//! was reset after it.
//!
//! The engines do not know about PipeWire: the glue hands them the planar
//! `f32` slices of the graph buffers.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering, Ordering::Relaxed};

use tessera_audio_ipc::{
    ClockSnapshot, ReadReport, RingConsumer, RingProducer, SampleFormat, raw_monotonic_ns,
};
use tessera_media::{Quality, ResampleError};

use crate::config::default_quality;
use crate::follow::{RxFollower, TxFollower};
use crate::session::{RingIo, RtSession};
use crate::timing::Fire;

/// A cycle fired longer ago than this is not processed (nanoseconds).
pub const MAX_CYCLE_AGE_NS: u64 = 250_000_000;

/// Counters published by the real-time side (relaxed atomics; read by the
/// control thread for logging).
#[derive(Debug, Default)]
pub struct RtStats {
    /// Graph cycles seen.
    pub cycles: AtomicU64,
    /// Cycles run as the graph driver (position from the scheduler).
    pub driven: AtomicU64,
    /// Cycles run as a follower of another driver.
    pub followed: AtomicU64,
    /// Cycles without a session (silence in, nothing out).
    pub silent: AtomicU64,
    /// Driven `process` calls with no fired cycle queued (silence).
    pub no_cycle: AtomicU64,
    /// Queued cycles skipped because a newer one was queued.
    pub skipped_cycles: AtomicU64,
    /// Cycles dropped as stale (old session, old clock epoch, too old).
    pub stale_cycles: AtomicU64,
    /// RX frames that were valid audio.
    pub rx_valid: AtomicU64,
    /// RX frames that were silence because they were not written (yet).
    pub rx_unwritten: AtomicU64,
    /// RX frames lost inside the window.
    pub rx_holes: AtomicU64,
    /// RX frames that were already overwritten.
    pub rx_overrun: AtomicU64,
    /// TX frames the ring discarded as too old.
    pub tx_late: AtomicU64,
    /// Re-anchors of the position (slips).
    pub slips: AtomicU64,
    /// Sessions attached.
    pub attaches: AtomicU64,
    /// Ring resets after a clock epoch change (TX).
    pub resets: AtomicU64,
    /// Requested periods above the supported maximum (clamped).
    pub clamped: AtomicU64,
    /// Failed clock overlay reads (the last good snapshot was used).
    pub clock_misses: AtomicU64,
    /// Follower mode: the last fill error in frames (positive = behind).
    pub follow_err: AtomicI64,
    /// Follower mode: the last resampling ratio trim in parts per million.
    pub follow_ppm: AtomicI64,
    /// Follower mode: the largest absolute fill error seen, in frames.
    pub follow_err_max: AtomicU64,
}

/// State shared between the control thread and one node's real-time callback.
#[derive(Debug, Default)]
pub struct RtShared {
    /// The graph period seen in the last cycle (frames).
    pub period_hint: AtomicU32,
    /// Start of the last driven cycle the real-time side completed.
    pub done_start: AtomicU64,
    /// End (start + frames actually served) of that cycle. The scheduler
    /// continues from here, so a graph period change never repeats or skips
    /// media positions.
    pub done_end: AtomicU64,
    /// Counters.
    pub stats: RtStats,
}

impl RtShared {
    /// Called by the real-time side after a driven cycle.
    pub fn publish_done(&self, start: u64, end: u64) {
        self.done_end.store(end, Ordering::Relaxed);
        self.done_start.store(start, Ordering::Release);
    }

    /// The last completed driven cycle as `(start, end)`.
    pub fn last_done(&self) -> (u64, u64) {
        let start = self.done_start.load(Ordering::Acquire);
        (start, self.done_end.load(Ordering::Relaxed))
    }
}

/// A command from the control thread.
#[derive(Debug)]
pub enum RtCmd {
    /// Start using this session. `epoch_gen` identifies it in cycles.
    Attach {
        /// Generation number.
        epoch_gen: u32,
        /// The session.
        session: Box<RtSession>,
    },
    /// Drop the current session (silence from now on).
    Detach,
}

/// Capacity of the command and retire rings.
pub const CMD_RING: usize = 8;
/// Capacity of the cycle queue.
pub const CYCLE_RING: usize = 8;

/// The control thread's end of a node's command channel.
#[derive(Debug)]
pub struct RtControl {
    cmd_tx: rtrb::Producer<RtCmd>,
    retired_rx: rtrb::Consumer<Box<RtSession>>,
    cycle_tx: rtrb::Producer<Fire>,
    /// Shared state (also held by the engine).
    pub shared: Arc<RtShared>,
    epoch_gen: u32,
}

/// The real-time end.
#[derive(Debug)]
pub struct RtPort {
    cmd_rx: rtrb::Consumer<RtCmd>,
    retired_tx: rtrb::Producer<Box<RtSession>>,
    cycle_rx: rtrb::Consumer<Fire>,
    shared: Arc<RtShared>,
}

/// Creates the two ends of a command channel.
pub fn rt_channel() -> (RtControl, RtPort) {
    let (cmd_tx, cmd_rx) = rtrb::RingBuffer::new(CMD_RING);
    let (retired_tx, retired_rx) = rtrb::RingBuffer::new(CMD_RING);
    let (cycle_tx, cycle_rx) = rtrb::RingBuffer::new(CYCLE_RING);
    let shared = Arc::new(RtShared::default());
    (
        RtControl {
            cmd_tx,
            retired_rx,
            cycle_tx,
            shared: Arc::clone(&shared),
            epoch_gen: 0,
        },
        RtPort {
            cmd_rx,
            retired_tx,
            cycle_rx,
            shared,
        },
    )
}

impl RtControl {
    /// Hands a session to the real-time side; returns its generation.
    /// On failure (command ring full) the session is returned.
    pub fn attach(&mut self, session: Box<RtSession>) -> Result<u32, Box<RtSession>> {
        let epoch_gen = self.epoch_gen.wrapping_add(1);
        match self.cmd_tx.push(RtCmd::Attach { epoch_gen, session }) {
            Ok(()) => {
                self.epoch_gen = epoch_gen;
                Ok(epoch_gen)
            }
            Err(rtrb::PushError::Full(RtCmd::Attach { session, .. })) => Err(session),
            Err(rtrb::PushError::Full(RtCmd::Detach)) => unreachable!("pushed an Attach"),
        }
    }

    /// Asks the real-time side to drop its session. `false` if the command
    /// ring is full (retry later).
    pub fn detach(&mut self) -> bool {
        self.cmd_tx.push(RtCmd::Detach).is_ok()
    }

    /// Queues the position of a cycle that is about to be triggered. `false`
    /// if the queue is full (the graph is not consuming; the cycle is lost).
    pub fn push_cycle(&mut self, fire: Fire) -> bool {
        self.cycle_tx.push(fire).is_ok()
    }

    /// The generation of the most recent attach.
    pub fn generation(&self) -> u32 {
        self.epoch_gen
    }

    /// Drops sessions the real-time side has retired (unmaps their memory).
    pub fn reap(&mut self) -> usize {
        let mut n = 0;
        while self.retired_rx.pop().is_ok() {
            n += 1;
        }
        n
    }
}

/// Sizes and policies of an engine, fixed at construction.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Channels of the node.
    pub channels: usize,
    /// Sample rate.
    pub rate: u32,
    /// Largest period handled (frames); larger periods are clamped.
    pub max_period: usize,
    /// RX: extra read offset behind `now - latency`.
    pub rx_offset: u64,
    /// TX: periods of lead.
    pub tx_ahead_periods: u32,
    /// TX: extra lead in frames.
    pub tx_ahead_margin: u32,
    /// Follower resampler quality.
    pub resampler: Quality,
    /// Follower resampler chunk (frames).
    pub chunk: usize,
}

impl EngineConfig {
    /// Defaults matching [`crate::Config`] for a node of `channels` at `rate`.
    pub fn new(channels: usize, rate: u32) -> Self {
        Self {
            channels,
            rate,
            max_period: 2048,
            rx_offset: 0,
            tx_ahead_periods: 1,
            tx_ahead_margin: 64,
            resampler: default_quality(),
            chunk: 64,
        }
    }

    fn tx_ahead(&self, frames: usize) -> u64 {
        frames as u64 * u64::from(self.tx_ahead_periods) + u64::from(self.tx_ahead_margin)
    }
}

#[inline]
fn i32_to_f32(x: i32) -> f32 {
    x as f32 * (1.0 / 2_147_483_648.0)
}

#[inline]
fn f32_to_i32(x: f32) -> i32 {
    // `as` saturates and maps NaN to 0.
    (x * 2_147_483_648.0) as i32
}

fn add_report(stats: &RtStats, r: &ReadReport) {
    stats.rx_valid.fetch_add(r.valid as u64, Relaxed);
    stats.rx_unwritten.fetch_add(r.unwritten as u64, Relaxed);
    stats.rx_holes.fetch_add(r.hole as u64, Relaxed);
    stats.rx_overrun.fetch_add(r.overrun as u64, Relaxed);
}

/// Reads ring frames `[pos, pos + out.len())` of `ch` as `f32`.
fn read_ring(
    c: &mut RingConsumer,
    fmt: SampleFormat,
    scratch: &mut [i32],
    ch: u32,
    pos: u64,
    out: &mut [f32],
) -> ReadReport {
    let mut total = ReadReport::default();
    match fmt {
        SampleFormat::F32 => {
            if let Ok(r) = c.read::<f32>(ch, pos, out) {
                total = r;
            } else {
                out.fill(0.0);
            }
        }
        SampleFormat::I32 => {
            let step = scratch.len().max(1);
            let mut done = 0usize;
            while done < out.len() {
                let n = step.min(out.len() - done);
                let (Some(tmp), Some(dst)) = (scratch.get_mut(..n), out.get_mut(done..done + n))
                else {
                    break;
                };
                match c.read::<i32>(ch, pos + done as u64, tmp) {
                    Ok(r) => {
                        total.valid += r.valid;
                        total.unwritten += r.unwritten;
                        total.hole += r.hole;
                        total.overrun += r.overrun;
                        total.epoch = r.epoch;
                        total.epoch_changed |= r.epoch_changed;
                    }
                    Err(_) => tmp.fill(0),
                }
                for (d, s) in dst.iter_mut().zip(tmp.iter()) {
                    *d = i32_to_f32(*s);
                }
                done += n;
            }
            if done < out.len() {
                out[done..].fill(0.0);
            }
        }
    }
    total
}

/// Writes `data` at `pos` of `ch`; returns the frames the ring discarded as
/// too old.
fn write_ring(
    p: &mut RingProducer,
    fmt: SampleFormat,
    scratch: &mut [i32],
    ch: u32,
    pos: u64,
    data: &[f32],
) -> usize {
    let mut late = 0;
    match fmt {
        SampleFormat::F32 => {
            if let Ok(r) = p.write::<f32>(ch, pos, data) {
                late = r.late;
            }
        }
        SampleFormat::I32 => {
            let step = scratch.len().max(1);
            let mut done = 0usize;
            while done < data.len() {
                let n = step.min(data.len() - done);
                let (Some(tmp), Some(src)) = (scratch.get_mut(..n), data.get(done..done + n))
                else {
                    break;
                };
                for (d, s) in tmp.iter_mut().zip(src.iter()) {
                    *d = f32_to_i32(*s);
                }
                if let Ok(r) = p.write::<i32>(ch, pos + done as u64, tmp) {
                    late += r.late;
                }
                done += n;
            }
        }
    }
    late
}

/// Reads the clock overlay. A torn or stuck read keeps the last good
/// snapshot (the affine map stays usable for extrapolation) and is counted;
/// `None` only before the first good read.
fn clock_now(s: &mut RtSession, stats: &RtStats) -> Option<(u64, u64)> {
    match s.overlay.try_read() {
        Some(snap) => s.last_clock = Some(snap),
        None => {
            stats.clock_misses.fetch_add(1, Relaxed);
        }
    }
    let snap: &ClockSnapshot = s.last_clock.as_ref()?;
    Some((
        snap.media_samples_at(raw_monotonic_ns(), s.rate),
        snap.epoch,
    ))
}

#[derive(Debug, Clone, Copy)]
enum Plan {
    Silence,
    Driven { start: u64 },
    Follow,
}

fn drain_cmds(
    port: &mut RtPort,
    session: &mut Option<Box<RtSession>>,
    epoch_gen: &mut u32,
    on_change: &mut bool,
) {
    while let Ok(cmd) = port.cmd_rx.pop() {
        let old = match cmd {
            RtCmd::Attach {
                epoch_gen: g,
                session: s,
            } => {
                *epoch_gen = g;
                port.shared.stats.attaches.fetch_add(1, Relaxed);
                session.replace(s)
            }
            RtCmd::Detach => session.take(),
        };
        *on_change = true;
        if let Some(old) = old {
            if let Err(rtrb::PushError::Full(old)) = port.retired_tx.push(old) {
                // Never drop (munmap/close) on the real-time thread.
                std::mem::forget(old);
            }
        }
    }
}

/// Pops the cycle for this `process`: the newest queued one.
fn take_cycle(cycle_rx: &mut rtrb::Consumer<Fire>, stats: &RtStats) -> Option<Fire> {
    let mut newest = None;
    let mut n = 0u64;
    while let Ok(c) = cycle_rx.pop() {
        newest = Some(c);
        n += 1;
    }
    if n > 1 {
        stats.skipped_cycles.fetch_add(n - 1, Relaxed);
    }
    newest
}

/// Checks a popped cycle against the session generation, the clock epoch and
/// its age.
fn cycle_is_current(c: &Fire, epoch_gen: u32, clock_epoch: Option<u64>) -> bool {
    c.generation == epoch_gen
        && clock_epoch == Some(c.epoch)
        && raw_monotonic_ns().saturating_sub(c.mono_ns) <= MAX_CYCLE_AGE_NS
}

/// The RX (network to graph, PipeWire `Audio/Source`) engine.
#[derive(Debug)]
pub struct RxEngine {
    cfg: EngineConfig,
    port: RtPort,
    session: Option<Box<RtSession>>,
    epoch_gen: u32,
    scratch: Vec<i32>,
    follow: RxFollower,
    anchored: bool,
    last_epoch: u64,
    /// Lead behind the valid horizon fixed at the last anchor (frames).
    rx_lead: u64,
    produced: bool,
    plan: Plan,
    frames: usize,
}

impl RxEngine {
    /// Allocates all buffers (setup path).
    ///
    /// # Errors
    /// The follower resampler cannot be built for this configuration.
    pub fn new(cfg: EngineConfig, port: RtPort) -> Result<Self, ResampleError> {
        let follow = RxFollower::new(
            cfg.resampler,
            cfg.channels,
            cfg.rate,
            cfg.chunk,
            cfg.max_period,
        )?;
        Ok(Self {
            scratch: vec![0; cfg.max_period],
            follow,
            cfg,
            port,
            session: None,
            epoch_gen: 0,
            anchored: false,
            last_epoch: 0,
            rx_lead: 0,
            produced: false,
            plan: Plan::Silence,
            frames: 0,
        })
    }

    /// Frames served in the current cycle (the request, clamped).
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Starts a cycle of `requested` frames. `driving` tells whether this node
    /// is the graph driver (position from the cycle queue) or a follower
    /// (position from the clock, with the resampler). Returns the frames to
    /// fill per channel.
    pub fn begin(&mut self, driving: bool, requested: usize) -> usize {
        let mut changed = false;
        drain_cmds(
            &mut self.port,
            &mut self.session,
            &mut self.epoch_gen,
            &mut changed,
        );
        if changed {
            self.anchored = false;
        }
        self.produced = false;
        let stats = &self.port.shared.stats;
        stats.cycles.fetch_add(1, Relaxed);
        if requested > self.cfg.max_period {
            stats.clamped.fetch_add(1, Relaxed);
        }
        let frames = requested.min(self.cfg.max_period);
        self.frames = frames;
        self.port
            .shared
            .period_hint
            .store(requested.min(u32::MAX as usize) as u32, Relaxed);
        let cycle = if driving {
            take_cycle(&mut self.port.cycle_rx, stats)
        } else {
            // Positions fired while we drove are void once we follow.
            while self.port.cycle_rx.pop().is_ok() {}
            None
        };
        self.plan = match self.session.as_deref_mut() {
            None => {
                stats.silent.fetch_add(1, Relaxed);
                Plan::Silence
            }
            Some(s) if driving => {
                let epoch = clock_now(s, stats).map(|c| c.1);
                match cycle {
                    None => {
                        stats.no_cycle.fetch_add(1, Relaxed);
                        Plan::Silence
                    }
                    Some(c) if cycle_is_current(&c, self.epoch_gen, epoch) => {
                        stats.driven.fetch_add(1, Relaxed);
                        Plan::Driven { start: c.start }
                    }
                    Some(_) => {
                        stats.stale_cycles.fetch_add(1, Relaxed);
                        Plan::Silence
                    }
                }
            }
            Some(s) => {
                stats.followed.fetch_add(1, Relaxed);
                let latency = s.ring.effective_latency();
                match clock_now(s, stats) {
                    Some((now, epoch)) => {
                        Self::follow_control(
                            &self.cfg,
                            &mut self.follow,
                            &mut self.anchored,
                            &mut self.last_epoch,
                            &mut self.rx_lead,
                            stats,
                            now,
                            epoch,
                            latency,
                            frames,
                        );
                        Plan::Follow
                    }
                    None => Plan::Silence,
                }
            }
        };
        frames
    }

    #[allow(clippy::too_many_arguments)]
    fn follow_control(
        cfg: &EngineConfig,
        follow: &mut RxFollower,
        anchored: &mut bool,
        last_epoch: &mut u64,
        lead: &mut u64,
        stats: &RtStats,
        now: u64,
        epoch: u64,
        latency: u64,
        frames: usize,
    ) {
        let guard = (2 * cfg.chunk).max(64) as u64;
        let target_end = now.saturating_sub(latency).saturating_sub(cfg.rx_offset);
        // The lead behind the valid horizon is fixed when anchoring: a graph
        // period change must not shift the setpoint (the loop would slew for
        // seconds). Only a period that needs more than the stored lead
        // re-anchors, once.
        let need = frames as u64 + cfg.chunk as u64 + guard;
        let grow = *anchored && need > *lead;
        if !*anchored || grow {
            *lead = need;
        }
        let setpoint = target_end.saturating_sub(*lead);
        let err = setpoint as i64 - follow.head() as i64;
        let slip_limit = 8 * frames as i64 + 1024;
        if !*anchored || grow || epoch != *last_epoch || err.abs() > slip_limit {
            if *anchored {
                stats.slips.fetch_add(1, Relaxed);
            }
            follow.anchor(setpoint);
            *anchored = true;
            *last_epoch = epoch;
            return;
        }
        let dt = frames as f64 / f64::from(cfg.rate.max(1));
        let ratio = follow.control(err as f64, dt);
        stats.follow_err.store(err, Relaxed);
        stats
            .follow_ppm
            .store(((ratio - 1.0) * 1e6) as i64, Relaxed);
        stats.follow_err_max.fetch_max(err.unsigned_abs(), Relaxed);
    }

    /// Fills channel `ch`; `out` must be `frames()` long (shorter is fine).
    pub fn channel(&mut self, ch: usize, out: &mut [f32]) {
        let n = out.len().min(self.frames);
        let (head, tail) = out.split_at_mut(n);
        tail.fill(0.0);
        let stats = &self.port.shared.stats;
        match (self.plan, self.session.as_deref_mut()) {
            (Plan::Driven { start }, Some(s)) => {
                let fmt = s.format;
                if let RingIo::Rx(c) = &mut s.io {
                    let r = read_ring(c, fmt, &mut self.scratch, ch as u32, start, head);
                    if ch == 0 {
                        add_report(stats, &r);
                        c.publish_read_pos(start.saturating_add(n as u64));
                    }
                } else {
                    head.fill(0.0);
                }
            }
            (Plan::Follow, Some(s)) => {
                let fmt = s.format;
                if let RingIo::Rx(c) = &mut s.io {
                    if !self.produced {
                        // The resampler works on all channels at once: the
                        // first channel asked for triggers the cycle's work.
                        self.produced = true;
                        let scratch = &mut self.scratch;
                        let mut sum = ReadReport::default();
                        self.follow.produce(n, |chan, pos, buf| {
                            let r = read_ring(c, fmt, scratch, chan as u32, pos, buf);
                            if chan == 0 {
                                sum.valid += r.valid;
                                sum.unwritten += r.unwritten;
                                sum.hole += r.hole;
                                sum.overrun += r.overrun;
                            }
                        });
                        add_report(stats, &sum);
                    }
                    let plane = self.follow.plane(ch);
                    let take = plane.len().min(n);
                    if let (Some(d), Some(sl)) = (head.get_mut(..take), plane.get(..take)) {
                        d.copy_from_slice(sl);
                    }
                    if let Some(rest) = head.get_mut(take..) {
                        rest.fill(0.0);
                    }
                } else {
                    head.fill(0.0);
                }
            }
            _ => head.fill(0.0),
        }
    }

    /// Ends the cycle.
    pub fn end(&mut self) {
        match self.plan {
            Plan::Driven { start } => {
                let end = start.saturating_add(self.frames as u64);
                self.port.shared.publish_done(start, end);
            }
            Plan::Follow if self.produced => self.follow.consume(self.frames),
            _ => {}
        }
        self.plan = Plan::Silence;
    }

    /// The shared counters.
    pub fn stats(&self) -> &RtStats {
        &self.port.shared.stats
    }
}

/// The TX (graph to network, PipeWire `Audio/Sink`) engine.
#[derive(Debug)]
pub struct TxEngine {
    cfg: EngineConfig,
    port: RtPort,
    session: Option<Box<RtSession>>,
    epoch_gen: u32,
    scratch: Vec<i32>,
    follow: TxFollower,
    anchored: bool,
    last_epoch: u64,
    /// Overlay epoch seen when the driven path last ran (reset detection).
    driven_epoch: Option<u64>,
    /// TX lead fixed at the last follower anchor (frames).
    tx_lead: u64,
    plan: Plan,
    frames: usize,
    ahead: u64,
    /// Where the driven path writes next (contiguous across graph period
    /// changes), and where the current cycle writes.
    w_pos: Option<u64>,
    write_at: u64,
}

impl TxEngine {
    /// Allocates all buffers (setup path).
    ///
    /// # Errors
    /// The follower resampler cannot be built for this configuration.
    pub fn new(cfg: EngineConfig, port: RtPort) -> Result<Self, ResampleError> {
        let follow = TxFollower::new(
            cfg.resampler,
            cfg.channels,
            cfg.rate,
            cfg.chunk,
            cfg.max_period,
        )?;
        Ok(Self {
            scratch: vec![0; cfg.max_period.max(cfg.chunk * 8)],
            follow,
            cfg,
            port,
            session: None,
            epoch_gen: 0,
            anchored: false,
            last_epoch: 0,
            driven_epoch: None,
            tx_lead: 0,
            plan: Plan::Silence,
            frames: 0,
            ahead: 0,
            w_pos: None,
            write_at: 0,
        })
    }

    /// Frames accepted in the current cycle.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Starts a cycle in which the graph delivers `requested` frames per
    /// channel. See [`RxEngine::begin`].
    pub fn begin(&mut self, driving: bool, requested: usize) -> usize {
        let mut changed = false;
        drain_cmds(
            &mut self.port,
            &mut self.session,
            &mut self.epoch_gen,
            &mut changed,
        );
        if changed {
            self.anchored = false;
            self.driven_epoch = None;
            self.w_pos = None;
        }
        let stats = &self.port.shared.stats;
        stats.cycles.fetch_add(1, Relaxed);
        if requested > self.cfg.max_period {
            stats.clamped.fetch_add(1, Relaxed);
        }
        let frames = requested.min(self.cfg.max_period);
        self.frames = frames;
        self.port
            .shared
            .period_hint
            .store(requested.min(u32::MAX as usize) as u32, Relaxed);
        let ahead = self.cfg.tx_ahead(frames);
        self.ahead = ahead;
        let cycle = if driving {
            take_cycle(&mut self.port.cycle_rx, stats)
        } else {
            while self.port.cycle_rx.pop().is_ok() {}
            None
        };
        self.plan = match self.session.as_deref_mut() {
            None => {
                stats.silent.fetch_add(1, Relaxed);
                Plan::Silence
            }
            Some(s) if driving => {
                let epoch = clock_now(s, stats).map(|c| c.1);
                // The producer owns the ring epoch: follow the clock's, and
                // reset *before* any position is written, whatever cycle is
                // current. A cycle computed under the old epoch is dropped
                // below, so it can never raise the ring's high-water mark.
                if let (Some(e), RingIo::Tx(p)) = (epoch, &mut s.io) {
                    if self.driven_epoch.is_some_and(|d| d != e) {
                        p.reset();
                        stats.resets.fetch_add(1, Relaxed);
                        self.w_pos = None;
                    }
                    self.driven_epoch = Some(e);
                }
                match cycle {
                    None => {
                        stats.no_cycle.fetch_add(1, Relaxed);
                        Plan::Silence
                    }
                    Some(c) if cycle_is_current(&c, self.epoch_gen, epoch) => {
                        stats.driven.fetch_add(1, Relaxed);
                        let start = c.start;
                        // Keep writing where the last cycle stopped while that
                        // is still a sane lead (a graph period change moves
                        // the nominal lead but must not overlap or skip
                        // positions); otherwise anchor at the nominal lead.
                        let min_lead = frames as u64 + u64::from(self.cfg.tx_ahead_margin);
                        let max_lead = ahead + self.cfg.max_period as u64;
                        self.write_at = match self.w_pos {
                            Some(w)
                                if w >= start.saturating_add(min_lead)
                                    && w <= start.saturating_add(max_lead) =>
                            {
                                w
                            }
                            Some(_) => {
                                stats.slips.fetch_add(1, Relaxed);
                                start.saturating_add(ahead)
                            }
                            None => start.saturating_add(ahead),
                        };
                        Plan::Driven { start }
                    }
                    Some(_) => {
                        stats.stale_cycles.fetch_add(1, Relaxed);
                        Plan::Silence
                    }
                }
            }
            Some(s) => {
                stats.followed.fetch_add(1, Relaxed);
                match clock_now(s, stats) {
                    Some((now, epoch)) => {
                        let reset = Self::follow_control(
                            &self.cfg,
                            &mut self.follow,
                            &mut self.anchored,
                            &mut self.last_epoch,
                            &mut self.tx_lead,
                            stats,
                            now,
                            epoch,
                            ahead,
                            frames,
                        );
                        if reset {
                            if let RingIo::Tx(p) = &mut s.io {
                                p.reset();
                                stats.resets.fetch_add(1, Relaxed);
                            }
                        }
                        self.follow.begin(frames);
                        Plan::Follow
                    }
                    None => Plan::Silence,
                }
            }
        };
        frames
    }

    /// Returns true when the ring should be reset (clock epoch change).
    #[allow(clippy::too_many_arguments)]
    fn follow_control(
        cfg: &EngineConfig,
        follow: &mut TxFollower,
        anchored: &mut bool,
        last_epoch: &mut u64,
        lead: &mut u64,
        stats: &RtStats,
        now: u64,
        epoch: u64,
        ahead: u64,
        frames: usize,
    ) -> bool {
        // The lead is fixed at the anchor; a smaller period keeps it, a
        // period that needs more (period + margin) re-anchors once.
        let min_lead = frames as u64 + u64::from(cfg.tx_ahead_margin);
        let grow = *anchored && *lead < min_lead;
        if !*anchored || grow {
            *lead = ahead;
        }
        let ahead = *lead;
        let err = follow.w_pos as i64 - now as i64 - ahead as i64;
        let slip_limit = 8 * frames as i64 + 1024;
        let epoch_changed = *anchored && epoch != *last_epoch;
        if !*anchored || grow || epoch_changed || err.abs() > slip_limit {
            if *anchored {
                stats.slips.fetch_add(1, Relaxed);
            }
            follow.anchor(now.saturating_add(ahead));
            *anchored = true;
            *last_epoch = epoch;
            return epoch_changed;
        }
        let dt = frames as f64 / f64::from(cfg.rate.max(1));
        let ratio = follow.control(err as f64, dt);
        stats.follow_err.store(err, Relaxed);
        stats
            .follow_ppm
            .store(((ratio - 1.0) * 1e6) as i64, Relaxed);
        stats.follow_err_max.fetch_max(err.unsigned_abs(), Relaxed);
        false
    }

    /// Takes the graph's samples of channel `ch`.
    pub fn channel(&mut self, ch: usize, input: &[f32]) {
        let n = input.len().min(self.frames);
        let Some(input) = input.get(..n) else { return };
        let stats = &self.port.shared.stats;
        match (self.plan, self.session.as_deref_mut()) {
            (Plan::Driven { .. }, Some(s)) => {
                let fmt = s.format;
                if let RingIo::Tx(p) = &mut s.io {
                    let late =
                        write_ring(p, fmt, &mut self.scratch, ch as u32, self.write_at, input);
                    if late > 0 {
                        stats.tx_late.fetch_add(late as u64, Relaxed);
                    }
                }
            }
            (Plan::Follow, Some(_)) => self.follow.push(ch, input),
            _ => {}
        }
    }

    /// Ends the cycle.
    pub fn end(&mut self) {
        match (self.plan, self.session.as_deref_mut()) {
            (Plan::Driven { start }, _) => {
                let end = start.saturating_add(self.frames as u64);
                self.port.shared.publish_done(start, end);
                self.w_pos = Some(self.write_at.saturating_add(self.frames as u64));
            }
            (Plan::Follow, Some(s)) => {
                let fmt = s.format;
                if let RingIo::Tx(p) = &mut s.io {
                    let scratch = &mut self.scratch;
                    let stats = &self.port.shared.stats;
                    let mut late = 0usize;
                    self.follow.commit(self.frames, |ch, pos, data| {
                        late += write_ring(p, fmt, scratch, ch as u32, pos, data);
                    });
                    if late > 0 {
                        stats.tx_late.fetch_add(late as u64, Relaxed);
                    }
                }
            }
            _ => {}
        }
        self.plan = Plan::Silence;
    }

    /// The shared counters.
    pub fn stats(&self) -> &RtStats {
        &self.port.shared.stats
    }
}

/// Plain-function form of one RX cycle over planar slices (what the PipeWire
/// callback does, minus the buffer handling). Returns the frames served.
pub fn process_rx(eng: &mut RxEngine, driving: bool, planes: &mut [&mut [f32]]) -> usize {
    let requested = planes.first().map_or(0, |p| p.len());
    let frames = eng.begin(driving, requested);
    for (ch, plane) in planes.iter_mut().enumerate() {
        eng.channel(ch, plane);
    }
    eng.end();
    frames
}

/// Plain-function form of one TX cycle over planar slices.
pub fn process_tx(eng: &mut TxEngine, driving: bool, planes: &[&[f32]]) -> usize {
    let requested = planes.first().map_or(0, |p| p.len());
    let frames = eng.begin(driving, requested);
    for (ch, plane) in planes.iter().enumerate() {
        eng.channel(ch, plane);
    }
    eng.end();
    frames
}
