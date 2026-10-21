//! In-daemon ALSA hardware-card bridges (`[[bridge]]`; ADR 0007 item 4,
//! ADR 0006, ADR 0004).
//!
//! A bridge moves audio between a local card, whose clock is independent of
//! the media clock, and one contiguous channel range of a device: playback
//! reads the device's RX audio, capture feeds its TX channels. The drift is
//! absorbed by the bridge's resampler and PI loop (`tessera-backend-alsa`);
//! the loop is deliberately slow and its correction clamped (configuration),
//! so it cannot fight a second servo downstream.
//!
//! * [`plan`] (always built): what should run, derived from the effective
//!   devices and the configuration by the runtime reconciler.
//! * `manager` and `rt_loop` (cargo feature `alsa-bridge`): the control
//!   thread that opens cards, retries and reports, and the real-time loop.
//!   Without the feature, the state engine refuses any `[[bridge]]` entry
//!   (`Environment::bridges_supported`), so nothing here ever has work.

pub mod plan;

#[cfg(feature = "alsa-bridge")]
pub mod manager;
#[cfg(feature = "alsa-bridge")]
pub mod rt_loop;

#[cfg(feature = "alsa-bridge")]
pub use manager::{BridgeDeps, BridgeManager, spawn};
pub use plan::{BridgePlan, plan_bridges};

/// Whether this build runs `[[bridge]]` entries.
pub const SUPPORTED: bool = cfg!(feature = "alsa-bridge");

/// Without the feature: a manager that is never given work.
#[cfg(not(feature = "alsa-bridge"))]
#[derive(Debug, Default)]
pub struct BridgeManager;

#[cfg(not(feature = "alsa-bridge"))]
impl BridgeManager {
    /// Nothing to run: configurations with bridges are refused at load.
    pub fn apply(&self, plans: Vec<BridgePlan>, errors: Vec<plan::PlanError>) {
        if !plans.is_empty() || !errors.is_empty() {
            tracing::error!(
                "this tesserad was built without the ALSA bridge (feature `alsa-bridge`); {} bridge(s) not started",
                plans.len() + errors.len()
            );
        }
    }

    /// Nothing to stop.
    pub fn stop(&self) {}
}
