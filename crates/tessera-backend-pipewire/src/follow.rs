//! Follower mode (ADR 0006/0007): another driver (a hardware card, say) runs
//! the PipeWire graph, so its clock is independent of the media clock. The
//! ring is then read/written through an asynchronous resampler whose ratio is
//! trimmed by the PI loop of `tessera-media` on the ring fill error.
//!
//! One multichannel resampler (`tessera-resample`) serves all channels of a
//! node, so the interpolation tables exist once per node, not once per
//! channel. The resampler works on interleaved frames; the graph and the rings
//! are planar, so the followers stage planar data and (de)interleave around
//! each resampler call. Everything here runs on the real-time thread and is
//! allocation-free after construction.

use tessera_media::{PiConfig, PiController, Quality, ResampleError, Resampler};
use tessera_resample::make_resampler;

/// Largest relative ratio the resampler accepts (+-0.5 %); the PI loop is
/// limited to [`MAX_CORRECTION`] inside it.
pub const MAX_RELATIVE_RATIO: f64 = 1.005;
/// Largest trim the PI loop may apply (3000 ppm, far above any crystal
/// tolerance).
pub const MAX_CORRECTION: f64 = 3e-3;
/// Default bandwidth of the follower loop in Hz: slow on purpose, the error
/// carries the timing jitter of the foreign driver's callbacks.
pub const LOOP_BANDWIDTH_HZ: f64 = 0.05;
/// Time constant of the error low-pass in seconds.
pub const ERROR_FILTER_TAU: f64 = 0.4;

/// The follower PI controller for a node at `rate_hz`.
///
/// # Errors
/// Never in practice; the configuration is constant and valid.
pub fn follower_pi(rate_hz: u32, bandwidth_hz: f64) -> PiController {
    let cfg =
        PiConfig::from_bandwidth(f64::from(rate_hz.max(1)), bandwidth_hz, 1.0, MAX_CORRECTION)
            .with_error_filter(ERROR_FILTER_TAU);
    // The constants above always validate; fall back to a neutral loop rather
    // than ever panicking on the real-time path.
    PiController::new(cfg).unwrap_or_else(|_| {
        PiController::new(PiConfig {
            kp: 0.0,
            ki: 0.0,
            max_correction: MAX_CORRECTION,
            error_filter_tau: 0.0,
        })
        .expect("a zero-gain controller is valid")
    })
}

fn input_capacity(chunk: usize) -> usize {
    // One output chunk needs about chunk * ratio input frames, plus the
    // filter's start-up delay (up to the longest sinc, 256 taps).
    chunk * 2 + 1024
}

/// RX follower: reads the ring through the resampler and stages the planar
/// output so that any graph period can be served.
#[derive(Debug)]
pub struct RxFollower {
    rs: Box<dyn Resampler>,
    pi: PiController,
    channels: usize,
    chunk: usize,
    cap: usize,
    /// Next ring position to feed into the resampler.
    pub in_pos: u64,
    /// Frames staged per channel and not yet delivered.
    pub fill: usize,
    inbuf: Vec<f32>,
    scratch: Vec<f32>,
    chunk_out: Vec<f32>,
    stage: Vec<f32>,
}

impl RxFollower {
    /// Builds a follower for `channels` channels, resampler `quality`, output
    /// chunks of `chunk` frames and graph periods up to `max_period`.
    ///
    /// # Errors
    /// The resampler's construction error.
    pub fn new(
        quality: Quality,
        channels: usize,
        rate_hz: u32,
        chunk: usize,
        max_period: usize,
    ) -> Result<Self, ResampleError> {
        let rs = make_resampler(quality, channels, chunk, MAX_RELATIVE_RATIO)?;
        let cap = max_period + chunk + 8;
        let in_cap = input_capacity(chunk);
        Ok(Self {
            chunk_out: vec![0.0; rs.output_frames_max() * channels],
            chunk,
            rs,
            pi: follower_pi(rate_hz, LOOP_BANDWIDTH_HZ),
            channels,
            cap,
            in_pos: 0,
            fill: 0,
            inbuf: vec![0.0; in_cap * channels],
            scratch: vec![0.0; in_cap],
            stage: vec![0.0; channels * cap],
        })
    }

    /// The position of the oldest frame not yet delivered.
    pub fn head(&self) -> u64 {
        self.in_pos.saturating_sub(self.fill as u64)
    }

    /// Re-anchors: the next input comes from `pos`, staged data is dropped.
    pub fn anchor(&mut self, pos: u64) {
        self.in_pos = pos;
        self.fill = 0;
        self.rs.reset();
        self.pi.reset();
        let _ = self.rs.set_relative_ratio(1.0);
    }

    /// One control step: `err` in frames (positive = the ring is too full,
    /// i.e. we are behind), `dt` seconds. Returns the new relative ratio.
    pub fn control(&mut self, err: f64, dt: f64) -> f64 {
        let r = self.pi.update(err, dt);
        let _ = self.rs.set_relative_ratio(r);
        r
    }

    /// Fills the stage with at least `frames` frames (as far as the
    /// resampler allows). `read(ch, pos, buf)` reads ring frames
    /// `[pos, pos + buf.len())` of channel `ch`.
    pub fn produce<R: FnMut(usize, u64, &mut [f32])>(&mut self, frames: usize, mut read: R) {
        let frames = frames.min(self.cap - self.chunk - 8);
        let ch = self.channels;
        while self.fill < frames {
            let need = self.rs.input_frames_next();
            if need == 0 || need > self.scratch.len() || self.fill + self.chunk > self.cap {
                break;
            }
            for c in 0..ch {
                let Some(buf) = self.scratch.get_mut(..need) else {
                    return;
                };
                read(c, self.in_pos, buf);
                for (i, v) in buf.iter().enumerate() {
                    if let Some(d) = self.inbuf.get_mut(i * ch + c) {
                        *d = *v;
                    }
                }
            }
            self.in_pos = self.in_pos.saturating_add(need as u64);
            let Some(input) = self.inbuf.get(..need * ch) else {
                return;
            };
            let produced = match self.rs.process(input, &mut self.chunk_out) {
                Ok(r) => r.output_frames,
                Err(_) => return,
            };
            if produced == 0 || self.fill + produced > self.cap {
                return;
            }
            for c in 0..ch {
                let base = c * self.cap + self.fill;
                if let Some(dst) = self.stage.get_mut(base..base + produced) {
                    for (i, d) in dst.iter_mut().enumerate() {
                        *d = self.chunk_out.get(i * ch + c).copied().unwrap_or(0.0);
                    }
                }
            }
            self.fill += produced;
        }
    }

    /// The staged frames of channel `ch` (at most `fill`).
    pub fn plane(&self, ch: usize) -> &[f32] {
        let base = ch * self.cap;
        self.stage.get(base..base + self.fill).unwrap_or(&[])
    }

    /// Drops the first `n` staged frames of every channel (delivered).
    pub fn consume(&mut self, n: usize) {
        let n = n.min(self.fill);
        for c in 0..self.channels {
            let base = c * self.cap;
            if base + self.fill <= self.stage.len() {
                self.stage.copy_within(base + n..base + self.fill, base);
            }
        }
        self.fill -= n;
    }
}

/// TX follower: stages the graph's planar input, resamples it and hands the
/// output chunks to the ring writer.
#[derive(Debug)]
pub struct TxFollower {
    rs: Box<dyn Resampler>,
    pi: PiController,
    channels: usize,
    cap: usize,
    /// Next ring position to write.
    pub w_pos: u64,
    fifo: Vec<f32>,
    fill: usize,
    inbuf: Vec<f32>,
    chunk_out: Vec<f32>,
    planar: Vec<f32>,
    out_cap: usize,
    /// Input frames dropped because the staging buffer was full.
    pub dropped: u64,
}

impl TxFollower {
    /// See [`RxFollower::new`].
    ///
    /// # Errors
    /// The resampler's construction error.
    pub fn new(
        quality: Quality,
        channels: usize,
        rate_hz: u32,
        chunk: usize,
        max_period: usize,
    ) -> Result<Self, ResampleError> {
        let rs = make_resampler(quality, channels, chunk, MAX_RELATIVE_RATIO)?;
        let out_cap = rs.output_frames_max();
        let cap = max_period + input_capacity(chunk);
        Ok(Self {
            chunk_out: vec![0.0; out_cap * channels],
            planar: vec![0.0; out_cap * channels],
            rs,
            pi: follower_pi(rate_hz, LOOP_BANDWIDTH_HZ),
            channels,
            cap,
            w_pos: 0,
            fifo: vec![0.0; channels * cap],
            fill: 0,
            inbuf: vec![0.0; input_capacity(chunk) * channels],
            out_cap,
            dropped: 0,
        })
    }

    /// Re-anchors: the next output frame goes to `pos`.
    pub fn anchor(&mut self, pos: u64) {
        self.w_pos = pos;
        self.fill = 0;
        self.rs.reset();
        self.pi.reset();
        let _ = self.rs.set_relative_ratio(1.0);
    }

    /// One control step: `err` in frames (positive = we are ahead of the
    /// wanted write position), `dt` seconds.
    pub fn control(&mut self, err: f64, dt: f64) -> f64 {
        let r = self.pi.update(err, dt);
        let _ = self.rs.set_relative_ratio(r);
        r
    }

    /// Zeroes the next `frames` staging slots of every channel, so channels
    /// the graph does not deliver are silent.
    pub fn begin(&mut self, frames: usize) {
        let n = frames.min(self.cap - self.fill);
        for c in 0..self.channels {
            let base = c * self.cap + self.fill;
            if let Some(s) = self.fifo.get_mut(base..base + n) {
                s.fill(0.0);
            }
        }
    }

    /// Stages the graph's input of channel `ch` (the first `frames` slots).
    pub fn push(&mut self, ch: usize, input: &[f32]) {
        let n = input.len().min(self.cap - self.fill);
        self.dropped += (input.len() - n) as u64;
        let base = ch * self.cap + self.fill;
        if let (Some(dst), Some(src)) = (self.fifo.get_mut(base..base + n), input.get(..n)) {
            dst.copy_from_slice(src);
        }
    }

    /// Commits `frames` staged frames and resamples whatever is complete;
    /// `write(ch, pos, data)` receives each output chunk per channel.
    pub fn commit<W: FnMut(usize, u64, &[f32])>(&mut self, frames: usize, mut write: W) {
        self.fill = (self.fill + frames).min(self.cap);
        let ch = self.channels;
        loop {
            let need = self.rs.input_frames_next();
            if need == 0 || need > self.fill || need * ch > self.inbuf.len() {
                break;
            }
            for c in 0..ch {
                if let Some(src) = self.fifo.get(c * self.cap..c * self.cap + need) {
                    for (i, v) in src.iter().enumerate() {
                        if let Some(d) = self.inbuf.get_mut(i * ch + c) {
                            *d = *v;
                        }
                    }
                }
            }
            let Some(input) = self.inbuf.get(..need * ch) else {
                break;
            };
            let produced = match self.rs.process(input, &mut self.chunk_out) {
                Ok(r) => r.output_frames.min(self.out_cap),
                Err(_) => break,
            };
            for c in 0..ch {
                if let Some(dst) = self
                    .planar
                    .get_mut(c * self.out_cap..c * self.out_cap + produced)
                {
                    for (i, d) in dst.iter_mut().enumerate() {
                        *d = self.chunk_out.get(i * ch + c).copied().unwrap_or(0.0);
                    }
                }
                if let Some(out) = self
                    .planar
                    .get(c * self.out_cap..c * self.out_cap + produced)
                {
                    write(c, self.w_pos, out);
                }
            }
            self.w_pos = self.w_pos.saturating_add(produced as u64);
            for c in 0..ch {
                let base = c * self.cap;
                if base + self.fill <= self.fifo.len() {
                    self.fifo.copy_within(base + need..base + self.fill, base);
                }
            }
            self.fill -= need;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_loop_removes_a_constant_drift_with_timing_jitter() {
        // The measured error carries +-1 ms of callback timing jitter; the
        // loop must settle on the true drift without wobbling.
        let fs = 48_000.0;
        for drift_ppm in [-300.0f64, 120.0, 400.0] {
            let drift = drift_ppm * 1e-6;
            let mut pi = follower_pi(48_000, LOOP_BANDWIDTH_HZ);
            let dt = 480.0 / fs;
            let (mut err, mut x) = (0.0f64, 12345u64);
            let (mut worst, mut wobble) = (0.0f64, 0.0f64);
            for i in 0..(120.0 / dt) as usize {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let noise = (x % 97) as f64 - 48.0;
                let ratio = pi.update(err + noise, dt);
                err += fs * dt * (drift - (1.0 - ratio));
                let t = i as f64 * dt;
                worst = worst.max(err.abs());
                if t > 80.0 {
                    wobble = wobble.max(((1.0 - ratio) - drift).abs());
                    assert!(err.abs() < 12.0, "drift {drift_ppm}: err {err} at {t}");
                }
            }
            assert!(worst < 150.0, "drift {drift_ppm}: peak {worst}");
            assert!(wobble < 250e-6, "drift {drift_ppm}: wobble {wobble}");
        }
    }

    #[test]
    fn rx_follower_off_reads_straight() {
        let mut f = RxFollower::new(Quality::Off, 2, 48_000, 64, 512).unwrap();
        f.anchor(1000);
        f.produce(100, |ch, pos, buf| {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = (pos + i as u64) as f32 + ch as f32 * 0.5;
            }
        });
        assert!(f.fill >= 100);
        assert_eq!(f.plane(0)[0], 1000.0);
        assert_eq!(f.plane(1)[0], 1000.5);
        assert_eq!(f.plane(0)[99], 1099.0);
        let head = f.head();
        assert_eq!(head, 1000);
        f.consume(100);
        assert_eq!(f.head(), 1100);
        assert_eq!(f.plane(0)[0], 1100.0);
    }

    #[test]
    fn tx_follower_off_writes_straight() {
        let mut f = TxFollower::new(Quality::Off, 2, 48_000, 64, 512).unwrap();
        f.anchor(7);
        let mut got = Vec::new();
        f.begin(130);
        f.push(0, &(0..130).map(|i| i as f32).collect::<Vec<_>>());
        f.push(1, &(0..130).map(|i| -(i as f32)).collect::<Vec<_>>());
        f.commit(130, |ch, pos, d| got.push((ch, pos, d.to_vec())));
        // Two full chunks of 64 went out; 2 frames wait in the staging buffer.
        let ch0: Vec<_> = got.iter().filter(|g| g.0 == 0).collect();
        assert_eq!(ch0.len(), 2);
        assert_eq!(ch0[0].1, 7);
        assert_eq!(ch0[1].1, 71);
        assert_eq!(ch0[0].2[0], 0.0);
        assert_eq!(ch0[1].2[0], 64.0);
        let ch1: Vec<_> = got.iter().filter(|g| g.0 == 1).collect();
        assert_eq!(ch1[1].2[3], -67.0);
        assert_eq!(f.w_pos, 7 + 128);
    }

    #[test]
    fn missing_channels_are_silent() {
        let mut f = TxFollower::new(Quality::Off, 2, 48_000, 64, 512).unwrap();
        f.anchor(0);
        f.begin(64);
        f.push(0, &[1.0; 64]);
        let mut seen = Vec::new();
        f.commit(64, |ch, _, d| seen.push((ch, d.iter().sum::<f32>())));
        assert!(seen.contains(&(0, 64.0)));
        assert!(seen.contains(&(1, 0.0)));
    }

    #[test]
    fn rx_follower_resamples_a_sine_at_unity_ratio() {
        for q in [Quality::Linear, Quality::Cubic, Quality::SincFast] {
            let mut f = RxFollower::new(q, 1, 48_000, 64, 1024).unwrap();
            f.anchor(0);
            let read = |_c: usize, pos: u64, buf: &mut [f32]| {
                for (i, b) in buf.iter_mut().enumerate() {
                    *b = ((pos + i as u64) as f32 * 0.01).sin();
                }
            };
            let mut all = Vec::new();
            for _ in 0..20 {
                f.produce(256, read);
                all.extend_from_slice(&f.plane(0)[..256]);
                f.consume(256);
            }
            let tail = &all[2000..4000];
            let mut best = f32::MAX;
            for delay in 0..200u64 {
                let e: f32 = tail
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let want = (((2000 + i) as u64).saturating_sub(delay) as f32 * 0.01).sin();
                        (v - want).abs()
                    })
                    .fold(0.0, f32::max);
                best = best.min(e);
            }
            assert!(best < 0.02, "{q:?}: best error {best}");
        }
    }
}
