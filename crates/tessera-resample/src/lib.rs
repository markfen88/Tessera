//! Adapter from the `rubato` asynchronous resamplers to
//! [`tessera_media::Resampler`] (ADR 0003/0006).
//!
//! This is a separate crate so that `tessera-media` keeps the workspace MSRV
//! (1.85) and its `no_std` builds: rubato 5.0.1 needs Rust 1.87.
//!
//! The adapter uses `FixedAsync::Output`: every call produces one fixed chunk
//! of output frames (a hardware period) and consumes however many input
//! frames that needs. Quality levels map to rubato interpolators:
//!
//! | [`Quality`] | rubato |
//! |---|---|
//! | `Cubic` | polynomial, cubic (4 points) |
//! | `SincFast` | sinc, 64 taps, linear interpolation, Hann window |
//! | `SincHq` | sinc, 256 taps, cubic interpolation, Blackman-Harris window |
//!
//! `Off` and `Linear` are served by the built-in resamplers of
//! `tessera-media`; [`make_resampler`] here serves every level. The ratio is changed
//! with ramping enabled so a PI loop updating it every period produces no
//! steps.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Adjustable, Async, FixedAsync, PolynomialDegree, Resampler as _, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};

use tessera_media::resample::{LinearResampler, Passthrough};
use tessera_media::{ProcessResult, Quality, ResampleError, Resampler};

/// A `rubato` `Async<f32>` resampler with a fixed output chunk.
pub struct RubatoResampler {
    inner: Async<f32>,
    channels: usize,
    ratio: f64,
    max_rel: f64,
}

impl core::fmt::Debug for RubatoResampler {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RubatoResampler")
            .field("channels", &self.channels)
            .field("ratio", &self.ratio)
            .field("output_frames_max", &self.inner.output_frames_max())
            .finish_non_exhaustive()
    }
}

impl RubatoResampler {
    /// Builds a resampler for `quality` (`Cubic`, `SincFast` or `SincHq`)
    /// emitting `chunk_frames` frames per call, ratio adjustable within
    /// `[1 / max_relative_ratio, max_relative_ratio]`. Allocates, so call it
    /// on the control thread.
    ///
    /// # Errors
    /// [`ResampleError::Unsupported`] for `Off`/`Linear` (use the built-in
    /// resamplers), [`ResampleError::BadConfig`] for invalid parameters.
    pub fn new(
        quality: Quality,
        channels: usize,
        chunk_frames: usize,
        max_relative_ratio: f64,
    ) -> Result<Self, ResampleError> {
        if channels == 0 || chunk_frames == 0 || !max_relative_ratio.is_finite() {
            return Err(ResampleError::BadConfig);
        }
        let fixed = FixedAsync::Output;
        let inner = match quality {
            Quality::Off | Quality::Linear => return Err(ResampleError::Unsupported),
            Quality::Cubic => Async::<f32>::new_poly(
                1.0,
                max_relative_ratio,
                PolynomialDegree::Cubic,
                chunk_frames,
                channels,
                fixed,
            ),
            Quality::SincFast => {
                let p = SincInterpolationParameters::new(64, WindowFunction::Hann2)
                    .oversampling_factor(128)
                    .interpolation(SincInterpolationType::Linear);
                Async::<f32>::new_sinc(1.0, max_relative_ratio, &p, chunk_frames, channels, fixed)
            }
            Quality::SincHq => {
                let p = SincInterpolationParameters::new(256, WindowFunction::BlackmanHarris2)
                    .oversampling_factor(256)
                    .interpolation(SincInterpolationType::Cubic);
                Async::<f32>::new_sinc(1.0, max_relative_ratio, &p, chunk_frames, channels, fixed)
            }
        }
        .map_err(|_| ResampleError::BadConfig)?;
        Ok(Self {
            inner,
            channels,
            ratio: 1.0,
            max_rel: max_relative_ratio,
        })
    }
}

impl Resampler for RubatoResampler {
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
        if !ratio.is_finite() || ratio <= 0.0 {
            return Err(ResampleError::InvalidRatio);
        }
        self.inner
            .set_resample_ratio_relative(ratio, true)
            .map_err(|_| ResampleError::InvalidRatio)?;
        self.ratio = ratio;
        Ok(())
    }

    fn input_frames_next(&self) -> usize {
        self.inner.input_frames_next()
    }

    fn output_frames_max(&self) -> usize {
        self.inner.output_frames_max()
    }

    fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
    ) -> Result<ProcessResult, ResampleError> {
        let need = self.inner.input_frames_next();
        let out_frames = self.inner.output_frames_max();
        let ch = self.channels;
        let input = input
            .get(..need * ch)
            .ok_or(ResampleError::InputTooShort { needed: need })?;
        let output = output
            .get_mut(..out_frames * ch)
            .ok_or(ResampleError::OutputTooSmall { needed: out_frames })?;
        let src = InterleavedSlice::new(input, ch, need).map_err(|_| ResampleError::Backend)?;
        let mut dst = InterleavedSlice::new_mut(output, ch, out_frames)
            .map_err(|_| ResampleError::Backend)?;
        let (input_frames, output_frames) = self
            .inner
            .process_into_buffer(&src, &mut dst, None)
            .map_err(|_| ResampleError::Backend)?;
        Ok(ProcessResult {
            input_frames,
            output_frames,
        })
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}

/// Builds a boxed resampler for any [`Quality`] level (control thread:
/// allocates). `Off` and `Linear` come from `tessera-media`, the rest from
/// rubato.
///
/// # Errors
/// [`ResampleError::BadConfig`] for invalid parameters.
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
        Quality::Cubic | Quality::SincFast | Quality::SincHq => Ok(Box::new(RubatoResampler::new(
            quality,
            channels,
            chunk_frames,
            max_relative_ratio,
        )?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_all_levels_and_rejects_the_rest() {
        for q in [Quality::Cubic, Quality::SincFast, Quality::SincHq] {
            let r = RubatoResampler::new(q, 2, 256, 1.1).unwrap();
            assert_eq!(r.channels(), 2);
            assert_eq!(r.output_frames_max(), 256);
            assert!(r.input_frames_next() > 0);
        }
        assert_eq!(
            RubatoResampler::new(Quality::Linear, 2, 256, 1.1).unwrap_err(),
            ResampleError::Unsupported
        );
        assert_eq!(
            RubatoResampler::new(Quality::Cubic, 0, 256, 1.1).unwrap_err(),
            ResampleError::BadConfig
        );
        assert!(make_resampler(Quality::SincFast, 2, 256, 1.1).is_ok());
    }

    #[test]
    fn ratio_validation() {
        let mut r = RubatoResampler::new(Quality::Cubic, 1, 128, 1.05).unwrap();
        assert_eq!(
            r.set_relative_ratio(f64::NAN),
            Err(ResampleError::InvalidRatio)
        );
        assert_eq!(r.set_relative_ratio(-1.0), Err(ResampleError::InvalidRatio));
        assert_eq!(r.set_relative_ratio(2.0), Err(ResampleError::InvalidRatio));
        assert_eq!(r.relative_ratio(), 1.0);
        r.set_relative_ratio(1.0002).unwrap();
        assert_eq!(r.relative_ratio(), 1.0002);
    }

    #[test]
    fn size_errors_leave_state_alone() {
        let mut r = RubatoResampler::new(Quality::Cubic, 2, 64, 1.1).unwrap();
        let need = r.input_frames_next();
        let mut out = vec![0.0f32; 64 * 2];
        assert_eq!(
            r.process(&vec![0.0; (need - 1) * 2], &mut out),
            Err(ResampleError::InputTooShort { needed: need })
        );
        assert_eq!(
            r.process(&vec![0.0; need * 2], &mut out[..10]),
            Err(ResampleError::OutputTooSmall { needed: 64 })
        );
        assert!(r.process(&vec![0.0; need * 2], &mut out).is_ok());
    }
}
