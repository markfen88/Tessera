//! Drift compensation at independent-clock boundaries (ADR 0003/0006).
//!
//! Resampling is needed only where a foreign hardware clock meets the media
//! clock (a PipeWire graph driven by a sound card, an in-daemon ALSA bridge).
//! Clients that follow the media clock never resample.
//!
//! This module is `core`-only and provides the pieces that do not need a
//! heap:
//!
//! * the [`Resampler`] trait (implemented by the built-in linear resampler,
//!   by the `rubato` adapter behind the `resample` feature, and by
//!   anything a platform crate wants to plug in);
//! * [`PiController`]: a PI loop on ring fill that outputs a resampling
//!   ratio, with an optional feed-forward term and anti-windup;
//! * [`RateEstimator`]: a smoothed estimate of the rate offset between two
//!   clocks, used as the feed-forward input;
//! * [`DriftCompensator`]: glue that keeps a resampler's ratio on target.
//!
//! The resampler runs on the client/bridge side, never in the network
//! engine thread.
//!
//! # Conventions
//!
//! * A *relative ratio* is `output rate / input rate` divided by the
//!   nominal ratio, so `1.0` means "nominal" and `1.0 + 50e-6` means "emit
//!   50 ppm more output frames than nominal".
//! * `fill error = fill - target`, in frames, of the ring the resampler is
//!   attached to. A positive error always means "this ring is too full",
//!   and the loop answers by *lowering* the relative ratio. That is the right
//!   sign both when the ring is the resampler's input (it must consume
//!   faster) and when it is its output (it must produce slower).

use crate::error::{DriftConfigError, ResampleError};

/// Resampling quality levels (ADR 0006 config
/// `off|linear|cubic|sinc-fast|sinc-hq`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Quality {
    /// No resampling (the ratio must stay 1.0).
    Off,
    /// Linear interpolation (built in, any target).
    Linear,
    /// Cubic polynomial interpolation (default on ARM).
    #[default]
    Cubic,
    /// Short windowed-sinc (default on x86).
    SincFast,
    /// Long windowed-sinc.
    SincHq,
}

/// Frames consumed and produced by one [`Resampler::process`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessResult {
    /// Input frames consumed (per channel).
    pub input_frames: usize,
    /// Output frames produced (per channel).
    pub output_frames: usize,
}

/// A streaming resampler over interleaved `f32` frames whose ratio can be
/// trimmed continuously.
///
/// The contract suits fixed-size hardware periods: the caller asks how many
/// input frames the next call needs, supplies them, and receives up to
/// `output_frames_max` frames (exactly one fixed chunk for the built-in and
/// `rubato` `FixedAsync::Output` implementations). Implementations must not
/// allocate in `process`.
pub trait Resampler: Send + core::fmt::Debug {
    /// Channels per frame.
    fn channels(&self) -> usize;

    /// Current relative ratio.
    fn relative_ratio(&self) -> f64;

    /// The `(minimum, maximum)` relative ratio this resampler accepts.
    fn ratio_limits(&self) -> (f64, f64);

    /// Sets the relative ratio (see the module conventions).
    ///
    /// # Errors
    /// [`ResampleError::InvalidRatio`] if it is not finite, not positive, or
    /// outside the range the resampler was built for.
    fn set_relative_ratio(&mut self, ratio: f64) -> Result<(), ResampleError>;

    /// Input frames the next [`process`](Self::process) call consumes.
    fn input_frames_next(&self) -> usize;

    /// Upper bound on the output frames of one call.
    fn output_frames_max(&self) -> usize;

    /// Resamples. `input` must hold at least `input_frames_next()` frames and
    /// `output` at least `output_frames_max()` frames, both interleaved.
    ///
    /// # Errors
    /// [`ResampleError::InputTooShort`], [`ResampleError::OutputTooSmall`]
    /// or a backend error. State is unchanged on a size error.
    fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
    ) -> Result<ProcessResult, ResampleError>;

    /// Clears internal history (after an underrun or a clock epoch change).
    fn reset(&mut self);
}

/// Gains and limits of a [`PiController`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PiConfig {
    /// Proportional gain: relative-ratio change per frame of fill error.
    pub kp: f64,
    /// Integral gain: relative-ratio change per frame of error per second.
    pub ki: f64,
    /// Largest correction magnitude (fraction, `1e-3` = 1000 ppm) applied to
    /// the ratio, feed-forward included.
    pub max_correction: f64,
    /// Time constant (seconds) of a first-order low-pass on the fill error
    /// before it enters the loop; 0 disables it. Fill measured at packet
    /// or period granularity is noisy, and an unfiltered proportional term
    /// would turn that noise into audible ratio jitter. Keep it well below
    /// `1 / (2 pi f_n)` of the loop so the extra pole does not hurt
    /// stability.
    pub error_filter_tau: f64,
}

impl PiConfig {
    /// Checks that every parameter is usable: gains and the filter constant
    /// finite and non-negative, `max_correction` finite and in `(0, 1)`.
    /// [`PiController::new`] calls this, so a controller never holds a
    /// configuration that could make its arithmetic panic or produce NaN.
    ///
    /// # Errors
    /// The first offending parameter.
    pub fn validate(&self) -> Result<(), DriftConfigError> {
        for (name, v) in [
            ("kp", self.kp),
            ("ki", self.ki),
            ("max_correction", self.max_correction),
            ("error_filter_tau", self.error_filter_tau),
        ] {
            if !v.is_finite() {
                return Err(DriftConfigError::NotFinite(name));
            }
            if v < 0.0 {
                return Err(DriftConfigError::Negative(name));
            }
        }
        if !(self.max_correction > 0.0 && self.max_correction < 1.0) {
            return Err(DriftConfigError::BadMaxCorrection);
        }
        Ok(())
    }

    /// Designs the loop as a critically/under-damped second-order system.
    ///
    /// Fill integrates the rate error (`d fill / dt = rate * correction`),
    /// so with natural frequency `w = 2 pi f_n` and damping `zeta`:
    /// `kp = 2 zeta w / rate`, `ki = w^2 / rate`.
    /// A natural frequency of 0.1 to 0.5 Hz and damping around 1 is a sound
    /// start for audio rings; ADR 0011 measurements will tune it.
    #[must_use]
    pub fn from_bandwidth(
        sample_rate: f64,
        natural_freq_hz: f64,
        damping: f64,
        max_correction: f64,
    ) -> Self {
        let w = 2.0 * core::f64::consts::PI * natural_freq_hz;
        Self {
            kp: 2.0 * damping * w / sample_rate,
            ki: w * w / sample_rate,
            max_correction,
            error_filter_tau: 0.0,
        }
    }

    /// The same gains with an error low-pass of time constant `tau` seconds.
    #[must_use]
    pub fn with_error_filter(mut self, tau: f64) -> Self {
        self.error_filter_tau = if tau.is_finite() && tau > 0.0 {
            tau
        } else {
            0.0
        };
        self
    }
}

/// PI controller on ring fill producing a relative resampling ratio.
#[derive(Debug, Clone)]
pub struct PiController {
    cfg: PiConfig,
    integral: f64,
    feed_forward: f64,
    ratio: f64,
    last_error: f64,
    filtered: Option<f64>,
}

impl PiController {
    /// A controller at ratio 1.0 with no feed-forward.
    ///
    /// # Errors
    /// See [`PiConfig::validate`].
    pub fn new(cfg: PiConfig) -> Result<Self, DriftConfigError> {
        cfg.validate()?;
        Ok(Self {
            cfg,
            integral: 0.0,
            feed_forward: 0.0,
            ratio: 1.0,
            last_error: 0.0,
            filtered: None,
        })
    }

    /// Sets the feed-forward term: the expected steady-state correction as a
    /// fraction (for example the output of a [`RateEstimator`]). Non-finite
    /// values are ignored.
    pub fn set_feed_forward(&mut self, fraction: f64) {
        if fraction.is_finite() {
            self.feed_forward = fraction.clamp(-self.cfg.max_correction, self.cfg.max_correction);
        }
    }

    /// The latest relative ratio.
    #[must_use]
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// The latest correction in parts per million (`(ratio - 1) * 1e6`).
    #[must_use]
    pub fn drift_ppm(&self) -> f64 {
        (self.ratio - 1.0) * 1e6
    }

    /// The slowly varying part of the ratio (feed-forward plus integrator,
    /// without the proportional term): what the loop has learned about the
    /// clock offset.
    #[must_use]
    pub fn trend_ratio(&self) -> f64 {
        let max = self.cfg.max_correction;
        1.0 + (self.feed_forward - self.integral).clamp(-max, max)
    }

    /// The latest fill error in frames.
    #[must_use]
    pub fn fill_error(&self) -> f64 {
        self.last_error
    }

    /// Clears the integrator (after a discontinuity); the feed-forward is
    /// kept.
    pub fn reset(&mut self) {
        self.integral = 0.0;
        self.ratio = 1.0;
        self.last_error = 0.0;
        self.filtered = None;
    }

    /// One control step: `fill_error` in frames (`fill - target`), `dt`
    /// seconds since the previous step. Returns the new relative ratio.
    /// Non-finite inputs leave the state unchanged.
    pub fn update(&mut self, fill_error: f64, dt: f64) -> f64 {
        if !fill_error.is_finite() || !dt.is_finite() || dt < 0.0 {
            return self.ratio;
        }
        // Bound the inputs so no intermediate product can overflow to
        // infinity and turn into NaN (a ring is never a quadrillion frames
        // off, nor a control period a million seconds).
        let fill_error = fill_error.clamp(-1e15, 1e15);
        let dt = dt.min(1e6);
        let max = self.cfg.max_correction;
        let tau = self.cfg.error_filter_tau;
        let e = match self.filtered {
            Some(f) if tau > 0.0 => f + (fill_error - f) * (dt / (tau + dt)),
            _ => fill_error,
        };
        self.filtered = Some(e);
        let p = self.cfg.kp * e;
        let old = self.feed_forward - p - self.integral;
        let new_integral = self.integral + self.cfg.ki * e * dt;
        let candidate = self.feed_forward - p - new_integral;
        // Anti-windup (conditional integration): while saturated, only accept
        // an integral change that moves the output back toward range.
        if candidate.abs() <= max || candidate.abs() < old.abs() {
            self.integral = new_integral;
        }
        let correction = (self.feed_forward - p - self.integral).clamp(-max, max);
        // Unreachable for validated gains, but never emit NaN.
        let correction = if correction.is_finite() {
            correction
        } else {
            0.0
        };
        self.last_error = fill_error;
        self.ratio = 1.0 + correction;
        self.ratio
    }
}

/// Smoothed estimate of the rate offset between a device clock and the media
/// clock, from frame counts accumulated over the same interval of
/// `CLOCK_MONOTONIC` time (ADR 0006).
#[derive(Debug, Clone)]
pub struct RateEstimator {
    alpha: f64,
    offset: Option<f64>,
}

impl RateEstimator {
    /// `alpha` in `(0, 1]` is the weight of each new observation (1 = no
    /// smoothing).
    #[must_use]
    pub fn new(alpha: f64) -> Self {
        let alpha = if alpha.is_finite() && alpha > 0.0 {
            alpha.min(1.0)
        } else {
            0.1
        };
        Self {
            alpha,
            offset: None,
        }
    }

    /// Feeds one interval: `media_frames` elapsed on the media clock and
    /// `device_frames` on the device clock over the same real time. Empty
    /// intervals are ignored.
    pub fn observe(&mut self, media_frames: u64, device_frames: u64) {
        if media_frames == 0 {
            return;
        }
        let sample = device_frames as f64 / media_frames as f64 - 1.0;
        self.offset = Some(match self.offset {
            None => sample,
            Some(o) => o + self.alpha * (sample - o),
        });
    }

    /// Estimated `device rate / media rate - 1`, or `None` before the first
    /// observation. For a playback bridge (media in, device out) this is
    /// the feed-forward relative-ratio correction; for capture (device in,
    /// media out) use its negation (to first order).
    #[must_use]
    pub fn offset(&self) -> Option<f64> {
        self.offset
    }
}

/// Keeps a [`Resampler`] at the ratio chosen by a [`PiController`].
#[derive(Debug)]
pub struct DriftCompensator<R: Resampler> {
    resampler: R,
    pi: PiController,
}

impl<R: Resampler> DriftCompensator<R> {
    /// Wraps `resampler` (assumed at ratio 1.0) with a controller.
    ///
    /// # Errors
    /// [`DriftConfigError::CorrectionExceedsRatioRange`] if the controller
    /// may command a ratio outside [`Resampler::ratio_limits`] (with a
    /// resampler that cannot change ratio, any controller is rejected).
    pub fn new(resampler: R, pi: PiController) -> Result<Self, DriftConfigError> {
        let (lo, hi) = resampler.ratio_limits();
        let max = pi.cfg.max_correction;
        if !(1.0 - max >= lo - 1e-12 && 1.0 + max <= hi + 1e-12) {
            return Err(DriftConfigError::CorrectionExceedsRatioRange);
        }
        Ok(Self { resampler, pi })
    }

    /// The resampler.
    pub fn resampler_mut(&mut self) -> &mut R {
        &mut self.resampler
    }

    /// The controller (read-only metrics: ratio, drift ppm, fill error).
    #[must_use]
    pub fn controller(&self) -> &PiController {
        &self.pi
    }

    /// Mutable controller access (feed-forward updates).
    pub fn controller_mut(&mut self) -> &mut PiController {
        &mut self.pi
    }

    /// Runs one control step and applies the ratio to the resampler. Returns
    /// the ratio now in effect.
    ///
    /// # Errors
    /// The resampler's error if it rejects the ratio (the previous ratio
    /// stays in effect).
    pub fn update(&mut self, fill: f64, target: f64, dt: f64) -> Result<f64, ResampleError> {
        let ratio = self.pi.update(fill - target, dt);
        self.resampler.set_relative_ratio(ratio)?;
        Ok(ratio)
    }

    /// Resamples one chunk; see [`Resampler::process`].
    ///
    /// # Errors
    /// See [`Resampler::process`].
    pub fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
    ) -> Result<ProcessResult, ResampleError> {
        self.resampler.process(input, output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pi() -> PiController {
        PiController::new(PiConfig::from_bandwidth(48_000.0, 0.2, 1.0, 2e-3)).unwrap()
    }

    #[test]
    fn positive_error_lowers_the_ratio() {
        let mut c = pi();
        assert!(c.update(10.0, 0.01) < 1.0);
        let mut c = pi();
        assert!(c.update(-10.0, 0.01) > 1.0);
        assert!(c.drift_ppm() > 0.0);
    }

    #[test]
    fn zero_error_stays_nominal() {
        let mut c = pi();
        for _ in 0..100 {
            assert_eq!(c.update(0.0, 0.01), 1.0);
        }
    }

    #[test]
    fn output_is_clamped_and_integrator_does_not_wind_up() {
        let mut c = pi();
        for _ in 0..10_000 {
            c.update(100_000.0, 0.01);
        }
        assert!((c.ratio() - (1.0 - 2e-3)).abs() < 1e-12);
        // Once the error reverses, the output leaves saturation promptly
        // (a wound-up integrator would hold it there for a long time).
        let mut steps = 0;
        while c.ratio() < 1.0 - 1e-3 && steps < 200 {
            c.update(-5_000.0, 0.01);
            steps += 1;
        }
        assert!(steps < 200, "stuck in saturation");
    }

    #[test]
    fn bad_inputs_are_ignored() {
        let mut c = pi();
        c.update(5.0, 0.01);
        let r = c.ratio();
        assert_eq!(c.update(f64::NAN, 0.01), r);
        assert_eq!(c.update(1.0, f64::INFINITY), r);
        assert_eq!(c.update(1.0, -1.0), r);
        c.set_feed_forward(f64::NAN);
        c.set_feed_forward(1.0); // clamped to max_correction
        let mut c2 = pi();
        c2.set_feed_forward(1.0);
        assert!((c2.update(0.0, 0.01) - (1.0 + 2e-3)).abs() < 1e-12);
    }

    #[test]
    fn feed_forward_sets_the_operating_point() {
        let mut c = pi();
        c.set_feed_forward(100e-6);
        assert!((c.update(0.0, 0.01) - 1.000_1).abs() < 1e-12);
    }

    #[test]
    fn hostile_configs_are_errors_not_panics() {
        let ok = PiConfig::from_bandwidth(48_000.0, 0.3, 1.0, 1e-3);
        assert!(PiController::new(ok).is_ok());
        let cases: [(PiConfig, DriftConfigError); 10] = [
            (
                PiConfig {
                    max_correction: -1e-3,
                    ..ok
                },
                DriftConfigError::Negative("max_correction"),
            ),
            (
                PiConfig {
                    max_correction: f64::NAN,
                    ..ok
                },
                DriftConfigError::NotFinite("max_correction"),
            ),
            (
                PiConfig {
                    max_correction: f64::INFINITY,
                    ..ok
                },
                DriftConfigError::NotFinite("max_correction"),
            ),
            (
                PiConfig {
                    max_correction: 0.0,
                    ..ok
                },
                DriftConfigError::BadMaxCorrection,
            ),
            (
                PiConfig {
                    max_correction: 1.0,
                    ..ok
                },
                DriftConfigError::BadMaxCorrection,
            ),
            (
                PiConfig { kp: -1.0, ..ok },
                DriftConfigError::Negative("kp"),
            ),
            (
                PiConfig { ki: f64::NAN, ..ok },
                DriftConfigError::NotFinite("ki"),
            ),
            (
                PiConfig {
                    error_filter_tau: -1.0,
                    ..ok
                },
                DriftConfigError::Negative("error_filter_tau"),
            ),
            (
                PiConfig {
                    error_filter_tau: f64::INFINITY,
                    ..ok
                },
                DriftConfigError::NotFinite("error_filter_tau"),
            ),
            // A zero sample rate yields infinite gains.
            (
                PiConfig::from_bandwidth(0.0, 0.3, 1.0, 1e-3),
                DriftConfigError::NotFinite("kp"),
            ),
        ];
        for (cfg, want) in cases {
            assert_eq!(PiController::new(cfg).unwrap_err(), want, "{cfg:?}");
        }
    }

    #[test]
    fn runtime_inputs_never_panic_or_poison_the_state() {
        let mut c = PiController::new(
            PiConfig::from_bandwidth(48_000.0, 0.3, 1.0, 1e-3).with_error_filter(0.2),
        )
        .unwrap();
        for e in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            1e308,
            -1e308,
            0.0,
            5.0,
        ] {
            for dt in [f64::NAN, f64::INFINITY, -1.0, 0.0, 1e308, 0.01] {
                let r = c.update(e, dt);
                assert!(
                    r.is_finite() && (1.0 - 1e-3..=1.0 + 1e-3).contains(&r),
                    "e={e} dt={dt} r={r}"
                );
                assert!(c.trend_ratio().is_finite() && c.drift_ppm().is_finite());
            }
        }
        // A sane step afterwards behaves normally.
        c.reset();
        assert!(c.update(10.0, 0.01) < 1.0);
        c.set_feed_forward(f64::NEG_INFINITY);
        c.set_feed_forward(-5.0);
        assert!(c.trend_ratio() >= 1.0 - 1e-3);
    }

    #[test]
    fn rate_estimator_converges() {
        let mut e = RateEstimator::new(0.2);
        assert_eq!(e.offset(), None);
        e.observe(0, 5);
        assert_eq!(e.offset(), None);
        for _ in 0..200 {
            e.observe(1_000_000, 1_000_050);
        }
        assert!((e.offset().unwrap() - 50e-6).abs() < 1e-9);
        assert!(RateEstimator::new(f64::NAN).alpha > 0.0);
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    /// Peak-to-peak spread of the ratio when the measured fill alternates
    /// by +-16 frames each step (packet-granularity noise).
    fn jitter(cfg: PiConfig) -> f64 {
        let mut c = PiController::new(cfg).unwrap();
        let (mut lo, mut hi) = (f64::MAX, f64::MIN);
        for i in 0..4000 {
            let r = c.update(if i % 2 == 0 { 16.0 } else { -16.0 }, 0.01);
            if i > 2000 {
                lo = lo.min(r);
                hi = hi.max(r);
            }
        }
        hi - lo
    }

    #[test]
    fn error_filter_suppresses_ratio_jitter() {
        let base = PiConfig::from_bandwidth(48_000.0, 0.3, 1.0, 0.5);
        let raw = jitter(base);
        let filtered = jitter(base.with_error_filter(0.3));
        assert!(raw > 1e-3, "unfiltered jitter {raw}");
        assert!(filtered < raw / 10.0, "filtered {filtered} vs raw {raw}");
        assert_eq!(base.with_error_filter(f64::NAN).error_filter_tau, 0.0);
        assert_eq!(base.with_error_filter(-1.0).error_filter_tau, 0.0);
    }

    #[test]
    fn trend_excludes_the_proportional_term() {
        let mut c = PiController::new(PiConfig::from_bandwidth(48_000.0, 0.3, 1.0, 0.5)).unwrap();
        c.set_feed_forward(100e-6);
        c.update(50.0, 0.0); // dt = 0: only the proportional term acts
        assert!((c.trend_ratio() - 1.000_1).abs() < 1e-12);
        assert!(c.ratio() < c.trend_ratio());
    }
}
