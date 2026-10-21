//! The drift loop of one direction: a resampler whose ratio a PI controller
//! trims on the position error (`tessera_media::DriftCompensator`, ADR 0006).

use std::sync::atomic::{AtomicU64, Ordering};

use tessera_media::{DriftCompensator, PiController, Quality, Resampler};

use super::{BridgeError, LoopParams, follower_pi};

/// Resampler plus controller. With `Quality::Off` there is nothing to trim:
/// the resampler is a pass-through whose ratio cannot change, and the loop
/// only re-anchors.
pub(super) enum Drift {
    Compensated(DriftCompensator<Box<dyn Resampler>>),
    Fixed(Box<dyn Resampler>),
}

impl Drift {
    /// # Errors
    /// [`BridgeError::Config`] if the controller may command a ratio the
    /// resampler was not built for (`DriftCompensator::new` checks).
    pub(super) fn new(
        rs: Box<dyn Resampler>,
        quality: Quality,
        rate_hz: u32,
        params: LoopParams,
    ) -> Result<Self, BridgeError> {
        if quality == Quality::Off {
            return Ok(Drift::Fixed(rs));
        }
        DriftCompensator::new(
            rs,
            follower_pi(rate_hz, params.bandwidth_hz, params.max_correction)?,
        )
        .map(Drift::Compensated)
        .map_err(|e| BridgeError::Config(format!("drift compensator: {e:?}")))
    }

    pub(super) fn rs(&mut self) -> &mut dyn Resampler {
        match self {
            Drift::Compensated(c) => &mut **c.resampler_mut(),
            Drift::Fixed(r) => &mut **r,
        }
    }

    /// One control step: `err` frames (positive: behind, consume faster),
    /// `dt` seconds. Returns the ratio in effect. A resampler that refuses
    /// the ratio keeps its previous one; that is counted in `errors`.
    pub(super) fn update(&mut self, err: f64, dt: f64, errors: &AtomicU64) -> f64 {
        match self {
            Drift::Compensated(c) => match c.update(err, 0.0, dt) {
                Ok(r) => r,
                Err(_) => {
                    errors.fetch_add(1, Ordering::Relaxed);
                    c.resampler_mut().relative_ratio()
                }
            },
            Drift::Fixed(_) => 1.0,
        }
    }

    /// The slowly varying part of the ratio (feed-forward plus integrator): what
    /// the loop has learned about the clock offset, without the proportional
    /// term that follows the position noise.
    pub(super) fn trend_ratio(&self) -> f64 {
        match self {
            Drift::Compensated(c) => c.controller().trend_ratio(),
            Drift::Fixed(_) => 1.0,
        }
    }

    /// Swaps in `pi` (built on the control thread, clamp `max`), keeping
    /// what the loop has learned (the trend, as feed-forward, which
    /// `set_feed_forward` clamps to `max`). No allocation: a controller is
    /// plain data. Returns `false`, leaving the old controller, if `max`
    /// is outside the ratio range the resampler was built for.
    pub(super) fn retune(&mut self, pi: PiController, max: f64) -> bool {
        if !self.fits(max) {
            return false;
        }
        match self {
            Drift::Compensated(c) => {
                let trend = c.controller().trend_ratio();
                *c.controller_mut() = pi;
                c.controller_mut().set_feed_forward(trend - 1.0);
                true
            }
            // Nothing to tune without a resampler.
            Drift::Fixed(_) => true,
        }
    }

    /// Can a controller with clamp `max` drive this resampler?
    pub(super) fn fits(&mut self, max: f64) -> bool {
        match self {
            Drift::Compensated(c) => {
                let (lo, hi) = c.resampler_mut().ratio_limits();
                1.0 - max >= lo - 1e-12 && 1.0 + max <= hi + 1e-12
            }
            Drift::Fixed(_) => true,
        }
    }

    /// Restarts after a discontinuity: clears the resampler's history and the
    /// integrator, keeping what the loop learned about the clock offset as the
    /// feed-forward term (so a re-anchor does not forget the 300 ppm).
    pub(super) fn reanchor(&mut self) {
        match self {
            Drift::Compensated(c) => {
                c.resampler_mut().reset();
                let trend = c.controller().trend_ratio();
                c.controller_mut().reset();
                c.controller_mut().set_feed_forward(trend - 1.0);
            }
            Drift::Fixed(r) => r.reset(),
        }
    }
}
