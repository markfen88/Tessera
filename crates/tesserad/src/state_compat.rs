//! The configuration the runtime runs: the merged configuration files plus
//! what was saved by command (`SetCardBridge` / `RemoveCardBridge`,
//! `SetMonitor`), as the state engine resolves it
//! ([`StateEngine::effective_bridges`], [`StateEngine::effective_monitor`]).

use tessera_state::{Config, StateEngine};

/// `cfg` with the effective bridges and monitors in place, so the runtime
/// (`runtime::derive`, `bridge::plan_bridges`) reads one configuration.
pub fn apply(engine: &StateEngine, cfg: &mut Config) {
    cfg.bridges = engine.effective_bridges();
    for d in &mut cfg.devices {
        let (map, delay) = engine.effective_monitor(&d.id);
        d.monitor_rx_from_tx = map;
        d.monitor_delay_us = delay;
    }
}
