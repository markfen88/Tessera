//! Finite diagnostic retention, not product routing limits. The observer stops
//! if engine context exceeds its budget; it cannot refuse a real command.
use super::*;

struct Budget {
    nodes: usize,
    channels: usize,
    text: usize,
}
impl Budget {
    fn new() -> Self {
        Self {
            nodes: MAX_NODES,
            channels: MAX_CHANNEL_VALUES,
            text: MAX_TEXT_BYTES,
        }
    }
    fn text(&mut self, value: &str) -> Option<()> {
        self.text = self.text.checked_sub(value.len())?;
        Some(())
    }
    fn specs(&mut self, flows: &[FlowSpec]) -> Option<()> {
        self.nodes = self.nodes.checked_sub(flows.len())?;
        for flow in flows {
            self.channels = self.channels.checked_sub(flow.channels.len())?;
            self.text = self.text.checked_sub(flow_text_bytes(flow)?)?;
        }
        Some(())
    }
    fn config(&mut self, config: &Config) -> Option<()> {
        self.text(&config.daemon.log_level)?;
        self.text(&config.daemon.admin_group)?;
        self.text = self
            .text
            .checked_sub(config.daemon.state_dir.as_os_str().len())?;
        self.text = self
            .text
            .checked_sub(config.daemon.runtime_dir.as_os_str().len())?;
        self.text(config.control.socket_group.as_deref().unwrap_or_default())?;
        self.text(config.network.secondary.as_deref().unwrap_or_default())?;
        self.nodes = self.nodes.checked_sub(config.network.interfaces.len())?;
        self.nodes = self.nodes.checked_sub(config.engine.cpu_affinity.len())?;
        self.nodes = self.nodes.checked_sub(config.backends.enabled.len())?;
        for interface in &config.network.interfaces {
            self.text(interface)?;
        }
        self.nodes = self.nodes.checked_sub(config.devices.len())?;
        for device in &config.devices {
            self.text(&device.id)?;
            self.text(&device.name)?;
            for labels in [&device.rx_channel_labels, &device.tx_channel_labels] {
                self.channels = self.channels.checked_sub(labels.len())?;
                for label in labels.values() {
                    self.text(label)?;
                }
            }
        }
        self.nodes = self.nodes.checked_sub(config.bridges.len())?;
        for bridge in &config.bridges {
            for text in [&bridge.name, &bridge.device, &bridge.alsa_pcm] {
                self.text(text)?;
            }
            self.channels = self
                .channels
                .checked_sub(bridge.channel_list.as_deref().unwrap_or_default().len())?;
        }
        for flows in [&config.flows, &config.legacy_protocol_flows] {
            self.nodes = self.nodes.checked_sub(flows.len())?;
            for flow in flows {
                self.text(&flow.id)?;
                self.text(&flow.device)?;
                self.text(flow.interface.as_deref().unwrap_or_default())?;
                self.channels = self.channels.checked_sub(flow.channels.len())?;
            }
        }
        Some(())
    }
}

pub(super) fn flows(flows: &[FlowSpec]) -> bool {
    Budget::new().specs(flows).is_some()
}

pub(super) fn context(
    config: &Config,
    devices: &[crate::model::DeviceModel],
    intent: &crate::persist::StateFile,
) -> bool {
    let mut budget = Budget::new();
    // Only effective geometry fields are copied from devices. Their labels and
    // subscriptions are not retained by Authority::new/reload.
    devices.len() <= MAX_NODES
        && budget.config(config).is_some()
        && budget
            .specs(intent.protocol_flows.as_deref().unwrap_or_default())
            .is_some()
}
