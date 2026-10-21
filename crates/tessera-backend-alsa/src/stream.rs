//! The PCM stream core: timeline, hardware pointer, xrun rules and the copy
//! between ALSA buffers and the audio-ipc ring.
//!
//! It is plain Rust over an injected [`Clock`] and the ring role handles, so
//! the xrun, drain, epoch and no-allocation properties are tested without
//! ALSA. The ioplug glue (`plugin.rs`) only translates callbacks into calls on
//! [`PcmStream`].
//!
//! # Timeline
//!
//! A *stream frame* `s` counts from 0 at `start`. Hardware time is the media
//! clock: no resampling (ADR 0006/0007), the PCM's clock *is* the media clock.
//!
//! * **Playback (ring `Tx`, the plugin produces).** Frame `s` is written to
//!   ring position `anchor + s` with `anchor = media_now + period + latency`
//!   at `start` (the daemon plays position `p` when the media clock reaches
//!   `p`, ADR 0006; this is the PipeWire client's "one period plus margin
//!   ahead"). The hardware pointer is `h = media_now - anchor` (0 until the
//!   clock reaches the anchor).
//! * **Capture (ring `Rx`, the plugin consumes).** Frame `s` is read from ring
//!   position `anchor + s` with
//!   `anchor = media_now - effective_latency - period - latency`; the daemon
//!   makes position `p` available at media time `p + effective_latency`, so
//!   `h = media_now - media_at_start` frames have arrived.
//!
//! # Xrun rules
//!
//! * Playback underrun: the hardware pointer passed what the application
//!   wrote (`h > appl`, more than `stop_threshold - buffer_size` beyond
//!   `appl` when the application raised the threshold; never when
//!   `stop_threshold >= boundary`). Stale audio is never replayed (ADR 0006);
//!   unwritten ring positions read as silence in the daemon.
//! * Capture overrun: `h - appl >= stop_threshold`.
//! * A clock epoch change (media time stepped) ends the stream with an xrun;
//!   recovery re-anchors at `prepare`/`start`.
//! * A clock overlay silent for `stale_ns` means a hung or dead daemon:
//!   [`Pointer::Lost`].

use tessera_audio_ipc::{RingConsumer, RingProducer};

use crate::clock::{Clock, Now};
use crate::convert::{Area, PcmFormat, RingSample};
use crate::error::Error;
use crate::settings::Dir;

/// Frames converted per ring access; the scratch buffer is this long.
pub const CHUNK: usize = 1024;

/// The ring role the stream plays.
#[derive(Debug)]
pub enum Role {
    /// Playback: the plugin writes the TX ring.
    Tx(RingProducer),
    /// Capture: the plugin reads the RX ring.
    Rx(RingConsumer),
}

/// Immutable stream parameters.
#[derive(Debug, Clone, Copy)]
pub struct StreamConfig {
    /// Direction.
    pub dir: Dir,
    /// The application's sample format.
    pub format: PcmFormat,
    /// Channels.
    pub channels: usize,
    /// Buffer size in frames.
    pub buffer_size: u64,
    /// Period size in frames.
    pub period_size: u64,
    /// Extra frames of margin beyond one period (the `latency` option).
    pub margin: u64,
    /// Overlay silence that counts as a dead daemon, ns; 0 disables.
    pub stale_ns: u64,
    /// The daemon's engine tick (wake-up interval) in frames, as measured by
    /// the plugin; 0 when unknown (the period stands in).
    pub tick_frames: u64,
    /// The ioplug cannot represent a hardware pointer that moved a whole
    /// buffer or more between two reads (alsa-lib before the
    /// `BOUNDARY_WA` flag): treat that as an xrun.
    pub alias_guard: bool,
}

/// What `pointer` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pointer {
    /// The hardware position in frames since `start` (not wrapped).
    Pos(u64),
    /// The stream must stop with an xrun.
    Xrun,
    /// The daemon is gone.
    Lost,
}

/// Frames available to the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvailInfo {
    /// Playback: free space; capture: data ready.
    pub frames: u64,
    /// An xrun is pending (the next `pointer` reports it).
    pub xrun: bool,
    /// The daemon is gone.
    pub lost: bool,
}

/// When the application is next due (see [`PcmStream::wake`]): what the poll
/// descriptor must say, the way a kernel driver's does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// Ready now: `avail_min` frames can be transferred, an xrun is pending,
    /// the daemon is gone, or the drain is over.
    Now,
    /// Ready once the media clock has advanced this many more frames.
    After(u64),
    /// Nothing changes by itself (capture not started, a full playback
    /// buffer not started, paused): only the application can make it ready.
    Never,
}

/// Why a stream operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamError {
    /// Underrun, overrun or clock step.
    Xrun,
    /// The daemon is gone.
    Lost,
    /// The ring, the clock or the buffer was unusable.
    Fault(&'static str),
}

impl StreamError {
    /// The positive errno an ALSA callback returns, negated.
    pub fn errno(self) -> i32 {
        match self {
            StreamError::Xrun => libc::EPIPE,
            StreamError::Lost => libc::ENODEV,
            StreamError::Fault(_) => libc::EIO,
        }
    }
}

/// Counters (diagnostics).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamStats {
    /// Xruns reported.
    pub xruns: u64,
    /// Playback frames the ring discarded as too old.
    pub late_frames: u64,
    /// Capture frames that were not written (yet) when read.
    pub unwritten_frames: u64,
    /// Capture frames lost inside the ring window.
    pub hole_frames: u64,
    /// Capture frames already overwritten when read.
    pub overrun_frames: u64,
}

/// The operations the ioplug glue needs, object-safe.
pub trait PcmStream: Send {
    /// Resets the stream to the state after `hw_params` (ioplug zeroes its
    /// application pointer at the same time).
    ///
    /// # Errors
    /// [`StreamError::Lost`] if the daemon is gone.
    fn prepare(&mut self) -> Result<(), StreamError>;
    /// Starts the stream; `appl_raw` is the application pointer.
    ///
    /// # Errors
    /// [`StreamError::Lost`] without a usable clock, or a ring fault.
    fn start(&mut self, appl_raw: u64) -> Result<(), StreamError>;
    /// Stops the stream, discarding playback data that was not played yet.
    fn stop(&mut self);
    /// The hardware pointer. `draining` clamps it to the application pointer
    /// and suppresses the underrun.
    fn pointer(&mut self, appl_raw: u64, draining: bool) -> Pointer;
    /// Frames available to the application, without side effects on the
    /// stream state.
    fn avail(&mut self, appl_raw: u64) -> AvailInfo;
    /// When the application, which wants `avail_min` frames per wake-up, is
    /// next due; `draining` (the ioplug state) waits for the end of the drain
    /// instead: playback until the last frame was played, capture until the
    /// buffer is full (where alsa-lib ends a capture drain). Same observation
    /// as [`PcmStream::avail`], no side effects.
    fn wake(&mut self, appl_raw: u64, avail_min: u64, draining: bool) -> Wake;
    /// Copies `size` frames at `offset` of `areas` (one per channel):
    /// playback reads the application's areas into the ring, capture fills
    /// them from the ring.
    ///
    /// # Errors
    /// [`StreamError`] as for `pointer`.
    fn transfer(
        &mut self,
        appl_raw: u64,
        areas: &mut [Area<'_>],
        offset: usize,
        size: usize,
    ) -> Result<usize, StreamError>;
    /// Sets the daemon's engine tick in frames (see `StreamConfig::tick_frames`);
    /// takes effect at the next `start`.
    fn set_tick(&mut self, frames: u64);
    /// Frames between the application and the wire: playback, the frames
    /// queued plus the start lead; capture, the frames waiting plus the lag of
    /// the newest readable audio behind the clock (effective latency
    /// included).
    ///
    /// # Errors
    /// [`StreamError::Xrun`] or [`StreamError::Lost`] as for `pointer`.
    fn delay(&mut self, appl_raw: u64, draining: bool) -> Result<i64, StreamError>;
    /// Pauses (`enable`) or resumes. Pausing silences the output at once and
    /// freezes the hardware pointer; resuming re-anchors to the media clock and
    /// plays on from where the pointer stopped (queued playback frames are
    /// written again, capture continues with the audio after the resume).
    ///
    /// # Errors
    /// [`StreamError::Fault`] if the stream was not started, or the errors of
    /// `pointer`.
    fn pause(&mut self, enable: bool, appl_raw: u64) -> Result<(), StreamError>;
    /// Records the software parameters that matter: the pointer wrap-around
    /// `boundary` and the `stop_threshold`.
    fn set_sw(&mut self, boundary: u64, stop_threshold: u64);
    /// Counters.
    fn stats(&self) -> StreamStats;
}

#[derive(Debug, Clone, Copy)]
struct Run {
    /// Media sample at which the hardware pointer is 0 (the start).
    time_base: u64,
    /// Ring position of stream frame 0.
    pos_base: u64,
    /// Frames between the start and frame 0 reaching its destination:
    /// playback, how far ahead of the clock frame 0 is written; capture, how
    /// far behind the clock the frame being read was captured.
    lead: u64,
    epoch: u64,
    last_h: u64,
}

/// Unwraps the application pointer, which ioplug keeps modulo the boundary.
#[derive(Debug, Clone, Copy, Default)]
struct Appl {
    last_raw: u64,
    total: u64,
}

impl Appl {
    fn update(&mut self, raw: u64, boundary: u64) -> u64 {
        let b = boundary.max(1);
        let raw = raw % b;
        let fwd = if raw >= self.last_raw {
            raw - self.last_raw
        } else {
            b - self.last_raw + raw
        };
        if fwd <= b / 2 {
            self.total = self.total.saturating_add(fwd);
        } else {
            // A rewind by the application.
            self.total = self.total.saturating_sub(b - fwd);
        }
        self.last_raw = raw;
        self.total
    }
}

enum Seen {
    Idle { appl: u64 },
    Live { appl: u64, h: u64 },
    Lost,
    Xrun,
}

/// The stream over a ring of sample type `S` and a clock.
#[derive(Debug)]
pub struct Stream<C: Clock, S: RingSample> {
    cfg: StreamConfig,
    role: Role,
    clock: C,
    scratch: Vec<S>,
    /// Playback data written before `start`: `channels * buffer_size`.
    stage: Vec<S>,
    appl: Appl,
    boundary: u64,
    stop_threshold: u64,
    run: Option<Run>,
    /// `Some(h)` while paused: the hardware pointer is frozen at `h`.
    paused: Option<u64>,
    stats: StreamStats,
}

/// Writes the staged playback frames `[from, to)` of every channel to the ring
/// at `base + s`. `stage` is `channels` planes of `bs` frames, frame `s` at
/// slot `s % bs`.
#[allow(clippy::too_many_arguments)]
fn flush_stage<S: RingSample>(
    producer: &mut RingProducer,
    stage: &[S],
    bs: u64,
    channels: usize,
    from: u64,
    to: u64,
    base: u64,
    late: &mut u64,
) -> Result<(), StreamError> {
    let from = from.max(to.saturating_sub(bs));
    for c in 0..channels {
        let plane = c * bs as usize;
        let mut s = from;
        while s < to {
            let slot = (s % bs) as usize;
            let n = ((bs as usize - slot) as u64).min(to - s) as usize;
            let Some(data) = stage.get(plane + slot..plane + slot + n) else {
                return Err(StreamError::Fault("stage"));
            };
            match producer.write::<S>(c as u32, base.saturating_add(s), data) {
                Ok(rep) => *late += rep.late as u64,
                Err(_) => return Err(StreamError::Fault("ring write")),
            }
            s += n as u64;
        }
    }
    Ok(())
}

impl<C: Clock, S: RingSample> Stream<C, S> {
    /// Builds the stream (allocates; call it from `hw_params`).
    ///
    /// # Errors
    /// [`Error::Params`] if the parameters do not fit the ring role.
    pub fn new(cfg: StreamConfig, role: Role, clock: C) -> Result<Self, Error> {
        let (ring_channels, ring_format) = match &role {
            Role::Tx(p) => (p.params().channels, p.params().format),
            Role::Rx(c) => (c.params().channels, c.params().format),
        };
        let dir_ok = matches!(
            (&role, cfg.dir),
            (Role::Tx(_), Dir::Playback) | (Role::Rx(_), Dir::Capture)
        );
        if !dir_ok {
            return Err(Error::Params(
                "ring role does not match the direction".into(),
            ));
        }
        if ring_format != S::RING {
            return Err(Error::Params("ring sample format mismatch".into()));
        }
        if cfg.channels == 0 || cfg.channels as u64 != u64::from(ring_channels) {
            return Err(Error::Params(format!(
                "{} channels, the ring carries {ring_channels}",
                cfg.channels
            )));
        }
        if cfg.buffer_size == 0 || cfg.period_size == 0 || cfg.period_size > cfg.buffer_size {
            return Err(Error::Params("bad buffer or period size".into()));
        }
        let stage_len = if cfg.dir == Dir::Playback {
            usize::try_from(cfg.buffer_size)
                .ok()
                .and_then(|b| b.checked_mul(cfg.channels))
                .ok_or_else(|| Error::Params("buffer too large".into()))?
        } else {
            0
        };
        Ok(Self {
            cfg,
            role,
            clock,
            scratch: vec![S::default(); CHUNK],
            stage: vec![S::default(); stage_len],
            appl: Appl::default(),
            boundary: u64::MAX,
            stop_threshold: cfg.buffer_size,
            run: None,
            paused: None,
            stats: StreamStats::default(),
        })
    }

    fn stale(&self, now: &Now) -> bool {
        self.cfg.stale_ns != 0 && now.age_ns > self.cfg.stale_ns
    }

    /// Observes appl, the clock and the xrun rules without changing the run.
    fn observe(&mut self, appl_raw: u64, draining: bool) -> Seen {
        let appl = self.appl.update(appl_raw, self.boundary);
        let Some(run) = self.run else {
            return Seen::Idle { appl };
        };
        if let Some(h) = self.paused {
            return Seen::Live { appl, h };
        }
        let Some(now) = self.clock.now() else {
            return Seen::Lost;
        };
        if self.stale(&now) {
            return Seen::Lost;
        }
        if now.epoch != run.epoch {
            return Seen::Xrun;
        }
        let h = now.samples.saturating_sub(run.time_base).max(run.last_h);
        if self.cfg.alias_guard && !draining && h - run.last_h >= self.cfg.buffer_size {
            return Seen::Xrun;
        }
        let never = self.stop_threshold >= self.boundary;
        match self.cfg.dir {
            Dir::Playback => {
                if draining {
                    // A drain is over when the last frame has been *played*, which
                    // is `lead` after the pointer reaches it. The reported pointer
                    // stays monotonic (it holds until the played position passes
                    // it) and never exceeds what was written.
                    let played = h.saturating_sub(run.lead);
                    return Seen::Live {
                        appl,
                        h: played.max(run.last_h).min(appl.max(run.last_h)),
                    };
                }
                let slack = self.stop_threshold.saturating_sub(self.cfg.buffer_size);
                if !never && h > appl.saturating_add(slack) {
                    return Seen::Xrun;
                }
            }
            Dir::Capture => {
                if !never && h.saturating_sub(appl) >= self.stop_threshold {
                    return Seen::Xrun;
                }
            }
        }
        Seen::Live { appl, h }
    }

    fn fault(msg: &'static str) -> StreamError {
        StreamError::Fault(msg)
    }

    /// Writes `n` frames per channel of `src(c)` to ring positions starting at
    /// `pos` (playback).
    fn ring_write(
        producer: &mut RingProducer,
        late: &mut u64,
        c: usize,
        pos: u64,
        data: &[S],
    ) -> Result<(), StreamError> {
        match producer.write::<S>(c as u32, pos, data) {
            Ok(rep) => {
                *late += rep.late as u64;
                Ok(())
            }
            Err(_) => Err(Self::fault("ring write")),
        }
    }
}

impl<C: Clock + Send, S: RingSample + Send> PcmStream for Stream<C, S> {
    fn prepare(&mut self) -> Result<(), StreamError> {
        self.run = None;
        self.paused = None;
        self.appl = Appl::default();
        let Some(now) = self.clock.now() else {
            return Err(StreamError::Lost);
        };
        if self.stale(&now) {
            return Err(StreamError::Lost);
        }
        if let Role::Tx(p) = &mut self.role {
            // Whatever an earlier run left in the ring must not play.
            p.reset();
        }
        Ok(())
    }

    fn start(&mut self, appl_raw: u64) -> Result<(), StreamError> {
        let appl = self.appl.update(appl_raw, self.boundary);
        let Some(now) = self.clock.now() else {
            return Err(StreamError::Lost);
        };
        if self.stale(&now) {
            return Err(StreamError::Lost);
        }
        let (period, margin) = (self.cfg.period_size, self.cfg.margin);
        match &mut self.role {
            Role::Tx(producer) => {
                let base = now.samples.saturating_add(period).saturating_add(margin);
                // Flush what the application wrote before the start.
                let bs = self.cfg.buffer_size;
                let mut late = 0;
                flush_stage(
                    producer,
                    &self.stage,
                    bs,
                    self.cfg.channels,
                    0,
                    appl,
                    base,
                    &mut late,
                )?;
                self.stats.late_frames += late;
                // The pointer counts from the start: frames must be written
                // `ahead` before the clock reaches them, so an application that
                // keeps less than that queued underruns (a real, visible xrun)
                // before any write can arrive too late.
                self.run = Some(Run {
                    time_base: now.samples,
                    pos_base: base,
                    lead: period.saturating_add(margin),
                    epoch: now.epoch,
                    last_h: 0,
                });
            }
            Role::Rx(consumer) => {
                let latency = consumer.effective_latency().min(1 << 40);
                // The daemon delivers data once per engine tick, not once per
                // application period: that cadence sets how far behind the
                // newest possible audio the stream must read.
                let tick = if self.cfg.tick_frames == 0 {
                    period
                } else {
                    self.cfg.tick_frames
                };
                // Never so far behind that lag plus buffer no longer fits the
                // ring: the oldest frame would already be overwritten.
                let headroom = consumer
                    .params()
                    .capacity_frames
                    .saturating_sub(self.cfg.buffer_size.saturating_add(64));
                let lag = latency
                    .saturating_add(tick)
                    .saturating_add(margin)
                    .min(headroom);
                self.run = Some(Run {
                    time_base: now.samples,
                    pos_base: now.samples.saturating_sub(lag),
                    lead: lag,
                    epoch: now.epoch,
                    last_h: 0,
                });
            }
        }
        Ok(())
    }

    fn stop(&mut self) {
        let pending = match (self.run, self.cfg.dir) {
            (Some(run), Dir::Playback) => {
                let h = self
                    .clock
                    .now()
                    .map_or(0, |n| n.samples.saturating_sub(run.time_base));
                self.appl.total > h
            }
            _ => false,
        };
        if pending {
            if let Role::Tx(p) = &mut self.role {
                p.reset();
            }
        }
        self.run = None;
    }

    fn pointer(&mut self, appl_raw: u64, draining: bool) -> Pointer {
        match self.observe(appl_raw, draining) {
            Seen::Idle { .. } => Pointer::Pos(0),
            Seen::Live { h, .. } => {
                if let Some(run) = &mut self.run {
                    run.last_h = h;
                }
                Pointer::Pos(h)
            }
            Seen::Lost => Pointer::Lost,
            Seen::Xrun => {
                self.stats.xruns += 1;
                Pointer::Xrun
            }
        }
    }

    fn avail(&mut self, appl_raw: u64) -> AvailInfo {
        let bs = self.cfg.buffer_size;
        match self.observe(appl_raw, false) {
            Seen::Idle { appl } => AvailInfo {
                frames: match self.cfg.dir {
                    Dir::Playback => bs.saturating_sub(appl),
                    Dir::Capture => 0,
                },
                xrun: false,
                lost: false,
            },
            Seen::Live { appl, h } => AvailInfo {
                frames: match self.cfg.dir {
                    Dir::Playback => bs.saturating_sub(appl.saturating_sub(h)),
                    Dir::Capture => h.saturating_sub(appl).min(bs),
                },
                xrun: false,
                lost: false,
            },
            Seen::Xrun => AvailInfo {
                frames: 0,
                xrun: true,
                lost: false,
            },
            Seen::Lost => AvailInfo {
                frames: 0,
                xrun: false,
                lost: true,
            },
        }
    }

    fn wake(&mut self, appl_raw: u64, avail_min: u64, draining: bool) -> Wake {
        let bs = self.cfg.buffer_size.max(1);
        let dir = self.cfg.dir;
        if draining && dir == Dir::Playback && self.paused.is_none() {
            // The drain is over when the last written frame has been played,
            // `lead` after the pointer reached it.
            let appl = self.appl.update(appl_raw, self.boundary);
            let Some(run) = self.run else {
                return Wake::Now;
            };
            let Some(now) = self.clock.now() else {
                return Wake::Now;
            };
            if self.stale(&now) || now.epoch != run.epoch {
                return Wake::Now;
            }
            let h = now.samples.saturating_sub(run.time_base);
            let end = appl.saturating_add(run.lead);
            return if h >= end {
                Wake::Now
            } else {
                Wake::After(end - h)
            };
        }
        let need = if draining { bs } else { avail_min.clamp(1, bs) };
        match self.observe(appl_raw, false) {
            Seen::Idle { appl } => match dir {
                Dir::Playback if bs.saturating_sub(appl) >= need => Wake::Now,
                _ => Wake::Never,
            },
            Seen::Live { appl, h } => {
                let avail = match dir {
                    Dir::Playback => bs.saturating_sub(appl.saturating_sub(h)),
                    Dir::Capture => h.saturating_sub(appl).min(bs),
                };
                if avail >= need {
                    Wake::Now
                } else if self.paused.is_some() {
                    Wake::Never
                } else {
                    Wake::After(need - avail)
                }
            }
            Seen::Xrun | Seen::Lost => Wake::Now,
        }
    }

    fn transfer(
        &mut self,
        appl_raw: u64,
        areas: &mut [Area<'_>],
        offset: usize,
        size: usize,
    ) -> Result<usize, StreamError> {
        if size == 0 {
            return Ok(0);
        }
        if areas.len() < self.cfg.channels {
            return Err(Self::fault("too few channel areas"));
        }
        let seen = self.observe(appl_raw, false);
        let fmt = self.cfg.format;
        let channels = self.cfg.channels;
        let bs = self.cfg.buffer_size;
        match (&mut self.role, seen) {
            (_, Seen::Lost) => Err(StreamError::Lost),
            (_, Seen::Xrun) => {
                self.stats.xruns += 1;
                Err(StreamError::Xrun)
            }
            (Role::Tx(_), Seen::Idle { appl }) => {
                // Not started: keep the data until `start` knows the anchor.
                for (c, area) in areas.iter_mut().enumerate().take(channels) {
                    let plane = c * bs as usize;
                    let mut done = 0usize;
                    while done < size {
                        let slot = ((appl + done as u64) % bs) as usize;
                        let n = (bs as usize - slot).min(size - done);
                        let Some(dst) = self.stage.get_mut(plane + slot..plane + slot + n) else {
                            return Err(Self::fault("stage"));
                        };
                        if !S::load(fmt, area, offset + done, dst) {
                            return Err(Self::fault("application buffer"));
                        }
                        done += n;
                    }
                }
                Ok(size)
            }
            (Role::Tx(producer), Seen::Live { appl, .. }) => {
                let Some(run) = self.run else {
                    return Err(Self::fault("no run"));
                };
                let pos0 = run.pos_base.saturating_add(appl);
                let mut late = 0;
                for (c, area) in areas.iter_mut().enumerate().take(channels) {
                    let mut done = 0usize;
                    while done < size {
                        let n = CHUNK.min(size - done);
                        let Some(buf) = self.scratch.get_mut(..n) else {
                            return Err(Self::fault("scratch"));
                        };
                        if !S::load(fmt, area, offset + done, buf) {
                            return Err(Self::fault("application buffer"));
                        }
                        Self::ring_write(producer, &mut late, c, pos0 + done as u64, buf)?;
                        // Keep the frames for a resume after a pause (a copy of at most
                        // one buffer per channel).
                        let plane = c * bs as usize;
                        let mut copied = 0usize;
                        while copied < n {
                            let slot = ((appl + (done + copied) as u64) % bs) as usize;
                            let m = (bs as usize - slot).min(n - copied);
                            if let (Some(dst), Some(src)) = (
                                self.stage.get_mut(plane + slot..plane + slot + m),
                                buf.get(copied..copied + m),
                            ) {
                                dst.copy_from_slice(src);
                            }
                            copied += m;
                        }
                        done += n;
                    }
                }
                self.stats.late_frames += late;
                Ok(size)
            }
            (Role::Rx(_), Seen::Idle { .. }) => Err(Self::fault("capture transfer before start")),
            (Role::Rx(consumer), Seen::Live { appl, .. }) => {
                let Some(run) = self.run else {
                    return Err(Self::fault("no run"));
                };
                let pos0 = run.pos_base.saturating_add(appl);
                for (c, area) in areas.iter_mut().enumerate().take(channels) {
                    let mut done = 0usize;
                    while done < size {
                        let n = CHUNK.min(size - done);
                        let Some(buf) = self.scratch.get_mut(..n) else {
                            return Err(Self::fault("scratch"));
                        };
                        match consumer.read::<S>(c as u32, pos0 + done as u64, buf) {
                            Ok(rep) => {
                                self.stats.unwritten_frames += rep.unwritten as u64;
                                self.stats.hole_frames += rep.hole as u64;
                                self.stats.overrun_frames += rep.overrun as u64;
                            }
                            Err(_) => return Err(Self::fault("ring read")),
                        }
                        if !S::store(fmt, area, offset + done, buf) {
                            return Err(Self::fault("application buffer"));
                        }
                        done += n;
                    }
                }
                consumer.publish_read_pos(pos0.saturating_add(size as u64));
                Ok(size)
            }
        }
    }

    fn set_tick(&mut self, frames: u64) {
        self.cfg.tick_frames = frames;
    }

    fn pause(&mut self, enable: bool, appl_raw: u64) -> Result<(), StreamError> {
        if enable {
            if self.paused.is_some() {
                return Ok(());
            }
            let h = match self.observe(appl_raw, false) {
                Seen::Live { h, .. } => h,
                Seen::Idle { .. } => return Err(Self::fault("pause before start")),
                Seen::Lost => return Err(StreamError::Lost),
                Seen::Xrun => return Err(StreamError::Xrun),
            };
            if let Role::Tx(p) = &mut self.role {
                // What was queued must not keep playing.
                p.reset();
            }
            if let Some(run) = &mut self.run {
                run.last_h = h;
            }
            self.paused = Some(h);
            return Ok(());
        }
        let Some(h) = self.paused.take() else {
            return Ok(());
        };
        let appl = self.appl.update(appl_raw, self.boundary);
        let Some(now) = self.clock.now() else {
            return Err(StreamError::Lost);
        };
        if self.stale(&now) {
            return Err(StreamError::Lost);
        }
        let Some(old) = self.run else {
            return Err(Self::fault("resume without a run"));
        };
        let bs = self.cfg.buffer_size;
        let pos_base = match &mut self.role {
            Role::Tx(producer) => {
                let base = now.samples.saturating_add(old.lead).saturating_sub(h);
                let mut late = 0;
                flush_stage(
                    producer,
                    &self.stage,
                    bs,
                    self.cfg.channels,
                    h,
                    appl,
                    base,
                    &mut late,
                )?;
                self.stats.late_frames += late;
                base
            }
            Role::Rx(_) => now.samples.saturating_sub(old.lead).saturating_sub(h),
        };
        self.run = Some(Run {
            time_base: now.samples.saturating_sub(h),
            pos_base,
            lead: old.lead,
            epoch: now.epoch,
            last_h: h,
        });
        Ok(())
    }

    fn delay(&mut self, appl_raw: u64, draining: bool) -> Result<i64, StreamError> {
        match self.observe(appl_raw, draining) {
            Seen::Idle { appl } => Ok(match self.cfg.dir {
                Dir::Playback => {
                    (appl
                        .saturating_add(self.cfg.period_size)
                        .saturating_add(self.cfg.margin)) as i64
                }
                Dir::Capture => 0,
            }),
            Seen::Live { appl, h } => {
                let lead = self.run.map_or(0, |r| r.lead);
                let queued = match self.cfg.dir {
                    Dir::Playback => appl as i64 - h as i64,
                    Dir::Capture => h as i64 - appl as i64,
                };
                Ok(queued.saturating_add(lead as i64))
            }
            Seen::Lost => Err(StreamError::Lost),
            Seen::Xrun => Err(StreamError::Xrun),
        }
    }

    fn set_sw(&mut self, boundary: u64, stop_threshold: u64) {
        self.boundary = boundary.max(1);
        self.stop_threshold = stop_threshold;
    }

    fn stats(&self) -> StreamStats {
        self.stats
    }
}
