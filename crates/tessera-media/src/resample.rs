//! Built-in resamplers and the quality-level factory.
//!
//! [`LinearResampler`] is a fixed-output-chunk linear interpolator with a
//! live ratio: dependency-free, deterministic, allocation-free in `process`.
//! It is the `linear` quality level and the reference the `rubato` adapter is
//! compared against in tests. [`Passthrough`] is the `off` level.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::drift::{ProcessResult, Quality, Resampler};
use crate::error::ResampleError;

/// Widest relative ratio range the linear resampler supports: the ratio may
/// move within `[1 / MAX_RELATIVE_RATIO, MAX_RELATIVE_RATIO]` at most.
pub const MAX_RELATIVE_RATIO: f64 = 2.0;

/// Linear-interpolation resampler producing a fixed chunk of output frames
/// per call, consuming a variable number of input frames.
///
/// Output frame `k` of a call sits at input position `pos + k * step`
/// (`step = 1 / ratio`) relative to the oldest retained input frame and is
/// interpolated between its two neighbours. At most three input frames are
/// retained between calls. Latency is about one input frame.
#[derive(Debug, Clone)]
pub struct LinearResampler {
    channels: usize,
    chunk: usize,
    max_rel: f64,
    ratio: f64,
    step: f64,
    pos: f64,
    window: Vec<f32>,
    window_frames: usize,
}

impl LinearResampler {
    /// A resampler for `channels` channels emitting `chunk_frames` frames per
    /// call, accepting ratios within `[1 / max_relative_ratio,
    /// max_relative_ratio]`. Allocates once, here.
    ///
    /// # Errors
    /// [`ResampleError::BadConfig`] for zero channels/chunk or a
    /// `max_relative_ratio` outside `[1, MAX_RELATIVE_RATIO]`.
    pub fn new(
        channels: usize,
        chunk_frames: usize,
        max_relative_ratio: f64,
    ) -> Result<Self, ResampleError> {
        if channels == 0
            || chunk_frames == 0
            || !max_relative_ratio.is_finite()
            || !(1.0..=MAX_RELATIVE_RATIO).contains(&max_relative_ratio)
        {
            return Err(ResampleError::BadConfig);
        }
        // Largest input needed per call: floor(pos + (M-1) step) + 2 frames
        // minus what is retained; reserve for the extreme step with slack.
        let max_in = (chunk_frames as f64 * max_relative_ratio) as usize + 6;
        let cap = max_in
            .checked_mul(channels)
            .ok_or(ResampleError::BadConfig)?;
        Ok(Self {
            channels,
            chunk: chunk_frames,
            max_rel: max_relative_ratio,
            ratio: 1.0,
            step: 1.0,
            pos: 0.0,
            window: Vec::with_capacity(cap),
            window_frames: 0,
        })
    }

    fn required_frames(&self) -> usize {
        let last = self.pos + (self.chunk - 1) as f64 * self.step;
        last as usize + 2
    }
}

impl Resampler for LinearResampler {
    fn channels(&self) -> usize {
        self.channels
    }

    fn relative_ratio(&self) -> f64 {
        self.ratio
    }

    fn ratio_limits(&self) -> (f64, f64) {
        (1.0 / self.max_rel, self.max_rel)
    }

    fn set_relative_ratio(&mut self, ratio: f64) -> Result<(), ResampleError> {
        if !ratio.is_finite() || ratio < 1.0 / self.max_rel || ratio > self.max_rel {
            return Err(ResampleError::InvalidRatio);
        }
        self.ratio = ratio;
        self.step = 1.0 / ratio;
        Ok(())
    }

    fn input_frames_next(&self) -> usize {
        self.required_frames().saturating_sub(self.window_frames)
    }

    fn output_frames_max(&self) -> usize {
        self.chunk
    }

    fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
    ) -> Result<ProcessResult, ResampleError> {
        let need = self.input_frames_next();
        let ch = self.channels;
        let in_samples = need * ch;
        let out_samples = self.chunk * ch;
        let Some(new) = input.get(..in_samples) else {
            return Err(ResampleError::InputTooShort { needed: need });
        };
        let Some(out) = output.get_mut(..out_samples) else {
            return Err(ResampleError::OutputTooSmall { needed: self.chunk });
        };
        self.window.extend_from_slice(new);
        self.window_frames += need;

        for (k, frame) in out.chunks_exact_mut(ch).enumerate() {
            let t = self.pos + k as f64 * self.step;
            let i0 = t as usize;
            let frac = (t - i0 as f64) as f32;
            let a = self.window.get(i0 * ch..(i0 + 1) * ch);
            let b = self.window.get((i0 + 1) * ch..(i0 + 2) * ch);
            match (a, b) {
                (Some(a), Some(b)) => {
                    for ((o, x), y) in frame.iter_mut().zip(a).zip(b) {
                        *o = x + (y - x) * frac;
                    }
                }
                _ => frame.iter_mut().for_each(|o| *o = 0.0),
            }
        }

        let t_end = self.pos + self.chunk as f64 * self.step;
        let drop = (t_end as usize).min(self.window_frames);
        self.window.drain(..drop * ch);
        self.window_frames -= drop;
        self.pos = t_end - drop as f64;
        Ok(ProcessResult {
            input_frames: need,
            output_frames: self.chunk,
        })
    }

    fn reset(&mut self) {
        self.window.clear();
        self.window_frames = 0;
        self.pos = 0.0;
    }
}

/// The `off` quality level: copies frames unchanged; the ratio must be 1.0.
#[derive(Debug, Clone)]
pub struct Passthrough {
    channels: usize,
    chunk: usize,
}

impl Passthrough {
    /// A pass-through of `chunk_frames` frames per call.
    ///
    /// # Errors
    /// [`ResampleError::BadConfig`] for zero channels or chunk.
    pub fn new(channels: usize, chunk_frames: usize) -> Result<Self, ResampleError> {
        if channels == 0 || chunk_frames == 0 {
            return Err(ResampleError::BadConfig);
        }
        Ok(Self {
            channels,
            chunk: chunk_frames,
        })
    }
}

impl Resampler for Passthrough {
    fn channels(&self) -> usize {
        self.channels
    }
    fn relative_ratio(&self) -> f64 {
        1.0
    }
    fn ratio_limits(&self) -> (f64, f64) {
        (1.0, 1.0)
    }
    fn set_relative_ratio(&mut self, ratio: f64) -> Result<(), ResampleError> {
        if ratio == 1.0 {
            Ok(())
        } else {
            Err(ResampleError::InvalidRatio)
        }
    }
    fn input_frames_next(&self) -> usize {
        self.chunk
    }
    fn output_frames_max(&self) -> usize {
        self.chunk
    }
    fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
    ) -> Result<ProcessResult, ResampleError> {
        let n = self.chunk * self.channels;
        let Some(src) = input.get(..n) else {
            return Err(ResampleError::InputTooShort { needed: self.chunk });
        };
        let Some(dst) = output.get_mut(..n) else {
            return Err(ResampleError::OutputTooSmall { needed: self.chunk });
        };
        dst.copy_from_slice(src);
        Ok(ProcessResult {
            input_frames: self.chunk,
            output_frames: self.chunk,
        })
    }
    fn reset(&mut self) {}
}

/// A boxed resampler is a resampler, so factory output plugs straight into
/// [`DriftCompensator`](crate::drift::DriftCompensator).
impl<R: Resampler + ?Sized> Resampler for Box<R> {
    fn channels(&self) -> usize {
        (**self).channels()
    }
    fn relative_ratio(&self) -> f64 {
        (**self).relative_ratio()
    }
    fn ratio_limits(&self) -> (f64, f64) {
        (**self).ratio_limits()
    }
    fn set_relative_ratio(&mut self, ratio: f64) -> Result<(), ResampleError> {
        (**self).set_relative_ratio(ratio)
    }
    fn input_frames_next(&self) -> usize {
        (**self).input_frames_next()
    }
    fn output_frames_max(&self) -> usize {
        (**self).output_frames_max()
    }
    fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
    ) -> Result<ProcessResult, ResampleError> {
        (**self).process(input, output)
    }
    fn reset(&mut self) {
        (**self).reset();
    }
}

/// Builds a boxed resampler for a quality level (control thread: allocates).
///
/// This crate provides `Off` and `Linear`. `Cubic`, `SincFast` and `SincHq`
/// need `rubato` and live in the `tessera-resample` crate, whose own
/// `make_resampler` serves every level; here they return
/// [`ResampleError::Unsupported`].
///
/// # Errors
/// [`ResampleError::BadConfig`], [`ResampleError::Unsupported`] or a backend
/// error.
pub fn make_resampler(
    quality: Quality,
    channels: usize,
    chunk_frames: usize,
    max_relative_ratio: f64,
) -> Result<Box<dyn Resampler>, ResampleError> {
    match quality {
        Quality::Off => Ok(Box::new(Passthrough::new(channels, chunk_frames)?)),
        Quality::Linear => Ok(Box::new(LinearResampler::new(
            channels,
            chunk_frames,
            max_relative_ratio,
        )?)),
        Quality::Cubic | Quality::SincFast | Quality::SincHq => Err(ResampleError::Unsupported),
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// Feeds a ramp `x[n] = n` and returns the produced output and the total
    /// input consumed.
    fn run(r: &mut dyn Resampler, calls: usize) -> (Vec<f32>, usize) {
        let ch = r.channels();
        let mut next = 0usize;
        let mut out_all = Vec::new();
        let mut out = vec![0.0f32; r.output_frames_max() * ch];
        for _ in 0..calls {
            let need = r.input_frames_next();
            let input: Vec<f32> = (next..next + need)
                .flat_map(|n| core::iter::repeat_n(n as f32, ch))
                .collect();
            next += need;
            let res = r.process(&input, &mut out).unwrap();
            assert_eq!(res.input_frames, need);
            out_all.extend_from_slice(&out[..res.output_frames * ch]);
        }
        (out_all, next)
    }

    #[test]
    fn unity_ratio_is_an_exact_copy_of_a_ramp() {
        let mut r = LinearResampler::new(2, 48, 1.1).unwrap();
        let (out, _) = run(&mut r, 10);
        for (i, f) in out.chunks_exact(2).enumerate() {
            assert!((f[0] - i as f32).abs() < 1e-3, "frame {i}: {}", f[0]);
            assert_eq!(f[0], f[1]);
        }
    }

    #[test]
    fn ratio_changes_the_rate() {
        // ratio 1.01: 1% more output than input, so input advances 1/1.01 per output frame.
        let mut r = LinearResampler::new(1, 100, 1.1).unwrap();
        r.set_relative_ratio(1.01).unwrap();
        let (out, consumed) = run(&mut r, 100);
        assert_eq!(out.len(), 10_000);
        let expected_in = 10_000.0 / 1.01;
        assert!(
            (consumed as f64 - expected_in).abs() < 4.0,
            "consumed {consumed}"
        );
        // A ramp stays a ramp with slope 1/ratio.
        let slope = (out[9_999] - out[5_000]) / 4_999.0;
        assert!((f64::from(slope) - 1.0 / 1.01).abs() < 1e-4);
        r.set_relative_ratio(0.99).unwrap();
        let (out, consumed) = run(&mut r, 100);
        assert_eq!(out.len(), 10_000);
        assert!(consumed > 10_000);
    }

    #[test]
    fn validation_and_errors() {
        assert_eq!(
            LinearResampler::new(0, 10, 1.1).unwrap_err(),
            ResampleError::BadConfig
        );
        assert_eq!(
            LinearResampler::new(1, 0, 1.1).unwrap_err(),
            ResampleError::BadConfig
        );
        assert_eq!(
            LinearResampler::new(1, 10, 0.5).unwrap_err(),
            ResampleError::BadConfig
        );
        assert_eq!(
            LinearResampler::new(1, 10, 3.0).unwrap_err(),
            ResampleError::BadConfig
        );
        let mut r = LinearResampler::new(1, 16, 1.1).unwrap();
        assert_eq!(
            r.set_relative_ratio(f64::NAN),
            Err(ResampleError::InvalidRatio)
        );
        assert_eq!(r.set_relative_ratio(1.2), Err(ResampleError::InvalidRatio));
        assert_eq!(r.set_relative_ratio(0.0), Err(ResampleError::InvalidRatio));
        let need = r.input_frames_next();
        let mut out = vec![0.0; 16];
        assert_eq!(
            r.process(&vec![0.0; need - 1], &mut out),
            Err(ResampleError::InputTooShort { needed: need })
        );
        assert_eq!(
            r.process(&vec![0.0; need], &mut out[..8]),
            Err(ResampleError::OutputTooSmall { needed: 16 })
        );
        // State unchanged by the failed calls.
        assert_eq!(r.input_frames_next(), need);
    }

    #[test]
    fn extreme_ratios_never_overrun_the_window() {
        for ratio in [0.5, 0.5001, 0.75, 1.0, 1.3, 1.9999, 2.0] {
            let mut r = LinearResampler::new(2, 33, 2.0).unwrap();
            r.set_relative_ratio(ratio).unwrap();
            let cap0 = r.window.capacity();
            run(&mut r, 500);
            assert_eq!(
                r.window.capacity(),
                cap0,
                "window reallocated at ratio {ratio}"
            );
        }
    }

    #[test]
    fn passthrough_and_factory() {
        let mut p = Passthrough::new(2, 4).unwrap();
        let mut out = [0.0f32; 8];
        let r = p.process(&[1.0; 8], &mut out).unwrap();
        assert_eq!((r.input_frames, r.output_frames), (4, 4));
        assert_eq!(out, [1.0; 8]);
        assert_eq!(
            p.set_relative_ratio(1.001),
            Err(ResampleError::InvalidRatio)
        );
        assert!(make_resampler(Quality::Off, 2, 4, 1.1).is_ok());
        assert!(make_resampler(Quality::Linear, 2, 4, 1.1).is_ok());
        assert_eq!(
            make_resampler(Quality::Cubic, 2, 4, 1.1).unwrap_err(),
            ResampleError::Unsupported
        );
    }
}
