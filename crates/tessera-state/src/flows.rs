//! Controller flow conversion through the configuration reader (ADR 0015).
//! This keeps runtime and file validation on the same path.

use serde::Deserialize;
use tessera_control_types::FlowSpec;
use toml::{Table, Value};

use crate::config::{FlowConfig, extract};
use crate::{Environment, Issue};

/// Use exactly the geometry the authority currently exposes, including saved
/// channel-count overrides. Flow validation and all-channel expansion must use
/// the same view.
pub(crate) fn effective_validation_config(
    config: &crate::config::Config,
    devices: &[crate::model::DeviceModel],
) -> crate::config::Config {
    let mut effective = config.clone();
    for configured in &mut effective.devices {
        if let Some(device) = devices.iter().find(|d| d.key.0 == configured.id) {
            configured.rx_channels = device.rx_channels;
            configured.tx_channels = device.tx_channels;
            configured.sample_rate = device.sample_rate;
            configured.encoding = device.encoding;
            configured.latency_us = device.latency_us;
        }
    }
    effective
}

/// Parse the compatibility runtime's restricted `[[flow]]` document.
/// No other top-level keys, tables or scalar flow entries are accepted.
/// Semantic validation against the effective device model happens in the actor.
///
/// # Errors
/// Syntax, scope, unknown keys or flow field types, with field paths.
pub fn parse_flow_specs(input: &str) -> Result<Vec<FlowSpec>, String> {
    let table: Table = toml::from_str(input).map_err(|e| e.to_string())?;
    if !flow_only(&table) {
        return Err("compatibility runtime may submit only [[flow]] entries".into());
    }
    let (config, issues) = extract(table, &Environment::new("controller"));
    if issues.iter().any(Issue::is_error) {
        return Err(issues
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; "));
    }
    config.flows.iter().map(to_spec).collect()
}

pub(crate) fn flow_only(table: &Table) -> bool {
    table.keys().all(|key| key == "flow")
        && table
            .get("flow")
            .is_none_or(|flows| matches!(flows, Value::Array(a) if a.iter().all(Value::is_table)))
}

pub(crate) fn to_spec(flow: &FlowConfig) -> Result<FlowSpec, String> {
    let value = Value::try_from(flow).map_err(|e| e.to_string())?;
    FlowSpec::deserialize(value).map_err(|e| e.to_string())
}

pub(crate) fn from_specs(
    flows: &[FlowSpec],
    env: &Environment,
) -> Result<Vec<FlowConfig>, Vec<Issue>> {
    let mut entries = Vec::with_capacity(flows.len());
    for (i, flow) in flows.iter().enumerate() {
        match Value::try_from(flow) {
            Ok(value) => entries.push(value),
            Err(e) => return Err(vec![Issue::error(format!("flow[{i}]"), e.to_string())]),
        }
    }
    let mut table = Table::new();
    table.insert("flow".into(), Value::Array(entries));
    let (config, issues) = extract(table, env);
    if issues.iter().any(Issue::is_error) {
        Err(issues)
    } else {
        Ok(config.flows)
    }
}
