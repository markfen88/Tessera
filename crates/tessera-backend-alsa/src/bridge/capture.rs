//! Card to network: reads the capture card, resamples to the media clock and
//! writes the TX ring ahead of the clock (ADR 0006).

use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use tessera_audio_ipc::{RingProducer, SampleFormat};
use tessera_media::{PiController, Resampler};
use tessera_resample::make_resampler;

use super::card::{CardError, CardPcm};
use super::clock::{BridgeNow, Source};
use super::drift::Drift;
use super::playback::{input_capacity, loop_params, slip_frames};
use super::transient_frames;
use super::{BridgeConfig, BridgeError, BridgeStats, MAX_RELATIVE_RATIO, TxLink};
use crate::convert::{PcmFormat, decode_interleaved, f32_to_i32};

pub(super) struct Capture {
    card: Box<dyn CardPcm>,
    ch: usize,
    chunk: usize,
    fmt: PcmFormat,
    rate: u32,
    lead: u64,
    /// Position error beyond which the loop re-anchors.
    slip: i64,
    link: Option<Box<TxLink>>,
    drift: Drift,
    running: bool,
    anchored: bool,
    generation: u64,
    /// Next ring position to write.
    w_pos: u64,
    fifo: Vec<f32>,
    fifo_frames: usize,
    outbuf: Vec<f32>,
    plane: Vec<f32>,
    plane_i: Vec<i32>,
    bytes: Vec<u8>,
}

impl Capture {
    pub(super) fn new(card: Box<dyn CardPcm>, cfg: &BridgeConfig) -> Result<Self, BridgeError> {
        let ch = card.channels();
        let chunk = cfg.chunk_frames.unwrap_or_else(|| card.period_frames());
        if ch == 0 || chunk == 0 || chunk * 2 > card.buffer_frames() {
            return Err(BridgeError::Config(format!(
                "capture card: {ch} channels, chunk {chunk}, buffer {} frames",
                card.buffer_frames()
            )));
        }
        let rate = card.rate();
        let transient = transient_frames(rate, cfg.loop_bandwidth_hz, cfg.max_correction);
        let fmt = card.format();
        let mut card = card;
        card.set_wake_avail(chunk)?;
        let rs = make_resampler(cfg.quality, ch, chunk, MAX_RELATIVE_RATIO)?;
        let out_frames = rs.output_frames_max().max(chunk);
        let drift = Drift::new(rs, cfg.quality, rate, loop_params(cfg))?;
        let in_cap = input_capacity(chunk);
        let frame_bytes = ch * fmt.bytes();
        Ok(Self {
            lead: cfg.tx_lead_frames.map_or_else(
                || (3 * chunk as u64).max(transient + 2 * chunk as u64),
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
            w_pos: 0,
            fifo: vec![0.0; (in_cap + 2 * chunk) * ch],
            fifo_frames: 0,
            outbuf: vec![0.0; out_frames * ch],
            plane: vec![0.0; out_frames],
            plane_i: vec![0; out_frames],
            bytes: vec![0; chunk * frame_bytes],
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

    /// How far ahead of the media clock the ring is written, frames.
    pub(super) fn lead(&self) -> u64 {
        self.lead
    }

    /// The largest loop transient (frames) the lead and the re-anchor
    /// threshold hold.
    pub(super) fn transient_capacity(&self) -> u64 {
        let by_lead = self.lead.saturating_sub(2 * self.chunk as u64);
        let by_slip = (self.slip.max(0) as u64).saturating_sub(1024) / 2;
        by_lead.min(by_slip)
    }

    pub(super) fn half_period(&self) -> Duration {
        Duration::from_nanos(
            self.card.period_frames() as u64 * 500_000_000 / u64::from(self.rate.max(1)),
        )
    }

    pub(super) fn set_link(&mut self, link: Option<Box<TxLink>>) -> Option<Box<TxLink>> {
        self.anchored = false;
        std::mem::replace(&mut self.link, link)
    }

    fn on_error(&mut self, e: CardError, stats: &BridgeStats) -> Result<(), BridgeError> {
        match e {
            CardError::Xrun | CardError::Suspended => {
                stats.tx_xruns.fetch_add(1, Relaxed);
                self.running = false;
                self.anchored = false;
                self.fifo_frames = 0;
                super::recover_card(self.card.as_mut(), stats)
            }
            CardError::Disconnected => Err(BridgeError::Card(CardError::Disconnected)),
            // Anything else may be transient: recover and keep going.
            CardError::Other(_) => {
                stats.card_errors.fetch_add(1, Relaxed);
                self.running = false;
                self.anchored = false;
                self.fifo_frames = 0;
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
                if let Err(e) = self.card.start() {
                    self.on_error(e, stats)?;
                    break;
                }
                self.running = true;
            }
            let avail = match self.card.avail() {
                Ok(a) => a,
                Err(e) => {
                    self.on_error(e, stats)?;
                    break;
                }
            };
            stats.tx_queue.store(avail as u64, Relaxed);
            if avail < self.chunk {
                break;
            }
            match self.card.read(&mut self.bytes) {
                Ok(n) if n >= self.chunk => {}
                Ok(_) => break,
                Err(e) => {
                    self.on_error(e, stats)?;
                    break;
                }
            }
            stats.tx_chunks.fetch_add(1, Relaxed);
            self.consume(now, done, stats);
            done += 1;
        }
        Ok(done)
    }

    /// Decodes the chunk just read into the fifo and writes every output
    /// chunk the resampler can make from it.
    fn consume(&mut self, now: &BridgeNow, k: usize, stats: &BridgeStats) {
        let Some(link) = self.link.as_mut() else {
            stats.tx_discarded_chunks.fetch_add(1, Relaxed);
            self.fifo_frames = 0;
            return;
        };
        if now.source == Source::Local {
            stats.clock_fallback_chunks.fetch_add(1, Relaxed);
        }
        if self.fifo_frames + self.chunk > self.fifo.len() / self.ch {
            // Cannot happen with the sizes chosen; never overrun the buffer.
            self.fifo_frames = 0;
        }
        let at = self.fifo_frames * self.ch;
        decode_interleaved(
            self.fmt,
            &self.bytes,
            &mut self.fifo[at..at + self.chunk * self.ch],
        );
        self.fifo_frames += self.chunk;
        let at_now = now.samples.saturating_add((k * self.chunk) as u64);
        let target = at_now.saturating_add(self.lead);
        let dt = self.chunk as f64 / f64::from(self.rate.max(1));
        let ring_fmt = link.producer.params().format;
        let ring_ch = link.producer.params().channels as usize;
        // The control variable is the media position of the newest captured
        // frame: what was written plus what still waits in the fifo. It moves
        // smoothly by one chunk per card chunk however the resampler splits
        // the input into output chunks, so it is measured once per card chunk.
        let head = self.w_pos.saturating_add(self.fifo_frames as u64);
        let mut err = head as i64 - target as i64;
        let slip = self.slip;
        let stepped = self.anchored && now.generation != self.generation;
        if !self.anchored || stepped || err.abs() > slip {
            if self.anchored && !stepped {
                stats.tx_slips.fetch_add(1, Relaxed);
            }
            if stepped {
                // The media clock stepped: what is in the ring is stale.
                link.producer.reset();
            }
            self.w_pos = target.saturating_sub(self.fifo_frames as u64);
            self.anchored = true;
            self.generation = now.generation;
            self.drift.reanchor();
            err = 0;
        }
        let ratio = self.drift.update(err as f64, dt, &stats.ratio_errors);
        stats.tx_err.store(err, Relaxed);
        stats.tx_err_max.fetch_max(err.unsigned_abs(), Relaxed);
        stats
            .tx_trim_ppb
            .store(((ratio - 1.0) * 1e9) as i64, Relaxed);
        stats
            .tx_trend_ppb
            .store(((self.drift.trend_ratio() - 1.0) * 1e9) as i64, Relaxed);
        loop {
            let need = self.drift.rs().input_frames_next();
            if need == 0 || need > self.fifo_frames {
                break;
            }
            let produced = match self
                .drift
                .rs()
                .process(&self.fifo[..need * self.ch], &mut self.outbuf)
            {
                Ok(r) => r.output_frames.min(self.chunk),
                Err(_) => 0,
            };
            // Consume the input.
            self.fifo
                .copy_within(need * self.ch..self.fifo_frames * self.ch, 0);
            self.fifo_frames -= need;
            if produced == 0 {
                continue;
            }
            for c in 0..self.ch.min(ring_ch) {
                for i in 0..produced {
                    self.plane[i] = self.outbuf[i * self.ch + c];
                }
                write_plane(
                    &mut link.producer,
                    ring_fmt,
                    c as u32,
                    self.w_pos,
                    &self.plane[..produced],
                    &mut self.plane_i[..produced],
                    stats,
                );
            }
            self.w_pos = self.w_pos.saturating_add(produced as u64);
        }
    }
}

fn write_plane(
    producer: &mut RingProducer,
    fmt: SampleFormat,
    c: u32,
    pos: u64,
    data: &[f32],
    tmp: &mut [i32],
    stats: &BridgeStats,
) {
    let late = match fmt {
        SampleFormat::F32 => producer.write::<f32>(c, pos, data).map_or(0, |r| r.late),
        SampleFormat::I32 => {
            for (t, v) in tmp.iter_mut().zip(data) {
                *t = f32_to_i32(*v);
            }
            producer
                .write::<i32>(c, pos, &tmp[..data.len()])
                .map_or(0, |r| r.late)
        }
    };
    if late > 0 && c == 0 {
        stats.tx_late_frames.fetch_add(late as u64, Relaxed);
    }
}
