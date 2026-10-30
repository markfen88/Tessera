//! Read-only routing catalogue on the existing engine thread. No persistence,
//! protocol requester, listener, routing intent or execution lives here.
use std::collections::{BTreeMap, BTreeSet};
use tessera_control_types::{
    CommandError, DeviceKey, Direction, EndpointRef, Query, Reply, Response, RoutingChannel,
    RoutingChannelPage, RoutingChannelsQuery, RoutingCursor, RoutingDevice, RoutingDevicePage,
    RoutingDevicesQuery, RoutingDiscoveryHealth, RoutingDiscoveryState,
    RoutingUnavailableReason as Reason, WIRE_VERSION,
};
use tessera_discovery::{ServiceId, ServiceKind, ServiceObservation};
use tessera_state::StateEngine;

type CmcHints<'a> = (Option<&'a str>, Option<&'a str>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnAdvertisement {
    pub id: ServiceId,
    pub host: String,
    pub port: u16,
    pub addresses: Vec<std::net::IpAddr>,
    pub txt: tessera_discovery::TxtRecord,
    pub device: DeviceKey,
}

/// A reconciled available-cache snapshot, not a statement about the whole LAN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoverySnapshot {
    /// Browser lifetime, retained even when disable/re-enable is coalesced.
    pub browser_epoch: u64,
    pub observations: Vec<ServiceObservation>,
    pub health: RoutingDiscoveryHealth,
    pub permitted_interfaces: Vec<String>,
    /// Actual announced service IDs and their local logical device keys.
    pub own: Vec<OwnAdvertisement>,
}
impl DiscoverySnapshot {
    pub fn empty(state: RoutingDiscoveryState) -> Self {
        Self {
            browser_epoch: 0,
            observations: Vec::new(),
            health: RoutingDiscoveryHealth::new(state),
            permitted_interfaces: Vec::new(),
            own: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    device: RoutingDevice,
    channels: Vec<RoutingChannel>,
    local: Option<LocalChannels>,
    // Invalidate references when identity, addresses, ports or TXT change,
    // even if the public label/channel rows would otherwise stay identical.
    observations: Vec<ServiceObservation>,
    browser_epoch: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalChannels {
    sample_rate: u32,
    encoding: u8,
    rx_labels: BTreeMap<u16, String>,
    tx_labels: BTreeMap<u16, String>,
    ambiguous_tx_labels: BTreeSet<String>,
}

#[derive(Debug)]
pub(crate) struct Inventory {
    namespace: String,
    serial: u64,
    revision: u64,
    snapshot: DiscoverySnapshot,
    entries: BTreeMap<String, Entry>,
    dirty: bool,
    local_count: usize,
    #[cfg(test)]
    rebuilds: usize,
}
impl Inventory {
    pub fn new() -> Self {
        Self {
            namespace: String::new(),
            serial: 0,
            revision: 0,
            snapshot: DiscoverySnapshot::empty(RoutingDiscoveryState::Starting),
            entries: BTreeMap::new(),
            dirty: true,
            local_count: 0,
            #[cfg(test)]
            rebuilds: 0,
        }
    }
    pub fn report(&mut self, snapshot: DiscoverySnapshot, engine: &StateEngine) {
        if self.snapshot == snapshot {
            return;
        }
        let complete_before = self.complete();
        let state_changed = self.snapshot.health.state != snapshot.health.state;
        self.dirty |= self.snapshot.observations != snapshot.observations
            || self.snapshot.browser_epoch != snapshot.browser_epoch
            || self.snapshot.own != snapshot.own
            || self.snapshot.permitted_interfaces != snapshot.permitted_interfaces;
        self.snapshot = snapshot;
        let completeness_changed = complete_before != self.complete();
        if state_changed || completeness_changed {
            self.revision += 1;
        }
        self.dirty |= completeness_changed;
        if !self.namespace.is_empty() {
            self.refresh(engine);
        }
    }
    fn fresh(&mut self) -> String {
        self.serial += 1;
        format!("{}-{}", self.namespace, self.serial)
    }
    fn complete(&self) -> bool {
        let h = &self.snapshot.health;
        h.state == RoutingDiscoveryState::Browsing
            // Event loss is reconciled by this authoritative full cache
            // snapshot. Current cap/resolve problems keep backend readiness
            // reconciling; lifetime counters remain diagnostic only.
            && h.rejected_observations == 0
    }
    fn refresh(&mut self, engine: &StateEngine) {
        if !self.dirty && self.locals_match(engine) {
            return;
        }
        self.dirty = false;
        self.local_count = engine.model().devices.len();
        #[cfg(test)]
        {
            self.rebuilds += 1;
        }
        let mut wanted = BTreeMap::new();
        for d in &engine.model().devices {
            let key = format!("local:{}", d.key.0);
            let endpoint = EndpointRef::new("", "");
            let mut device = RoutingDevice::new(endpoint.clone(), &d.name);
            device.wire_id = Some(d.wire_device_id.to_hex());
            device.local_device = Some(d.key.clone());
            device.rx_channels = Some(u32::from(d.rx_channels));
            device.tx_channels = Some(u32::from(d.tx_channels));
            device.unavailable.push(Reason::ExecutionUnavailable);
            wanted.insert(
                key,
                Entry {
                    device,
                    channels: Vec::new(),
                    local: Some(LocalChannels {
                        sample_rate: d.sample_rate,
                        encoding: d.encoding,
                        rx_labels: d.rx_labels.clone(),
                        tx_labels: d.tx_labels.clone(),
                        ambiguous_tx_labels: ambiguous_local_labels(d.tx_channels, &d.tx_labels),
                    }),
                    observations: Vec::new(),
                    browser_epoch: None,
                },
            );
        }
        // Group only within a source/interface AND SRV target. Duplicate
        // labels on another appearance never identify the same endpoint.
        let mut groups: BTreeMap<
            (String, String, String, Option<String>),
            Vec<&ServiceObservation>,
        > = BTreeMap::new();
        let mut hints: BTreeMap<String, BTreeSet<CmcHints<'_>>> = BTreeMap::new();
        for observation in &self.snapshot.observations {
            let service = &observation.service;
            let label = match service.id.kind {
                ServiceKind::Arc | ServiceKind::Cmc => service.id.instance.as_str(),
                ServiceKind::Chan | ServiceKind::Bund => match service.id.instance.rsplit_once('@')
                {
                    Some((_, label)) if !label.is_empty() => label,
                    _ => continue,
                },
                _ => continue,
            };
            let key = (
                label.to_ascii_lowercase(),
                service.host.to_ascii_lowercase(),
                observation.source.clone(),
                observation.interface.clone(),
            );
            if service.id.kind == ServiceKind::Cmc {
                hints
                    .entry(label.to_ascii_lowercase())
                    .or_default()
                    .insert((service.txt.get_str("id"), service.txt.get_str("process")));
            }
            groups.entry(key).or_default().push(observation);
        }
        let local_keys: BTreeSet<_> = engine.model().devices.iter().map(|d| &d.key).collect();
        let mut own_by_id: BTreeMap<_, Vec<_>> = BTreeMap::new();
        let mut own_channels: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for own in &self.snapshot.own {
            if local_keys.contains(&own.device) {
                if own.id.kind == ServiceKind::Chan {
                    own_channels
                        .entry((
                            own.host.to_ascii_lowercase(),
                            own.port,
                            channel_txt(&own.txt),
                        ))
                        .or_default()
                        .push(own);
                }
                own_by_id
                    .entry((own.id.kind, own.id.instance.to_ascii_lowercase()))
                    .or_default()
                    .push(own);
            }
        }
        for ((label, host, source, interface), observations) in groups {
            // Own records are linked using actual responder announcements,
            // not display-name coincidence. Local devices remain in state
            // inventory even if browse capacity excluded our observations.
            if observations.iter().all(|o| {
                own_by_id
                    .get(&(
                        o.service.id.kind,
                        o.service.id.instance.to_ascii_lowercase(),
                    ))
                    .is_some_and(|candidates| {
                        candidates.iter().any(|own| {
                            own.host.eq_ignore_ascii_case(&o.service.host)
                                && own.port == o.service.port
                                && txt_equal(&own.txt, &o.service.txt)
                                && o.service
                                    .addresses
                                    .iter()
                                    .any(|a| own.addresses.contains(a))
                        })
                    })
                    // The runtime also advertises friendly names for our own
                    // TX channels. Link only the same published host, port,
                    // complete channel metadata (including id), and address.
                    // A differing CMC identity or channel still stays visible.
                    || (o.service.id.kind == ServiceKind::Chan
                        && own_channels
                            .get(&(
                                o.service.host.to_ascii_lowercase(),
                                o.service.port,
                                channel_metadata(o),
                            ))
                            .is_some_and(|candidates| {
                                candidates.iter().any(|own| {
                                    o.service.addresses.iter().any(|a| own.addresses.contains(a))
                                })
                            }))
            }) {
                continue;
            }
            let key = format!("remote:{label:?}:{host:?}:{source:?}:{interface:?}");
            let endpoint = EndpointRef::new("", "");
            let display = observations[0].service.id.instance.rsplit_once('@').map_or(
                observations[0].service.id.instance.as_str(),
                |(_, label)| label,
            );
            let mut device = RoutingDevice::new(endpoint.clone(), display);
            let wire_ids: BTreeSet<_> = observations
                .iter()
                .filter(|o| o.service.id.kind == ServiceKind::Cmc)
                .filter_map(|o| o.service.txt.get_str("id"))
                .collect();
            if wire_ids.len() == 1 {
                device.wire_id = wire_ids.first().map(|id| (*id).to_owned());
            }
            device.interface.clone_from(&interface);
            device.unavailable = vec![
                Reason::ExecutionUnavailable,
                Reason::ReceiverChannelsUnknown,
                Reason::AdvertisedChannelsOnly,
            ];
            match &interface {
                None => device.unavailable.push(Reason::InterfaceUnknown),
                Some(name) if !self.snapshot.permitted_interfaces.contains(name) => {
                    device.unavailable.push(Reason::InterfaceNotPermitted)
                }
                Some(_) => {}
            }
            if !self.complete() {
                device.unavailable.push(Reason::IncompleteDiscovery);
            }
            if hints.get(&label).is_some_and(|values| values.len() > 1) {
                device.unavailable.push(Reason::ConflictingMetadata);
            }
            // A device that advertises manual multicast flows stays routable:
            // a subscription names a channel, and the receiver takes it from
            // the multicast flow or by unicast (`state-machines.md` §2), so
            // `BundleUnsupported` is no longer reported.
            let mut by_id: BTreeMap<u32, Vec<&ServiceObservation>> = BTreeMap::new();
            for o in &observations {
                if o.service.id.kind != ServiceKind::Chan {
                    continue;
                }
                if let Some(id) = o
                    .service
                    .txt
                    .get_str("id")
                    .and_then(|s| s.parse::<u32>().ok())
                    .filter(|id| *id != 0)
                {
                    by_id.entry(id).or_default().push(o);
                } else if !device.unavailable.contains(&Reason::ConflictingMetadata) {
                    device.unavailable.push(Reason::ConflictingMetadata);
                }
            }
            let mut channels = Vec::new();
            for (id, aliases) in by_id {
                let first = aliases[0];
                let friendly: Vec<_> = aliases
                    .iter()
                    .filter(|o| !o.service.txt.contains("default"))
                    .collect();
                let selected = if friendly.len() == 1 {
                    *friendly[0]
                } else {
                    first
                };
                let name = selected
                    .service
                    .id
                    .instance
                    .rsplit_once('@')
                    .map_or("", |(name, _)| name);
                let mut row = RoutingChannel::new(endpoint.clone(), Direction::Tx, id, name);
                row.aliases = aliases
                    .iter()
                    .filter_map(|o| {
                        o.service
                            .id
                            .instance
                            .rsplit_once('@')
                            .map(|(name, _)| name.to_owned())
                    })
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                row.sample_rate = selected
                    .service
                    .txt
                    .get_str("rate")
                    .and_then(|s| s.parse().ok())
                    .filter(|n| *n != 0);
                row.encoding = selected
                    .service
                    .txt
                    .get_str("enc")
                    .and_then(|s| s.parse().ok())
                    .filter(|n| *n != 0);
                row.unavailable.clone_from(&device.unavailable);
                // More than one friendly label or differing advertised format
                // cannot be resolved by arbitrarily choosing an alias.
                if friendly.len() > 1
                    || aliases.iter().any(|o| {
                        o.service.port != first.service.port
                            || channel_metadata(o) != channel_metadata(first)
                    })
                {
                    row.unavailable.push(Reason::ConflictingMetadata);
                }
                channels.push(row);
            }
            // Advertisements alone cannot prove the total TX count either.
            wanted.insert(
                key,
                Entry {
                    device,
                    channels,
                    local: None,
                    observations: observations.into_iter().cloned().collect(),
                    browser_epoch: Some(self.snapshot.browser_epoch),
                },
            );
        }
        // Saved subscription intent is name-only. Never offer an apparently
        // unique binding when several observations have that device name.
        let mut labels = BTreeMap::new();
        for entry in wanted.values() {
            *labels
                .entry(entry.device.label.to_ascii_lowercase())
                .or_insert(0usize) += 1;
        }
        for entry in wanted.values_mut() {
            let mut channel_names = BTreeMap::new();
            for channel in &entry.channels {
                *channel_names
                    .entry(channel.label.to_ascii_lowercase())
                    .or_insert(0usize) += 1;
            }
            for channel in &mut entry.channels {
                if channel_names[&channel.label.to_ascii_lowercase()] > 1
                    && !channel.unavailable.contains(&Reason::ConflictingMetadata)
                {
                    channel.unavailable.push(Reason::ConflictingMetadata);
                }
            }
            if labels[&entry.device.label.to_ascii_lowercase()] > 1 {
                if !entry
                    .device
                    .unavailable
                    .contains(&Reason::ConflictingMetadata)
                {
                    entry.device.unavailable.push(Reason::ConflictingMetadata);
                }
                for channel in &mut entry.channels {
                    if !channel.unavailable.contains(&Reason::ConflictingMetadata) {
                        channel.unavailable.push(Reason::ConflictingMetadata);
                    }
                }
            }
        }
        let mut next = BTreeMap::new();
        for (key, mut entry) in wanted {
            if let Some(old) = self.entries.get(&key) {
                entry.device.endpoint.clone_from(&old.device.endpoint);
                for channel in &mut entry.channels {
                    channel.endpoint.clone_from(&old.device.endpoint);
                }
                if old != &entry {
                    entry.device.endpoint.generation = self.fresh();
                }
            } else {
                entry.device.endpoint = EndpointRef::new(self.fresh(), self.fresh());
            }
            for channel in &mut entry.channels {
                channel.endpoint.clone_from(&entry.device.endpoint);
            }
            next.insert(key, entry);
        }
        if next != self.entries {
            self.revision += 1;
            self.entries = next;
        }
    }
    fn locals_match(&self, engine: &StateEngine) -> bool {
        self.local_count == engine.model().devices.len()
            && engine.model().devices.iter().all(|d| {
                self.entries
                    .get(&format!("local:{}", d.key.0))
                    .is_some_and(|e| {
                        e.device.label == d.name
                            && e.device.wire_id.as_deref()
                                == Some(d.wire_device_id.to_hex().as_str())
                            && e.device.rx_channels == Some(u32::from(d.rx_channels))
                            && e.device.tx_channels == Some(u32::from(d.tx_channels))
                            && e.local.as_ref().is_some_and(|local| {
                                local.sample_rate == d.sample_rate
                                    && local.encoding == d.encoding
                                    && local.rx_labels == d.rx_labels
                                    && local.tx_labels == d.tx_labels
                            })
                    })
            })
    }
    fn synchronize(&mut self, engine: &StateEngine) -> Result<(), CommandError> {
        if self.namespace.is_empty() {
            let mut bytes = [0; 16];
            getrandom::fill(&mut bytes).map_err(|error| CommandError::Internal {
                message: format!("routing inventory namespace unavailable: {error}"),
            })?;
            self.namespace = bytes.iter().map(|b| format!("{b:02x}")).collect();
        }
        self.refresh(engine);
        Ok(())
    }

    pub(crate) fn receiver_target(
        &mut self,
        engine: &StateEngine,
        endpoint: &EndpointRef,
    ) -> Result<crate::routing_receiver::Target, CommandError> {
        use crate::routing_receiver::{Target, unsupported};
        self.synchronize(engine)?;
        let entry = self
            .entries
            .values()
            .find(|e| &e.device.endpoint == endpoint)
            .ok_or(CommandError::RoutingSnapshotExpired)?;
        if !self.complete()
            || entry.device.unavailable.iter().any(|reason| {
                matches!(
                    reason,
                    Reason::ConflictingMetadata
                        | Reason::BundleUnsupported
                        | Reason::InterfaceUnknown
                        | Reason::InterfaceNotPermitted
                )
            })
        {
            return Err(unsupported(
                "receiver discovery is incomplete, ambiguous or outside permitted interfaces",
            ));
        }
        let network = engine
            .network_status()
            .ok_or_else(|| unsupported("host interface observation unavailable"))?;
        let (interface, port, addresses, txt) = if let Some(local) = &entry.device.local_device {
            if engine.model().devices.first().map(|d| &d.key) != Some(local) {
                return Err(unsupported(
                    "only the first local logical device has a receiver endpoint",
                ));
            }
            let mut arc = self
                .snapshot
                .own
                .iter()
                .filter(|o| o.device == *local && o.id.kind == ServiceKind::Arc);
            let own = arc
                .next()
                .ok_or_else(|| unsupported("local receiver advertisement unavailable"))?;
            if arc.next().is_some() {
                return Err(unsupported("local receiver advertisement is ambiguous"));
            }
            let interface = network
                .primary
                .as_ref()
                .ok_or_else(|| unsupported("primary interface unavailable"))?;
            if engine
                .model()
                .network
                .selected
                .as_ref()
                .is_some_and(|selected| selected != interface)
            {
                return Err(unsupported("primary interface change is pending"));
            }
            (interface, own.port, &own.addresses, &own.txt)
        } else {
            let mut arc = entry
                .observations
                .iter()
                .filter(|o| o.service.id.kind == ServiceKind::Arc);
            let observed = arc
                .next()
                .ok_or_else(|| unsupported("receiver control advertisement unavailable"))?;
            if arc.next().is_some() {
                return Err(unsupported("receiver control advertisement is ambiguous"));
            }
            (
                entry
                    .device
                    .interface
                    .as_ref()
                    .ok_or_else(|| unsupported("receiver interface unavailable"))?,
                observed.service.port,
                &observed.service.addresses,
                &observed.service.txt,
            )
        };
        if !self.snapshot.permitted_interfaces.contains(interface) {
            return Err(unsupported("receiver interface is not permitted"));
        }
        let local = network
            .interfaces
            .iter()
            .find(|i| &i.name == interface && i.usable && i.carrier)
            .and_then(|i| i.ipv4.as_ref())
            .and_then(|a| a.parse::<std::net::Ipv4Addr>().ok())
            .ok_or_else(|| unsupported("receiver interface has no usable observed IPv4 address"))?;
        let ipv4: BTreeSet<_> = addresses
            .iter()
            .filter_map(|a| match a {
                std::net::IpAddr::V4(a) => Some(*a),
                _ => None,
            })
            .collect();
        if ipv4.len() != 1 || port == 0 {
            return Err(unsupported(
                "receiver needs one unambiguous IPv4 control endpoint",
            ));
        }
        let peer = *ipv4.first().expect("one IPv4 address");
        if peer.is_unspecified() || peer.is_loopback() || peer.is_multicast() || peer.is_broadcast()
        {
            return Err(unsupported("receiver control endpoint is not unicast"));
        }
        if entry.local.is_some() && peer != local {
            return Err(unsupported(
                "local receiver advertisement differs from the primary address",
            ));
        }
        let profile = txt
            .get_str("arcp_vers")
            .and_then(|version| tessera_control::arc::Legacy::from_advertised_version(version).ok())
            .ok_or_else(|| unsupported("receiver ARC profile is not supported"))?;
        Ok(Target {
            endpoint: endpoint.clone(),
            label: entry.device.label.clone(),
            interface: interface.clone(),
            source: local,
            peer: std::net::SocketAddrV4::new(peer, port),
            profile,
        })
    }

    pub(crate) fn receiver_result(
        &mut self,
        engine: &StateEngine,
        target: &crate::routing_receiver::Target,
        page: crate::routing_receiver::PreparedPage,
        max_frame: usize,
    ) -> Result<Reply, CommandError> {
        if self.receiver_target(engine, &target.endpoint)? != *target {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        page.into_reply(max_frame)
    }

    /// Execution class of a receiver: this host's first receiver (its channel
    /// count), or another device's receiver reached over its own ARC endpoint
    /// (`None`: the count comes from a complete observation). A remote
    /// receiver applies its own locks and admission to the write, as it does
    /// for any controller. mDNS is resolution, never authentication.
    pub(crate) fn operation_geometry(
        &mut self,
        engine: &StateEngine,
        target: &crate::routing_receiver::Target,
    ) -> Result<Option<u16>, CommandError> {
        if self.receiver_target(engine, &target.endpoint)? != *target {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        let entry = self
            .entries
            .values()
            .find(|e| e.device.endpoint == target.endpoint)
            .ok_or(CommandError::RoutingSnapshotExpired)?;
        let first = engine
            .model()
            .devices
            .first()
            .ok_or_else(|| crate::routing_receiver::unsupported("no protocol device"))?;
        if entry.device.local_device.is_none() {
            return Ok(None);
        }
        if entry.device.local_device.as_ref() != Some(&first.key) {
            return Err(crate::routing_receiver::unsupported(
                "only the first local logical device has a receiver endpoint",
            ));
        }
        if first.rx_channels > 255 {
            return Err(crate::routing_receiver::unsupported(
                "receiver mutation geometry exceeds the evidenced wire profile",
            ));
        }
        Ok(Some(first.rx_channels))
    }

    pub(crate) fn operation_source(
        &mut self,
        engine: &StateEngine,
        reference: &tessera_control_types::RoutingSourceRef,
        receiver_local: bool,
    ) -> Result<tessera_control_types::ReceiverSelection, CommandError> {
        self.synchronize(engine)?;
        let entry = self
            .entries
            .values()
            .find(|e| e.device.endpoint == reference.endpoint)
            .ok_or(CommandError::RoutingSnapshotExpired)?;
        // The host's own channels are synthesized rows, as in `local_channels`.
        let own = entry.local.as_ref().and_then(|local| {
            let index = u16::try_from(reference.channel).ok()?;
            if index == 0 || u64::from(index) > u64::from(entry.device.tx_channels.unwrap_or(0)) {
                return None;
            }
            let label = local
                .tx_labels
                .get(&index)
                .cloned()
                .unwrap_or_else(|| format!("TX {index}"));
            let mut row = RoutingChannel::new(
                entry.device.endpoint.clone(),
                Direction::Tx,
                reference.channel,
                label,
            );
            row.sample_rate = Some(local.sample_rate);
            row.encoding = Some(local.encoding);
            if local
                .ambiguous_tx_labels
                .contains(&row.label.to_ascii_lowercase())
            {
                row.unavailable.push(Reason::ConflictingMetadata);
            }
            Some(row)
        });
        let channel = match &own {
            Some(row) => row,
            None => entry
                .channels
                .iter()
                .find(|c| c.direction == Direction::Tx && c.id == reference.channel)
                .ok_or(CommandError::RoutingSnapshotExpired)?,
        };
        self.qualified_source(engine, entry, channel, receiver_local)?;
        Ok(tessera_control_types::ReceiverSelection::new(
            &entry.device.label,
            &channel.label,
        ))
    }

    fn qualified_source(
        &self,
        engine: &StateEngine,
        entry: &Entry,
        channel: &RoutingChannel,
        receiver_local: bool,
    ) -> Result<(), CommandError> {
        use crate::routing_receiver::unsupported;
        // A host's own transmitter may feed another device's receiver; it is
        // never a source for its own receiver.
        let own = entry.local.is_some();
        if !self.complete()
            || (own && receiver_local)
            || channel.unavailable.iter().any(|r| {
                matches!(
                    r,
                    Reason::ConflictingMetadata
                        | Reason::BundleUnsupported
                        | Reason::InterfaceUnknown
                        | Reason::InterfaceNotPermitted
                        | Reason::IncompleteDiscovery
                )
            })
        {
            return Err(unsupported(
                "source discovery is incomplete, ambiguous or not a qualified remote source",
            ));
        }
        let first = engine
            .model()
            .devices
            .first()
            .ok_or_else(|| unsupported("no protocol device"))?;
        if channel.sample_rate != Some(first.sample_rate)
            || channel.encoding != Some(first.encoding)
        {
            return Err(unsupported("source and receiver formats do not match"));
        }
        let ids: BTreeSet<_> = entry
            .observations
            .iter()
            .filter(|o| o.service.id.kind == ServiceKind::Cmc)
            .filter_map(|o| o.service.txt.get_str("id"))
            .collect();
        if !own
            && (ids.len() != 1
                || !engine
                    .config()
                    .config
                    .control
                    .routing_sources
                    .iter()
                    .any(|id| id.eq_ignore_ascii_case(ids.first().copied().unwrap_or(""))))
        {
            return Err(unsupported(
                "source host has not been qualified for guarded receiver routing",
            ));
        }
        tessera_state::env::validate_device_name(&entry.device.label, 255)
            .map_err(|_| unsupported("source device name is not protocol-compatible"))?;
        tessera_state::env::validate_channel_label(&channel.label, 255)
            .map_err(|_| unsupported("source channel name is not protocol-compatible"))?;
        Ok(())
    }

    pub fn query(
        &mut self,
        engine: &StateEngine,
        query: Query,
        max_frame: usize,
    ) -> Result<Reply, CommandError> {
        self.synchronize(engine)?;
        match query {
            Query::RoutingDevices(RoutingDevicesQuery { cursor, .. }) => {
                self.devices(cursor.as_ref(), max_frame)
            }
            Query::RoutingChannels(q) => self.channels(&q, max_frame),
            _ => engine.query(&query),
        }
    }
    fn devices(&self, cursor: Option<&RoutingCursor>, max: usize) -> Result<Reply, CommandError> {
        let snapshot = format!("{}-{}", self.namespace, self.revision);
        let mut rows: Vec<_> = self.entries.values().map(|e| e.device.clone()).collect();
        rows.sort_by(|a, b| (&a.label, &a.endpoint.id).cmp(&(&b.label, &b.endpoint.id)));
        let start = cursor_start(cursor, "devices", &snapshot, rows.len())?;
        let mut page = RoutingDevicePage::new(&snapshot, self.snapshot.health.clone());
        page.complete = self.complete();
        page.next_cursor = Some(RoutingCursor("x".repeat(256)));
        let base = wire_len(Reply::RoutingDevices(page.clone()))?;
        let end = page_end(&rows, start, base, max)?;
        page.devices = rows[start..end].to_vec();
        page.next_cursor = (end < rows.len()).then(|| make_cursor("devices", &snapshot, end));
        Ok(Reply::RoutingDevices(page))
    }
    fn channels(&self, q: &RoutingChannelsQuery, max: usize) -> Result<Reply, CommandError> {
        let e = self
            .entries
            .values()
            .find(|e| e.device.endpoint == q.endpoint)
            .ok_or(CommandError::RoutingSnapshotExpired)?;
        if let Some(local) = &e.local {
            return self.local_channels(e, local, q, max);
        }
        let rows: Vec<_> = e
            .channels
            .iter()
            .filter(|r| r.direction == q.direction)
            .cloned()
            .collect();
        let scope = match q.direction {
            Direction::Rx => "rx",
            Direction::Tx => "tx",
        };
        let snapshot = &e.device.endpoint.generation;
        let start = cursor_start(q.cursor.as_ref(), scope, snapshot, rows.len())?;
        let mut page = RoutingChannelPage::new(snapshot, self.snapshot.health.clone());
        page.complete = e.device.local_device.is_some();
        page.next_cursor = Some(RoutingCursor("x".repeat(256)));
        let base = wire_len(Reply::RoutingChannels(page.clone()))?;
        let end = page_end(&rows, start, base, max)?;
        page.channels = rows[start..end].to_vec();
        page.next_cursor = (end < rows.len()).then(|| make_cursor(scope, snapshot, end));
        Ok(Reply::RoutingChannels(page))
    }

    fn local_channels(
        &self,
        entry: &Entry,
        local: &LocalChannels,
        query: &RoutingChannelsQuery,
        max: usize,
    ) -> Result<Reply, CommandError> {
        let (scope, count, labels) = match query.direction {
            Direction::Rx => (
                "rx",
                entry.device.rx_channels.unwrap_or(0),
                &local.rx_labels,
            ),
            Direction::Tx => (
                "tx",
                entry.device.tx_channels.unwrap_or(0),
                &local.tx_labels,
            ),
        };
        let generation = &entry.device.endpoint.generation;
        let start = cursor_start(query.cursor.as_ref(), scope, generation, count as usize)?;
        let mut page = RoutingChannelPage::new(generation, self.snapshot.health.clone());
        page.complete = true;
        page.next_cursor = Some(RoutingCursor("x".repeat(256)));
        let mut size = wire_len(Reply::RoutingChannels(page.clone()))?;
        let mut end = start;
        // Generate only the requested page, even for a 65535-channel device.
        for index in start + 1..=count as usize {
            let channel = u16::try_from(index).map_err(|_| CommandError::Internal {
                message: "local channel is outside its one-based range".into(),
            })?;
            let label = labels
                .get(&channel)
                .cloned()
                .unwrap_or_else(|| format!("{} {index}", scope.to_ascii_uppercase()));
            let mut row = RoutingChannel::new(
                entry.device.endpoint.clone(),
                query.direction,
                index as u32,
                label,
            );
            row.local_channel = tessera_control_types::Channel::new(channel);
            row.sample_rate = Some(local.sample_rate);
            row.encoding = Some(local.encoding);
            row.unavailable.push(Reason::ExecutionUnavailable);
            if query.direction == Direction::Tx
                && local
                    .ambiguous_tx_labels
                    .contains(&row.label.to_ascii_lowercase())
            {
                row.unavailable.push(Reason::ConflictingMetadata);
            }
            let bytes = serde_json::to_vec(&row).map_err(|_| CommandError::Internal {
                message: "cannot encode local channel".into(),
            })?;
            let candidate = size.saturating_add(bytes.len()).saturating_add(1);
            if candidate > max {
                if end == start {
                    return Err(CommandError::ResponseTooLarge {
                        size: candidate as u64,
                        limit: max as u64,
                        key: "control.max_frame_bytes".into(),
                    });
                }
                break;
            }
            size = candidate;
            page.channels.push(row);
            end = index;
        }
        page.next_cursor = (end < count as usize).then(|| make_cursor(scope, generation, end));
        Ok(Reply::RoutingChannels(page))
    }
}

/// Only explicit labels can collide with another explicit label or a default.
/// Avoid enumerating every local ordinal on each page request.
fn ambiguous_local_labels(count: u16, labels: &BTreeMap<u16, String>) -> BTreeSet<String> {
    let mut names = BTreeMap::new();
    for label in labels.values() {
        *names.entry(label.to_ascii_lowercase()).or_insert(0usize) += 1;
    }
    names
        .into_iter()
        .filter_map(|(name, occurrences)| {
            let default_collision = name
                .strip_prefix("tx ")
                .and_then(|n| n.parse::<u16>().ok())
                .is_some_and(|n| {
                    n != 0 && n <= count && !labels.contains_key(&n) && name == format!("tx {n}")
                });
            (occurrences > 1 || default_collision).then_some(name)
        })
        .collect()
}

fn channel_metadata(o: &ServiceObservation) -> Vec<Vec<u8>> {
    channel_txt(&o.service.txt)
}
fn txt_equal(a: &tessera_discovery::TxtRecord, b: &tessera_discovery::TxtRecord) -> bool {
    let mut a = a.to_strings();
    let mut b = b.to_strings();
    a.sort();
    b.sort();
    a == b
}
fn channel_txt(txt: &tessera_discovery::TxtRecord) -> Vec<Vec<u8>> {
    let mut strings = txt.to_strings();
    strings.retain(|s| !s.eq_ignore_ascii_case(b"default"));
    strings.sort();
    strings
}
fn make_cursor(scope: &str, revision: &str, next: usize) -> RoutingCursor {
    RoutingCursor(format!("{scope}/{revision}/{next}"))
}
fn cursor_start(
    cursor: Option<&RoutingCursor>,
    scope: &str,
    revision: &str,
    len: usize,
) -> Result<usize, CommandError> {
    let Some(RoutingCursor(value)) = cursor else {
        return Ok(0);
    };
    if value.len() > 256 {
        return Err(CommandError::RoutingSnapshotExpired);
    }
    let mut parts = value.split('/');
    let (Some(s), Some(r), Some(n), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(CommandError::RoutingSnapshotExpired);
    };
    if s != scope || r != revision {
        return Err(CommandError::RoutingSnapshotExpired);
    }
    n.parse()
        .ok()
        .filter(|n: &usize| *n > 0 && *n < len)
        .ok_or(CommandError::RoutingSnapshotExpired)
}
fn wire_len(reply: Reply) -> Result<usize, CommandError> {
    serde_json::to_vec(&Response {
        v: WIRE_VERSION,
        id: u64::MAX,
        result: Ok(reply),
    })
    .map(|bytes| bytes.len())
    .map_err(|_| CommandError::Internal {
        message: "cannot encode routing inventory".into(),
    })
}
fn page_end<T: serde::Serialize>(
    rows: &[T],
    start: usize,
    mut size: usize,
    max: usize,
) -> Result<usize, CommandError> {
    let mut end = start;
    for row in &rows[start..] {
        let bytes = serde_json::to_vec(row).map_err(|_| CommandError::Internal {
            message: "cannot encode routing record".into(),
        })?;
        let candidate = size.saturating_add(bytes.len()).saturating_add(1);
        if candidate > max {
            if end == start {
                return Err(CommandError::ResponseTooLarge {
                    size: candidate as u64,
                    limit: max as u64,
                    key: "control.max_frame_bytes".into(),
                });
            }
            break;
        }
        size = candidate;
        end += 1;
    }
    Ok(end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tessera_discovery::{ServiceInstance, TxtRecord};
    use tessera_state::{ConfigPaths, Environment, ManualClock, Paths};

    struct Fixture {
        engine: StateEngine,
        path: std::path::PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            Self::with_config("[[device]]\nid='main'\nname='Local'\nrx_channels=2\ntx_channels=3\n")
        }
        fn with_config(config: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "routing-inventory-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(path.join("config.d")).unwrap();
            std::fs::write(path.join("config.toml"), config).unwrap();
            let paths = Paths::new(
                ConfigPaths::new(path.join("config.toml"), path.join("config.d")),
                path.join("state.toml"),
            );
            let (engine, _) = StateEngine::start(
                Environment::new("test"),
                paths,
                Arc::new(ManualClock::new()),
            )
            .unwrap();
            Self { engine, path }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
    fn observation(
        kind: ServiceKind,
        instance: &str,
        source: &str,
        txt: TxtRecord,
    ) -> ServiceObservation {
        // External non-exhaustive observation is supplied through the public
        // constructor, just as backends expose it to the daemon.
        ServiceObservation::new(
            source,
            Some("eth0".into()),
            ServiceInstance {
                id: ServiceId::new(kind, instance),
                host: "stage.local.".into(),
                port: 4455,
                addresses: vec!["192.0.2.10".parse().unwrap()],
                txt,
            },
        )
    }
    fn snapshot(observations: Vec<ServiceObservation>) -> DiscoverySnapshot {
        let mut snapshot = DiscoverySnapshot::empty(RoutingDiscoveryState::Browsing);
        snapshot.permitted_interfaces.push("eth0".into());
        snapshot.observations = observations;
        snapshot
    }

    fn receiver_fixture() -> (Fixture, Inventory, EndpointRef) {
        let mut fixture = Fixture::new();
        let mut network = tessera_control_types::NetworkStatus::new(
            Some("eth0".into()),
            tessera_control_types::PrimarySource::Automatic,
        );
        let mut interface = tessera_control_types::InterfaceStatus::new("eth0");
        interface.ipv4 = Some("192.0.2.1".into());
        interface.usable = true;
        interface.carrier = true;
        network.interfaces.push(interface);
        fixture.engine.report_network_status(network);
        let arc = observation(
            ServiceKind::Arc,
            "Stage",
            "source",
            TxtRecord::new().with("arcp_vers", "2.7.41").unwrap(),
        );
        let mut inventory = Inventory::new();
        inventory.report(snapshot(vec![arc]), &fixture.engine);
        let endpoint = devices(&mut inventory, &fixture.engine)
            .devices
            .into_iter()
            .find(|d| d.local_device.is_none())
            .unwrap()
            .endpoint;
        (fixture, inventory, endpoint)
    }

    #[test]
    fn receiver_targets_require_current_profile_interface_and_one_unicast_address() {
        let (fixture, mut inventory, endpoint) = receiver_fixture();
        let target = inventory
            .receiver_target(&fixture.engine, &endpoint)
            .unwrap();
        assert_eq!(target.source.to_string(), "192.0.2.1");
        assert_eq!(target.peer.to_string(), "192.0.2.10:4455");
        assert_eq!(target.interface, "eth0");
        let mut changed = inventory.snapshot.clone();
        changed.observations[0].service.txt = TxtRecord::new().with("arcp_vers", "4.0.0").unwrap();
        inventory.report(changed, &fixture.engine);
        assert_eq!(
            inventory
                .receiver_target(&fixture.engine, &endpoint)
                .unwrap_err(),
            CommandError::RoutingSnapshotExpired
        );
        let endpoint = devices(&mut inventory, &fixture.engine)
            .devices
            .into_iter()
            .find(|d| d.local_device.is_none())
            .unwrap()
            .endpoint;
        assert!(matches!(
            inventory.receiver_target(&fixture.engine, &endpoint),
            Err(CommandError::Unsupported { .. })
        ));

        for addresses in [
            vec!["127.0.0.1"],
            vec!["224.0.0.1"],
            vec!["192.0.2.10", "192.0.2.11"],
        ] {
            let mut changed = inventory.snapshot.clone();
            changed.observations[0].service.txt =
                TxtRecord::new().with("arcp_vers", "2.7.41").unwrap();
            changed.observations[0].service.addresses =
                addresses.into_iter().map(|s| s.parse().unwrap()).collect();
            inventory.report(changed, &fixture.engine);
            let endpoint = devices(&mut inventory, &fixture.engine)
                .devices
                .into_iter()
                .find(|d| d.local_device.is_none())
                .unwrap()
                .endpoint;
            assert!(matches!(
                inventory.receiver_target(&fixture.engine, &endpoint),
                Err(CommandError::Unsupported { .. })
            ));
        }
    }

    #[test]
    fn receiver_completion_revalidates_binding_and_limits_without_changing_intent() {
        let (mut fixture, mut inventory, endpoint) = receiver_fixture();
        let target = inventory
            .receiver_target(&fixture.engine, &endpoint)
            .unwrap();
        let before = fixture.engine.effective_config().unwrap();
        let page = crate::routing_receiver::PreparedPage::checked(
            tessera_control_types::RoutingReceiverPage::new(endpoint, "Stage".into(), 1, 0),
        );
        assert!(matches!(
            inventory.receiver_result(&fixture.engine, &target, page.clone(), 1),
            Err(CommandError::ResponseTooLarge { .. })
        ));
        assert!(matches!(
            inventory.receiver_result(&fixture.engine, &target, page.clone(), 65536),
            Ok(Reply::RoutingReceiver(_))
        ));
        let mut network = fixture.engine.network_status().unwrap().clone();
        network.interfaces[0].ipv4 = Some("192.0.2.3".into());
        fixture.engine.report_network_status(network);
        assert_eq!(
            inventory.receiver_result(&fixture.engine, &target, page, 65536),
            Err(CommandError::RoutingSnapshotExpired)
        );
        assert_eq!(fixture.engine.effective_config().unwrap(), before);
    }

    #[test]
    fn receiver_preparation_refuses_incomplete_discovery_and_disallowed_interfaces() {
        for disallowed in [false, true] {
            let (fixture, mut inventory, endpoint) = receiver_fixture();
            let mut changed = inventory.snapshot.clone();
            if disallowed {
                changed.permitted_interfaces.clear();
            } else {
                changed.health.state = RoutingDiscoveryState::Failed;
            }
            inventory.report(changed, &fixture.engine);
            assert!(
                inventory
                    .receiver_target(&fixture.engine, &endpoint)
                    .is_err()
            );
        }
    }
    fn devices(inventory: &mut Inventory, engine: &StateEngine) -> RoutingDevicePage {
        match inventory
            .query(
                engine,
                Query::RoutingDevices(RoutingDevicesQuery::default()),
                65536,
            )
            .unwrap()
        {
            Reply::RoutingDevices(page) => page,
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn local_asymmetric_channels_keep_one_based_direction_and_bounded_pages() {
        let f = Fixture::with_config(
            "[[device]]\nid='main'\nname='Local'\nrx_channels=513\ntx_channels=65535\n\
             [device.rx_channel_labels]\n'513'='Last input'\n\
             [device.tx_channel_labels]\n'65535'='Last output'\n",
        );
        let mut inventory = Inventory::new();
        let device = devices(&mut inventory, &f.engine).devices.remove(0);
        assert_eq!(device.rx_channels, Some(513));
        assert_eq!(device.tx_channels, Some(65535));
        let mut query = RoutingChannelsQuery::new(device.endpoint.clone(), Direction::Rx, None);
        let mut ids = Vec::new();
        loop {
            let Reply::RoutingChannels(page) = inventory
                .query(&f.engine, Query::RoutingChannels(query.clone()), 65536)
                .unwrap()
            else {
                panic!()
            };
            assert!(page.complete);
            assert!(wire_len(Reply::RoutingChannels(page.clone())).unwrap() <= 65536);
            assert!(!page.channels.is_empty());
            for row in &page.channels {
                assert_eq!(row.direction, Direction::Rx);
                assert_eq!(row.endpoint, device.endpoint);
                assert_eq!(u32::from(row.local_channel.unwrap().get()), row.id);
                ids.push(row.id);
            }
            if page.next_cursor.is_none() {
                assert_eq!(page.channels.last().unwrap().label, "Last input");
                break;
            }
            query.cursor = page.next_cursor;
        }
        assert_eq!(ids, (1..=513).collect::<Vec<_>>());
        let tx = RoutingChannelsQuery::new(device.endpoint.clone(), Direction::Tx, None);
        let Reply::RoutingChannels(first) = inventory
            .query(&f.engine, Query::RoutingChannels(tx.clone()), 65536)
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(first.channels[0].id, 1);
        assert_eq!(first.channels[0].label, "TX 1");
        assert!(first.next_cursor.is_some());
        let mut last = tx.clone();
        last.cursor = Some(make_cursor("tx", &device.endpoint.generation, 65534));
        let Reply::RoutingChannels(page) = inventory
            .query(&f.engine, Query::RoutingChannels(last), 65536)
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(page.channels.len(), 1);
        assert_eq!(page.channels[0].id, 65535);
        assert_eq!(page.channels[0].direction, Direction::Tx);
        assert_eq!(page.channels[0].label, "Last output");
        assert!(page.next_cursor.is_none());
        let mut cross_direction = tx;
        cross_direction.cursor = Some(make_cursor("rx", &device.endpoint.generation, 1));
        assert!(matches!(
            inventory.query(&f.engine, Query::RoutingChannels(cross_direction), 65536),
            Err(CommandError::RoutingSnapshotExpired)
        ));
    }

    #[test]
    fn mapping_name_ambiguity_is_reported_across_endpoints_and_output_ids() {
        let f = Fixture::with_config(
            "[[device]]\nid='main'\nname='Local'\nrx_channels=1\ntx_channels=3\n[device.tx_channel_labels]\n'1'='TX 2'\n",
        );
        let mut inventory = Inventory::new();
        assert!(
            inventory.namespace.is_empty(),
            "unused inventory requires no entropy"
        );
        let a = observation(
            ServiceKind::Chan,
            "Vocal@Stage",
            "s1",
            TxtRecord::new().with("id", "1").unwrap(),
        );
        let b = observation(
            ServiceKind::Chan,
            "Vocal@Stage",
            "s2",
            TxtRecord::new().with("id", "2").unwrap(),
        );
        inventory.report(snapshot(vec![a.clone(), b]), &f.engine);
        assert!(
            inventory.namespace.is_empty(),
            "reports alone do not initialize query identity"
        );
        let page = devices(&mut inventory, &f.engine);
        assert!(
            page.devices
                .iter()
                .filter(|d| d.local_device.is_none())
                .all(|d| d.unavailable.contains(&Reason::ConflictingMetadata))
        );
        let local = page
            .devices
            .iter()
            .find(|d| d.local_device.is_some())
            .unwrap();
        let Reply::RoutingChannels(channels) = inventory
            .query(
                &f.engine,
                Query::RoutingChannels(RoutingChannelsQuery::new(
                    local.endpoint.clone(),
                    Direction::Tx,
                    None,
                )),
                65536,
            )
            .unwrap()
        else {
            panic!()
        };
        assert!(
            channels.channels[0]
                .unavailable
                .contains(&Reason::ConflictingMetadata)
        );
        assert!(
            channels.channels[1]
                .unavailable
                .contains(&Reason::ConflictingMetadata)
        );
        assert!(
            !channels.channels[2]
                .unavailable
                .contains(&Reason::ConflictingMetadata)
        );
        let b = observation(
            ServiceKind::Chan,
            "Vocal@Stage",
            "s1",
            TxtRecord::new().with("id", "2").unwrap(),
        );
        // Different output IDs with equal names on one endpoint cannot be
        // selected safely by the name-only legacy command either.
        inventory.report(snapshot(vec![a, b]), &f.engine);
        let remote = devices(&mut inventory, &f.engine)
            .devices
            .into_iter()
            .find(|d| d.local_device.is_none())
            .unwrap();
        let Reply::RoutingChannels(channels) = inventory
            .query(
                &f.engine,
                Query::RoutingChannels(RoutingChannelsQuery::new(
                    remote.endpoint,
                    Direction::Tx,
                    None,
                )),
                65536,
            )
            .unwrap()
        else {
            panic!()
        };
        assert!(
            channels
                .channels
                .iter()
                .all(|c| c.unavailable.contains(&Reason::ConflictingMetadata))
        );
    }
    #[test]
    fn single_source_metadata_changes_invalidate_observation_references() {
        let f = Fixture::new();
        let mut inventory = Inventory::new();
        let mut observed = observation(
            ServiceKind::Cmc,
            "Stage",
            "s1",
            TxtRecord::new().with("id", "original").unwrap(),
        );
        inventory.report(snapshot(vec![observed.clone()]), &f.engine);
        let mut previous = devices(&mut inventory, &f.engine)
            .devices
            .into_iter()
            .find(|d| d.local_device.is_none())
            .unwrap()
            .endpoint;
        for change in 0..3 {
            match change {
                0 => observed.service.txt = TxtRecord::new().with("id", "replacement").unwrap(),
                1 => observed.service.port += 1,
                _ => observed.service.addresses = vec!["192.0.2.20".parse().unwrap()],
            }
            inventory.report(snapshot(vec![observed.clone()]), &f.engine);
            let next = devices(&mut inventory, &f.engine)
                .devices
                .into_iter()
                .find(|d| d.local_device.is_none())
                .unwrap()
                .endpoint;
            assert_eq!(next.id, previous.id);
            assert_ne!(next.generation, previous.generation);
            assert!(matches!(
                inventory.query(
                    &f.engine,
                    Query::RoutingChannels(RoutingChannelsQuery::new(
                        previous,
                        Direction::Tx,
                        None
                    )),
                    65536
                ),
                Err(CommandError::RoutingSnapshotExpired)
            ));
            previous = next;
        }
    }
    #[test]
    fn replacing_a_browser_expires_remote_refs_without_invalidating_local_refs() {
        let f = Fixture::new();
        let mut inventory = Inventory::new();
        let mut observed = snapshot(vec![observation(
            ServiceKind::Chan,
            "Microphone@Stage",
            "s1",
            TxtRecord::new().with("id", "1").unwrap(),
        )]);
        observed.browser_epoch = 1;
        inventory.report(observed.clone(), &f.engine);
        let before = devices(&mut inventory, &f.engine);
        let remote = before
            .devices
            .iter()
            .find(|d| d.local_device.is_none())
            .unwrap();
        let local = before
            .devices
            .iter()
            .find(|d| d.local_device.is_some())
            .unwrap();
        let remote_query = RoutingChannelsQuery::new(remote.endpoint.clone(), Direction::Tx, None);
        let local_query = RoutingChannelsQuery::new(local.endpoint.clone(), Direction::Rx, None);
        // A replacement browser can assign the same source incarnation and
        // observe identical records before the next full snapshot arrives.
        observed.browser_epoch += 1;
        inventory.report(observed.clone(), &f.engine);
        assert!(matches!(
            inventory.query(&f.engine, Query::RoutingChannels(remote_query), 65536),
            Err(CommandError::RoutingSnapshotExpired)
        ));
        assert!(
            inventory
                .query(&f.engine, Query::RoutingChannels(local_query), 65536)
                .is_ok()
        );
        let after = devices(&mut inventory, &f.engine);
        assert_ne!(before.snapshot, after.snapshot);
        inventory.report(observed, &f.engine);
        assert_eq!(devices(&mut inventory, &f.engine).snapshot, after.snapshot);
    }

    #[test]
    fn advertised_source_aliases_remain_inventory_labels() {
        let f = Fixture::with_config(
            "[[device]]\nid='main'\nname='Local'\nrx_channels=2\ntx_channels=0\n\
             [control]\nrouting_sources=['0123456789abcdef']\n",
        );
        let txt = TxtRecord::new()
            .with("id", "2")
            .unwrap()
            .with("rate", "48000")
            .unwrap()
            .with("enc", "24")
            .unwrap();
        let mut factory_txt = txt.clone();
        factory_txt.push_flag("default").unwrap();
        let factory = observation(ServiceKind::Chan, "TX 2@Stage", "s1", factory_txt);
        let friendly = observation(ServiceKind::Chan, "2@Stage", "s1", txt);
        let cmc = observation(
            ServiceKind::Cmc,
            "Stage",
            "s1",
            TxtRecord::new().with("id", "0123456789abcdef").unwrap(),
        );
        let mut inventory = Inventory::new();
        inventory.report(
            snapshot(vec![cmc.clone(), factory.clone(), friendly.clone()]),
            &f.engine,
        );
        inventory.refresh(&f.engine);
        assert_eq!(
            inventory
                .entries
                .values()
                .flat_map(|e| &e.channels)
                .find(|c| c.id == 2)
                .unwrap()
                .aliases,
            ["2", "TX 2"]
        );
        // Labels remain observations; they do not identify an existing source
        // session or authorize its teardown.
    }

    #[test]
    fn remote_aliases_u32_ids_unknown_rx_and_stale_generations() {
        let f = Fixture::new();
        let mut inventory = Inventory::new();
        let txt = TxtRecord::new()
            .with("id", "70000")
            .unwrap()
            .with("rate", "48000")
            .unwrap();
        let factory = observation(ServiceKind::Chan, "TX 1@Stage", "s1", {
            let mut t = txt.clone();
            t.push_flag("default").unwrap();
            t
        });
        let friendly = observation(ServiceKind::Chan, "Microphone@Stage", "s1", txt);
        inventory.report(snapshot(vec![factory.clone(), friendly.clone()]), &f.engine);
        let page = devices(&mut inventory, &f.engine);
        assert!(page.complete);
        let remote = page
            .devices
            .iter()
            .find(|d| d.local_device.is_none())
            .unwrap();
        assert_eq!(remote.label, "Stage");
        assert_eq!(remote.rx_channels, None);
        assert_eq!(remote.tx_channels, None);
        let endpoint = remote.endpoint.clone();
        let q = RoutingChannelsQuery::new(endpoint.clone(), Direction::Tx, None);
        let Reply::RoutingChannels(channels) = inventory
            .query(&f.engine, Query::RoutingChannels(q.clone()), 65536)
            .unwrap()
        else {
            panic!()
        };
        assert!(!channels.complete);
        assert_eq!(channels.channels.len(), 1);
        assert_eq!(channels.channels[0].id, 70000);
        assert_eq!(channels.channels[0].label, "Microphone");
        assert_eq!(channels.channels[0].local_channel, None);
        inventory.report(snapshot(vec![factory.clone(), friendly.clone()]), &f.engine);
        assert_eq!(
            devices(&mut inventory, &f.engine).snapshot,
            page.snapshot,
            "TTL refresh is stable"
        );
        let mut changed = friendly;
        changed.service.port += 1;
        inventory.report(snapshot(vec![factory, changed]), &f.engine);
        assert!(matches!(
            inventory.query(&f.engine, Query::RoutingChannels(q), 65536),
            Err(CommandError::RoutingSnapshotExpired)
        ));
        let mut second = Inventory::new();
        assert!(matches!(
            second.query(
                &f.engine,
                Query::RoutingChannels(RoutingChannelsQuery::new(endpoint, Direction::Tx, None)),
                65536
            ),
            Err(CommandError::RoutingSnapshotExpired)
        ));
    }
    #[test]
    fn paging_respects_small_connection_frames_and_scope() {
        let f = Fixture::new();
        let mut inventory = Inventory::new();
        let observations = (0..400)
            .map(|n| {
                observation(
                    ServiceKind::Arc,
                    &format!("device-{n:04}"),
                    "s",
                    TxtRecord::new(),
                )
            })
            .collect();
        inventory.report(snapshot(observations), &f.engine);
        let first = devices(&mut inventory, &f.engine);
        assert!(first.next_cursor.is_some());
        assert!(wire_len(Reply::RoutingDevices(first.clone())).unwrap() <= 65536);
        let mut seen: Vec<_> = first
            .devices
            .iter()
            .map(|d| d.endpoint.id.clone())
            .collect();
        let mut cursor = first.next_cursor;
        while let Some(next) = cursor {
            let Reply::RoutingDevices(page) = inventory
                .query(
                    &f.engine,
                    Query::RoutingDevices(RoutingDevicesQuery::new(Some(next))),
                    65536,
                )
                .unwrap()
            else {
                panic!()
            };
            assert!(!page.devices.is_empty());
            assert!(wire_len(Reply::RoutingDevices(page.clone())).unwrap() <= 65536);
            seen.extend(page.devices.iter().map(|d| d.endpoint.id.clone()));
            cursor = page.next_cursor;
        }
        assert_eq!(seen.len(), 401);
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 401);
        for bad in [
            "x".repeat(257),
            format!("tx/{}/1", first.snapshot),
            format!("devices/{}/0", first.snapshot),
        ] {
            assert!(matches!(
                inventory.query(
                    &f.engine,
                    Query::RoutingDevices(RoutingDevicesQuery::new(Some(RoutingCursor(bad)))),
                    65536
                ),
                Err(CommandError::RoutingSnapshotExpired)
            ));
        }
        let old = make_cursor("devices", &first.snapshot, 1);
        let mut changed = snapshot(Vec::new());
        changed.health.lost_events = 1;
        inventory.report(changed, &f.engine);
        assert!(devices(&mut inventory, &f.engine).complete);
        assert!(matches!(
            inventory.query(
                &f.engine,
                Query::RoutingDevices(RoutingDevicesQuery::new(Some(old))),
                65536
            ),
            Err(CommandError::RoutingSnapshotExpired)
        ));
    }
    #[test]
    fn lifecycle_and_reconciled_loss_have_independent_revisions() {
        let f = Fixture::new();
        let mut inventory = Inventory::new();
        let mut observed = observation(ServiceKind::Arc, "Stage", "s1", TxtRecord::new());
        observed.incarnation = 1;
        let snap = snapshot(vec![observed.clone()]);
        inventory.report(snap.clone(), &f.engine);
        let first = devices(&mut inventory, &f.engine);
        let endpoint = first
            .devices
            .iter()
            .find(|d| d.local_device.is_none())
            .unwrap()
            .endpoint
            .clone();
        // Full cache reconciliation repairs notification loss. Lifetime
        // diagnostic counters neither poison completeness nor expire pages.
        let mut reconciled = snap;
        reconciled.health.lost_events = 100;
        reconciled.health.resolve_failures = 2;
        reconciled.health.dropped = 5;
        inventory.report(reconciled, &f.engine);
        let next = devices(&mut inventory, &f.engine);
        assert!(next.complete);
        assert_eq!(next.snapshot, first.snapshot);
        assert_eq!(inventory.rebuilds, 1);
        // Removed and identically restored between reports: only the
        // cache's retained incarnation exposes this lifecycle transition.
        observed.incarnation = 2;
        inventory.report(snapshot(vec![observed]), &f.engine);
        assert!(matches!(
            inventory.query(
                &f.engine,
                Query::RoutingChannels(RoutingChannelsQuery::new(endpoint, Direction::Tx, None)),
                65536
            ),
            Err(CommandError::RoutingSnapshotExpired)
        ));
        let mut outage = snapshot(Vec::new());
        outage.health.state = RoutingDiscoveryState::Failed;
        inventory.report(outage, &f.engine);
        assert!(!devices(&mut inventory, &f.engine).complete);
    }

    #[test]
    fn large_catalogue_pages_do_not_repeat_reconciliation_or_block_legacy_intent() {
        let mut f = Fixture::new();
        let mut inventory = Inventory::new();
        let mut snap = snapshot(Vec::new());
        // Thousands of own records and remote endpoints previously caused
        // a cross-product scan on every page on the authority thread.
        for i in 1..=2048 {
            let own = observation(
                ServiceKind::Chan,
                &format!("TX {i}@Local"),
                "s1",
                TxtRecord::new().with("id", i.to_string()).unwrap(),
            );
            snap.own.push(OwnAdvertisement {
                id: own.service.id.clone(),
                host: own.service.host.clone(),
                port: own.service.port,
                addresses: own.service.addresses.clone(),
                txt: own.service.txt.clone(),
                device: DeviceKey("main".into()),
            });
            snap.observations.push(own);
            snap.observations.push(observation(
                ServiceKind::Arc,
                &format!("Remote {i:04}"),
                "s1",
                TxtRecord::new(),
            ));
        }
        snap.health.lost_events = 4096;
        inventory.report(snap, &f.engine);
        let mut cursor = None;
        let mut seen = 0;
        loop {
            let Reply::RoutingDevices(page) = inventory
                .query(
                    &f.engine,
                    Query::RoutingDevices(RoutingDevicesQuery::new(cursor)),
                    65536,
                )
                .unwrap()
            else {
                panic!()
            };
            assert!(page.complete);
            seen += page.devices.len();
            // Legacy writes remain admitted between pages; saved intent is
            // not inventory identity and cannot trigger another rebuild.
            f.engine
                .apply(
                    tessera_control_types::Origin::Internal,
                    tessera_control_types::Command::Subscribe {
                        device: DeviceKey("main".into()),
                        rx_channel: tessera_control_types::Channel::new(1).unwrap(),
                        tx_channel: "Output".into(),
                        tx_device: "Existing peer".into(),
                    },
                )
                .unwrap();
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(
            seen, 2049,
            "all local advertisements link to the existing local device"
        );
        assert_eq!(inventory.rebuilds, 1);
    }

    #[test]
    fn own_channel_aliases_link_by_published_metadata_without_hiding_conflicts() {
        let f = Fixture::new();
        let factory = observation(
            ServiceKind::Chan,
            "TX 1@Local",
            "s1",
            TxtRecord::new()
                .with("id", "1")
                .unwrap()
                .with("rate", "48000")
                .unwrap(),
        );
        let mut alias = factory.clone();
        alias.service.id = ServiceId::new(ServiceKind::Chan, "1@Local");
        alias.service.txt.push_flag("default").unwrap();
        let cmc = observation(
            ServiceKind::Cmc,
            "Local",
            "s1",
            TxtRecord::new()
                .with("id", "0123456789abcdef")
                .unwrap()
                .with("process", "0")
                .unwrap(),
        );
        let mut baseline = snapshot(vec![factory.clone(), alias.clone(), cmc.clone()]);
        for observed in [&factory, &cmc] {
            baseline.own.push(OwnAdvertisement {
                id: observed.service.id.clone(),
                host: observed.service.host.clone(),
                port: observed.service.port,
                addresses: observed.service.addresses.clone(),
                txt: observed.service.txt.clone(),
                device: DeviceKey("main".into()),
            });
        }
        // DNS-SD TXT attributes retain meaning when Avahi returns a different
        // attribute order; identity and every value must still match.
        baseline.observations[2].service.txt = TxtRecord::new()
            .with("process", "0")
            .unwrap()
            .with("id", "0123456789abcdef")
            .unwrap();
        let mut inventory = Inventory::new();
        inventory.report(baseline.clone(), &f.engine);
        assert_eq!(devices(&mut inventory, &f.engine).devices.len(), 1);
        for change in 0..4 {
            let mut conflicting = baseline.clone();
            match change {
                0 => {
                    conflicting.observations[1].service.addresses =
                        vec!["192.0.2.11".parse().unwrap()]
                }
                1 => conflicting.observations[1].service.port += 1,
                2 => {
                    conflicting.observations[1].service.txt =
                        TxtRecord::new().with("id", "2").unwrap()
                }
                _ => {
                    conflicting.observations[2].service.txt =
                        TxtRecord::new().with("id", "another").unwrap()
                }
            }
            inventory.report(conflicting, &f.engine);
            assert_eq!(
                devices(&mut inventory, &f.engine).devices.len(),
                2,
                "conflicting channel or CMC metadata must remain visible"
            );
        }
    }

    #[test]
    fn duplicate_names_provenance_and_own_advertisements_do_not_hide_peers() {
        let f = Fixture::new();
        let mut inventory = Inventory::new();
        let a = observation(
            ServiceKind::Cmc,
            "Stage",
            "s1",
            TxtRecord::new().with("id", "a").unwrap(),
        );
        let mut b = a.clone();
        b.source = "s2".into();
        b.interface = None;
        b.service.txt = TxtRecord::new().with("id", "b").unwrap();
        let mut snap = snapshot(vec![a.clone(), b]);
        snap.own.push(OwnAdvertisement {
            id: a.service.id.clone(),
            host: a.service.host.clone(),
            port: a.service.port,
            addresses: vec!["192.0.2.99".parse().unwrap()],
            txt: a.service.txt.clone(),
            device: DeviceKey("main".into()),
        });
        inventory.report(snap.clone(), &f.engine);
        let page = devices(&mut inventory, &f.engine);
        assert_eq!(
            page.devices.len(),
            3,
            "a same-named peer at another address stays visible"
        );
        assert!(
            page.devices
                .iter()
                .filter(|d| d.local_device.is_none())
                .all(|d| d.unavailable.contains(&Reason::ConflictingMetadata))
        );
        assert!(
            page.devices
                .iter()
                .any(|d| d.unavailable.contains(&Reason::InterfaceUnknown))
        );
        snap.own[0].addresses = a.service.addresses;
        inventory.report(snap, &f.engine);
        assert_eq!(
            devices(&mut inventory, &f.engine).devices.len(),
            2,
            "a conflicting CMC identity is not hidden as our own advertisement"
        );
    }
}
