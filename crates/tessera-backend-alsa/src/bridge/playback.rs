//! Network to card: reads the RX ring through the drift-compensating
//! resampler and feeds the playback card (ADR 0006).

use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use tessera_audio_ipc::{ReadReport, RingConsumer, SampleFormat};
use tessera_media::{PiController, Resampler};
use tessera_resample::make_resampler;

use super::card::{CardError, CardPcm};
use super::clock::{BridgeNow, Source};
use super::drift::Drift;
use super::{
    BridgeConfig, BridgeError, BridgeStats, LoopParams, MAX_RELATIVE_RATIO, RxLink,
    transient_frames,
};
use crate::convert::{PcmFormat, encode_interleaved, i32_to_f32};

/// Input frames the buffers hold per resampler call (a chunk can need about
/// `chunk * ratio` plus the filter's start-up delay, up to 256 taps).
pub(super) fn input_capacity(chunk: usize) -> usize {
    chunk * 2 + 1024
}

pub(super) struct Playback {
    card: Box<dyn CardPcm>,
    ch: usize,
    chunk: usize,
    fmt: PcmFormat,
    rate: u32,
    /// Frames the card queue is kept at (after a write).
    target: usize,
    guard: u64,
    /// Position error beyond which the loop re-anchors.
    slip: i64,
    link: Option<Box<RxLink>>,
    drift: Drift,
    running: bool,
    anchored: bool,
    generation: u64,
    /// Next ring position to feed into the resampler.
    in_pos: u64,
    inbuf: Vec<f32>,
    outbuf: Vec<f32>,
    plane: Vec<f32>,
    plane_i: Vec<i32>,
    bytes: Vec<u8>,
    silence: Vec<u8>,
}

impl Playback {
    pub(super) fn new(card: Box<dyn CardPcm>, cfg: &BridgeConfig) -> Result<Self, BridgeError> {
        let ch = card.channels();
        let chunk = cfg.chunk_frames.unwrap_or_else(|| card.period_frames());
        if ch == 0 || chunk == 0 || chunk * 2 > card.buffer_frames() {
            return Err(BridgeError::Config(format!(
                "playback card: {ch} channels, chunk {chunk}, buffer {} frames",
                card.buffer_frames()
            )));
        }
        let rate = card.rate();
        let transient = transient_frames(rate, cfg.loop_bandwidth_hz, cfg.max_correction);
        let fmt = card.format();
        let target = cfg.prefill_periods.max(1) * chunk;
        if target + chunk > card.buffer_frames() {
            return Err(BridgeError::Config(format!(
                "playback card buffer of {} frames cannot hold {} prefilled periods plus one",
                card.buffer_frames(),
                cfg.prefill_periods
            )));
        }
        let mut card = card;
        card.set_wake_avail(card.buffer_frames() - target + chunk)?;
        let rs = make_resampler(cfg.quality, ch, chunk, MAX_RELATIVE_RATIO)?;
        let out_frames = rs.output_frames_max().max(chunk);
        let drift = Drift::new(rs, cfg.quality, rate, loop_params(cfg))?;
        let in_cap = input_capacity(chunk);
        let frame_bytes = ch * fmt.bytes();
        Ok(Self {
            target,
            guard: cfg.rx_guard_frames.map_or_else(
                || (2 * chunk as u64 + 64).max(transient + chunk as u64 + 64),
                u64::from,
            ),
            slip: slip_frames(chunk, transient),
            ch,
            chunk,
            fmt,
            rate,
            link: None,
            running: false,
            anchored: false,
            generation: 0,
            in_pos: 0,
            inbuf: vec![0.0; in_cap * ch],
            outbuf: vec![0.0; out_frames * ch],
            plane: vec![0.0; in_cap],
            plane_i: vec![0; in_cap],
            bytes: vec![0; chunk * frame_bytes],
            silence: vec![0; chunk * frame_bytes],
            drift,
            card,
        })
    }

    pub(super) fn wait(&mut self, timeout: Duration) -> Result<(), CardError> {
        self.card.wait(timeout)
    }

    pub(super) fn retune(&mut self, pi: PiController, max: f64) -> bool {
        self.drift.retune(pi, max)
    }

    pub(super) fn fits(&mut self, max: f64) -> bool {
        self.drift.fits(max)
    }

    /// How far behind the delivered audio the ring is read, frames.
    pub(super) fn guard(&self) -> u64 {
        self.guard
    }

    /// The largest loop transient (frames) the guard and the re-anchor
    /// threshold hold.
    pub(super) fn transient_capacity(&self) -> u64 {
        let by_guard = self.guard.saturating_sub(self.chunk as u64 + 64);
        let by_slip = (self.slip.max(0) as u64).saturating_sub(1024) / 2;
        by_guard.min(by_slip)
    }

    /// Replaces the ring; returns the previous one for dropping elsewhere.
    pub(super) fn set_link(&mut self, link: Option<Box<RxLink>>) -> Option<Box<RxLink>> {
        self.anchored = false;
        std::mem::replace(&mut self.link, link)
    }

    fn start(&mut self) -> Result<(), CardError> {
        let n = self.target;
        let mut written = 0;
        while written + self.chunk <= n {
            let w = self.card.write(&self.silence)?;
            if w == 0 {
                break;
            }
            written += w;
        }
        self.card.start()?;
        self.running = true;
        self.anchored = false;
        Ok(())
    }

    /// Recovers from an xrun or suspend; fatal errors are returned.
    fn on_error(&mut self, e: CardError, stats: &BridgeStats) -> Result<(), BridgeError> {
        match e {
            CardError::Xrun | CardError::Suspended => {
                stats.rx_xruns.fetch_add(1, Relaxed);
                self.running = false;
                self.anchored = false;
                super::recover_card(self.card.as_mut(), stats)
            }
            CardError::Disconnected => Err(BridgeError::Card(CardError::Disconnected)),
            // Anything else may be transient: recover and keep going.
            CardError::Other(_) => {
                stats.card_errors.fetch_add(1, Relaxed);
                self.running = false;
                self.anchored = false;
                super::recover_card(self.card.as_mut(), stats)
            }
        }
    }

    pub(super) fn step(
        &mut self,
        now: &BridgeNow,
        stats: &BridgeStats,
        budget: usize,
    ) -> Result<usize, BridgeError> {
        let mut done = 0;
        while done < budget {
            if !self.running {
                if let Err(e) = self.start() {
                    self.on_error(e, stats)?;
                    break;
                }
            }
            let avail = match self.card.avail() {
                Ok(a) => a,
                Err(e) => {
                    self.on_error(e, stats)?;
                    break;
                }
            };
            // One chunk per wake: write only when the queue (buffer minus free
            // space) will not exceed the target after the write.
            let queued = self.card.buffer_frames().saturating_sub(avail);
            if avail < self.chunk || queued + self.chunk > self.target {
                break;
            }
            match self.produce(now, done, stats) {
                Ok(()) => done += 1,
                Err(e) => {
                    self.on_error(e, stats)?;
                    break;
                }
            }
        }
        Ok(done)
    }

    /// Writes the next chunk to the card.
    fn produce(&mut self, now: &BridgeNow, k: usize, stats: &BridgeStats) -> Result<(), CardError> {
        let Some(link) = self.link.as_mut() else {
            stats.rx_silence_chunks.fetch_add(1, Relaxed);
            self.card.write(&self.silence)?;
            return Ok(());
        };
        if now.source == Source::Local {
            stats.clock_fallback_chunks.fetch_add(1, Relaxed);
        }
        let latency = link.consumer.effective_latency().min(1 << 40);
        // Chunks of one step are queued back to back: chunk `k` plays `k`
        // chunks after the first.
        let at = now.samples.saturating_add((k * self.chunk) as u64);
        let want = at.saturating_sub(latency).saturating_sub(self.guard);
        let mut err = want as i64 - self.in_pos as i64;
        let slip = self.slip;
        if !self.anchored || now.generation != self.generation || err.abs() > slip {
            if self.anchored && now.generation == self.generation {
                stats.rx_slips.fetch_add(1, Relaxed);
            }
            self.in_pos = want;
            self.anchored = true;
            self.generation = now.generation;
            self.drift.reanchor();
            err = 0;
        }
        let dt = self.chunk as f64 / f64::from(self.rate.max(1));
        let ratio = self.drift.update(err as f64, dt, &stats.ratio_errors);
        stats.rx_err.store(err, Relaxed);
        stats.rx_err_max.fetch_max(err.unsigned_abs(), Relaxed);
        stats
            .rx_trim_ppb
            .store(((ratio - 1.0) * 1e9) as i64, Relaxed);
        stats
            .rx_trend_ppb
            .store(((self.drift.trend_ratio() - 1.0) * 1e9) as i64, Relaxed);

        let need = self.drift.rs().input_frames_next();
        if need == 0 || need > self.plane.len() {
            return Err(CardError::Other(libc::EINVAL));
        }
        let ring_fmt = link.consumer.params().format;
        let ring_ch = link.consumer.params().channels as usize;
        for c in 0..self.ch {
            let plane = &mut self.plane[..need];
            if c < ring_ch {
                let rep = read_plane(
                    &mut link.consumer,
                    ring_fmt,
                    c as u32,
                    self.in_pos,
                    plane,
                    &mut self.plane_i[..need],
                );
                if c == 0 {
                    let missing = (rep.unwritten + rep.hole + rep.overrun) as u64;
                    if missing > 0 {
                        stats.rx_missing_frames.fetch_add(missing, Relaxed);
                    }
                }
            } else {
                plane.fill(0.0);
            }
            for (i, v) in plane.iter().enumerate() {
                self.inbuf[i * self.ch + c] = *v;
            }
        }
        self.in_pos = self.in_pos.saturating_add(need as u64);
        let produced = match self
            .drift
            .rs()
            .process(&self.inbuf[..need * self.ch], &mut self.outbuf)
        {
            Ok(r) => r.output_frames.min(self.chunk),
            Err(_) => 0,
        };
        if produced < self.chunk {
            stats
                .rx_short_frames
                .fetch_add((self.chunk - produced) as u64, Relaxed);
            self.outbuf[produced * self.ch..self.chunk * self.ch].fill(0.0);
        }
        encode_interleaved(
            self.fmt,
            &self.outbuf[..self.chunk * self.ch],
            &mut self.bytes,
        );
        self.card.write(&self.bytes)?;
        stats.rx_chunks.fetch_add(1, Relaxed);
        if let Ok(q) = self.card.delay() {
            stats.rx_queue.store(q as u64, Relaxed);
        }
        Ok(())
    }
}

/// The re-anchor threshold: eight chunks plus 1024 frames, or twice the
/// loop's transient plus 1024 if that is more (a slow loop must not be taken
/// for a lost one).
pub(super) fn slip_frames(chunk: usize, transient: u64) -> i64 {
    let s = (8 * chunk as u64 + 1024).max(transient.saturating_mul(2).saturating_add(1024));
    i64::try_from(s).unwrap_or(i64::MAX)
}

/// The loop tuning of a configuration.
pub(super) fn loop_params(cfg: &BridgeConfig) -> LoopParams {
    LoopParams {
        bandwidth_hz: cfg.loop_bandwidth_hz,
        max_correction: cfg.max_correction,
    }
}

/// Reads `out.len()` frames of channel `c` at `pos` as `f32`.
fn read_plane(
    consumer: &mut RingConsumer,
    fmt: SampleFormat,
    c: u32,
    pos: u64,
    out: &mut [f32],
    tmp: &mut [i32],
) -> ReadReport {
    match fmt {
        SampleFormat::F32 => match consumer.read::<f32>(c, pos, out) {
            Ok(r) => r,
            Err(_) => {
                out.fill(0.0);
                ReadReport::default()
            }
        },
        SampleFormat::I32 => match consumer.read::<i32>(c, pos, tmp) {
            Ok(r) => {
                for (o, v) in out.iter_mut().zip(tmp.iter()) {
                    *o = i32_to_f32(*v);
                }
                r
            }
            Err(_) => {
                out.fill(0.0);
                ReadReport::default()
            }
        },
    }
}
