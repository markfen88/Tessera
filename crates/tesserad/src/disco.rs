//! mDNS/DNS-SD advertisement of the logical devices (`tessera-discovery`).
//!
//! [`DiscoveryManager::sync`] makes the set of registered devices equal to the
//! wanted set: new devices are registered, changed ones updated (rename, rate,
//! channel labels), removed ones withdrawn; [`DiscoveryManager::shutdown`]
//! withdraws everything (goodbye packets) before the daemon exits. The
//! backend is chosen by `[discovery] mdns` (`auto` / `avahi` / `builtin`;
//! `off` starts nothing). Failing to start discovery is never fatal: it is
//! logged once and retried on the next sync.
//!
//! **PROVISIONAL**: the records and their ports come from
//! `spec/protocol/discovery-mdns.md` (`likely`/`guess`); nothing listens on the
//! advertised ARC/CMC ports until those protocols exist, so a controller that
//! resolves a tessera device cannot talk to it yet. The advertisement exists so
//! that the registration, rename and withdraw paths are exercised end to end.
//! While that is so, [`DiscoveryManager`] warns once per advertised control
//! port that no UDP socket on the host is bound to (a warning only: a separate
//! responder may answer for the daemon).

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use crate::routing_inventory::{DiscoverySnapshot, OwnAdvertisement};
use tessera_control_types::{DeviceKey, RoutingDiscoveryState};
use tessera_discovery::advert::{ArcAdvert, BundleAdvert, ChannelAdvert, CmcAdvert};
use tessera_discovery::provisional::{DEFAULT_ARC_PORT, DEFAULT_CMC_PORT, DEFAULT_FLOW_PORT};
use tessera_discovery::{
    BrowseReadiness, Browser, DeviceServices, Discovery, DiscoveryConfig, Error as DiscError, Mode,
    Responder, ResponderEvent, ServiceKind, ServiceSpec,
};
use tessera_state::config::MdnsMode;
use tessera_state::model::DeviceModel;

use crate::netif;

/// Retry period after a failed start.
const RETRY: Duration = Duration::from_secs(10);

/// What to advertise for one device (plain data).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAdvert {
    /// Device key.
    pub key: String,
    /// Device name (the mDNS host name is derived from it).
    pub name: String,
    /// Wire device id.
    pub wire_id: [u8; 8],
    /// Sample rate.
    pub rate: u32,
    /// Wire bits per sample.
    pub bits: u8,
    /// Latency, nanoseconds.
    pub latency_ns: u64,
    /// Effective TX channel labels, channel 1 first.
    pub tx_labels: Vec<String>,
    /// Port offset so that several devices do not advertise one port.
    pub index: u16,
    /// Manual multicast transmit flows (bundles, spec 3.4).
    pub bundles: Vec<Bundle>,
}

/// One manual multicast transmit flow as advertised (`discovery-mdns.md`
/// §§3.3-3.4): its id, the TX channels it carries in wire order, the group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    /// Flow (bundle) id, chosen by the controller that created it.
    pub id: u32,
    /// TX channels carried, in wire order (1-based).
    pub channels: Vec<u16>,
    /// Multicast group.
    pub addr: Ipv4Addr,
    /// Destination port.
    pub port: u16,
    /// Frames per packet.
    pub fpp: u16,
}

impl DeviceAdvert {
    /// Derive the advertisement from the effective device model.
    #[must_use]
    pub fn of(d: &DeviceModel, index: usize) -> Self {
        Self {
            key: d.key.0.clone(),
            name: d.name.clone(),
            wire_id: d.wire_device_id.0,
            rate: d.sample_rate,
            bits: d.encoding,
            latency_ns: u64::from(d.latency_us) * 1_000,
            tx_labels: (1..=d.tx_channels).map(|c| d.tx_label(c)).collect(),
            index: u16::try_from(index).unwrap_or(u16::MAX),
            bundles: Vec::new(),
        }
    }

    /// Add the device's manual multicast flows: the daemon's own multicast
    /// transmit flows of this device in the effective flow set.
    #[must_use]
    pub fn with_bundles(mut self, flows: &[tessera_state::config::FlowConfig]) -> Self {
        self.bundles = flows
            .iter()
            .filter(|f| f.device == self.key && f.direction == tessera_control_types::Direction::Tx)
            .filter_map(|f| {
                let id =
                    f.id.strip_prefix(crate::native::MULTICAST_PREFIX)?
                        .parse()
                        .ok()?;
                let addr = f.peer.filter(Ipv4Addr::is_multicast)?;
                Some(Bundle {
                    id,
                    channels: f.channels.clone(),
                    addr,
                    port: f.port,
                    fpp: f
                        .frames_per_packet
                        .and_then(|v| u16::try_from(v).ok())
                        .unwrap_or(32),
                })
            })
            .collect();
        self
    }
}

/// The DNS host label for a device name: letters, digits and `-`; anything
/// else becomes `-`.
#[must_use]
pub fn hostname_of(name: &str) -> String {
    let h: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(31)
        .collect();
    if h.is_empty() {
        "tessera".to_owned()
    } else {
        h
    }
}

/// The services of one device.
///
/// # Errors
/// A TXT record or name that the discovery layer rejects, as text.
pub fn services_for(d: &DeviceAdvert) -> Result<DeviceServices, String> {
    let host = hostname_of(&d.name);
    let off = d.index.saturating_mul(10);
    let arc = ArcAdvert::default();
    let cmc = CmcAdvert {
        id: d.wire_id,
        ..CmcAdvert::default()
    };
    let mut services: Vec<ServiceSpec> = vec![
        arc.service(&host, DEFAULT_ARC_PORT.saturating_add(off))
            .map_err(|e| e.to_string())?,
        cmc.service(&host, DEFAULT_CMC_PORT.saturating_add(off))
            .map_err(|e| e.to_string())?,
    ];
    let nchan = u16::try_from(d.tx_labels.len().min(8)).unwrap_or(8);
    let mut used = std::collections::HashSet::new();
    for (i, label) in d.tx_labels.iter().enumerate() {
        let id = u32::try_from(i + 1).unwrap_or(u32::MAX);
        // The same factory name ARC 0x2000 reports (`control-arc.md` §4.5).
        let factory = crate::native::wire::tx_factory_name(u16::try_from(id).unwrap_or(u16::MAX));
        // A renamed channel needs two records (spec 3.3): the factory name
        // flagged `default`, and the friendly name.
        let mut names = vec![(factory.clone(), true)];
        if *label != factory {
            names.push((label.clone(), false));
        }
        for (name, default) in names {
            let mut name = name;
            if !used.insert(format!("{}@{host}", name.to_ascii_lowercase())) {
                name = format!("{name} {id}");
                used.insert(format!("{}@{host}", name.to_ascii_lowercase()));
            }
            let ch = ChannelAdvert {
                name,
                id,
                rate: d.rate,
                bytes_per_sample: d.bits / 8,
                bits: d.bits,
                latency_ns: d.latency_ns,
                nchan,
                default,
                bundles: d
                    .bundles
                    .iter()
                    .filter_map(|b| {
                        let pos = b.channels.iter().position(|c| u32::from(*c) == id)?;
                        Some((b.id, u16::try_from(pos + 1).ok()?))
                    })
                    .collect(),
                ..ChannelAdvert::default()
            };
            services.push(
                ch.service(&host, DEFAULT_FLOW_PORT.saturating_add(off))
                    .map_err(|e| e.to_string())?,
            );
        }
    }
    for b in &d.bundles {
        let bundle = BundleAdvert {
            id: b.id,
            nchan: u16::try_from(b.channels.len()).unwrap_or(u16::MAX),
            latency_ns: d.latency_ns,
            fpp: b.fpp,
            rate: d.rate,
            bits: d.bits,
            addr: b.addr,
            port: b.port,
        };
        services.push(
            bundle
                .service(&host, DEFAULT_FLOW_PORT.saturating_add(off))
                .map_err(|e| e.to_string())?,
        );
    }
    Ok(DeviceServices {
        device: d.key.clone(),
        hostname: host,
        addresses: Vec::new(),
        services,
    })
}

/// What the manager is asked to keep true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryPlan {
    /// Browse without changing the advertisement/control path.
    pub browse: bool,
    /// `[discovery] mdns`.
    pub mode: MdnsMode,
    /// Interfaces to advertise on (empty: the discovery layer's default).
    pub interfaces: Vec<String>,
    /// Devices to advertise.
    pub devices: Vec<DeviceAdvert>,
}

/// UDP ports a controller could reach: the ports of the sockets in the text of
/// `/proc/net/udp` (`v6 = false`) or `/proc/net/udp6` that are bound to the
/// wildcard address or to one of `local`, the addresses the media interfaces
/// have now. A socket bound to loopback, or to an address the host no longer
/// has, does not count: nothing on the network can reach it.
#[must_use]
pub fn udp_ports_served(table: &str, v6: bool, local: &[Ipv4Addr]) -> HashSet<u16> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let (addr, port) = line.split_whitespace().nth(1)?.rsplit_once(':')?;
            let reachable = if v6 {
                // Only the wildcard (`::`, which also takes IPv4 unless the
                // socket is v6-only): a specific IPv6 address is not one the
                // advertised IPv4 records point at.
                addr.len() == 32 && addr.bytes().all(|b| b == b'0')
            } else {
                // Printed as the native-endian integer of the address bytes.
                let ip = Ipv4Addr::from(u32::from_str_radix(addr, 16).ok()?.to_ne_bytes());
                ip.is_unspecified() || local.contains(&ip)
            };
            reachable
                .then(|| u16::from_str_radix(port, 16).ok())
                .flatten()
        })
        .collect()
}

/// The addresses the media interfaces have now: the named ones, or every
/// interface of the host when the plan names none.
fn local_addresses(interfaces: &[String]) -> Vec<Ipv4Addr> {
    if interfaces.is_empty() {
        netif::scan().into_iter().filter_map(|i| i.ipv4).collect()
    } else {
        interfaces
            .iter()
            .filter_map(|n| netif::lookup(n).ok().map(|i| i.addr))
            .collect()
    }
}

/// The UDP ports served on the media interfaces, or `None` if the kernel
/// tables cannot be read (then nothing is reported either way).
fn serving_udp_ports(interfaces: &[String]) -> Option<HashSet<u16>> {
    let local = local_addresses(interfaces);
    let mut ports = udp_ports_served(
        &std::fs::read_to_string("/proc/net/udp").ok()?,
        false,
        &local,
    );
    if let Ok(v6) = std::fs::read_to_string("/proc/net/udp6") {
        ports.extend(udp_ports_served(&v6, true, &local));
    }
    Some(ports)
}

/// A change in whether an advertised control port has a listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerChange {
    /// Advertised, but no socket on this host is bound to the port (seen for
    /// the first time, or lost again).
    Unserved {
        /// Device key.
        device: String,
        /// The first service kind advertised on the port.
        kind: ServiceKind,
        /// UDP port.
        port: u16,
    },
    /// A socket is bound to a port that was reported as unserved.
    Served {
        /// Device key.
        device: String,
        /// The first service kind advertised on the port.
        kind: ServiceKind,
        /// UDP port.
        port: u16,
    },
}

/// Compare the advertised control ports (ARC, CMC and the channel flow-control
/// port; the multicast bundles are not listened on) with the bound UDP ports.
/// `unserved` holds the (device, port) pairs already reported and is updated,
/// so each transition is reported once.
pub fn listener_changes(
    registered: &HashMap<String, DeviceServices>,
    in_use: &HashSet<u16>,
    unserved: &mut HashSet<(String, u16)>,
) -> Vec<ListenerChange> {
    let mut keys: Vec<&String> = registered.keys().collect();
    keys.sort();
    let mut wanted: HashSet<(String, u16)> = HashSet::new();
    let mut changes = Vec::new();
    for key in keys {
        for s in &registered[key].services {
            if !matches!(
                s.kind,
                ServiceKind::Arc | ServiceKind::Cmc | ServiceKind::Chan
            ) || !wanted.insert((key.clone(), s.port))
            {
                continue;
            }
            let pair = (key.clone(), s.port);
            let (device, kind, port) = (key.clone(), s.kind, s.port);
            if in_use.contains(&port) {
                if unserved.remove(&pair) {
                    changes.push(ListenerChange::Served { device, kind, port });
                }
            } else if unserved.insert(pair) {
                changes.push(ListenerChange::Unserved { device, kind, port });
            }
        }
    }
    unserved.retain(|p| wanted.contains(p));
    changes
}

/// Keeps the registered devices in step with a plan.
#[derive(Default)]
pub struct DiscoveryManager {
    started: Option<Started>,
    registered: HashMap<String, DeviceServices>,
    /// Advertised (device, port) pairs already reported as having no listener.
    unserved: HashSet<(String, u16)>,
    failed_at: Option<Instant>,
    last_error: Option<String>,
    browsing: bool,
    epoch: u64,
    last_inventory: Option<(u64, u64, u64, RoutingDiscoveryState)>,
}

struct Started {
    disco: Discovery,
    responder: Responder,
    mode: MdnsMode,
    interfaces: Vec<String>,
    browser: Option<Browser>,
    events: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for DiscoveryManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveryManager")
            .field("registered", &self.registered.len())
            .finish_non_exhaustive()
    }
}

fn mode_of(m: MdnsMode) -> Option<Mode> {
    match m {
        MdnsMode::Auto => Some(Mode::Auto),
        MdnsMode::Avahi => Some(Mode::Avahi),
        MdnsMode::Builtin => Some(Mode::Builtin),
        MdnsMode::Off => None,
    }
}

impl DiscoveryManager {
    /// A manager with nothing started.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Devices currently registered.
    #[must_use]
    pub fn registered(&self) -> usize {
        self.registered.len()
    }

    /// Make the registered devices match `plan`.
    pub async fn sync(&mut self, plan: &DiscoveryPlan) {
        self.browsing = plan.browse;
        let Some(mode) = mode_of(plan.mode) else {
            self.shutdown().await;
            return;
        };
        if self
            .started
            .as_ref()
            .is_some_and(|s| s.mode != plan.mode || s.interfaces != plan.interfaces)
        {
            // The backend or its interfaces changed: start over.
            self.shutdown().await;
        }
        if self.started.is_none() {
            if self.failed_at.is_some_and(|t| t.elapsed() < RETRY) {
                return;
            }
            let cfg = DiscoveryConfig {
                mode,
                interfaces: plan.interfaces.clone(),
                ..DiscoveryConfig::default()
            };
            match Discovery::start(cfg).await {
                Ok(d) => {
                    let responder = d.responder();
                    let events = responder.events().map(|mut events| {
                        tokio::spawn(async move {
                            while let Some(event) = events.recv().await {
                                match event {
                                    ResponderEvent::Conflict { .. }
                                    | ResponderEvent::Failed { .. } => {
                                        tracing::warn!("discovery: {event}")
                                    }
                                    _ => tracing::debug!("discovery: {event}"),
                                }
                            }
                        })
                    });
                    self.epoch += 1;
                    tracing::info!("discovery started ({})", d.backend());
                    self.failed_at = None;
                    self.last_error = None;
                    self.started = Some(Started {
                        disco: d,
                        responder,
                        mode: plan.mode,
                        interfaces: plan.interfaces.clone(),
                        browser: None,
                        events,
                    });
                }
                Err(e) => {
                    self.failed_at = Some(Instant::now());
                    let msg = e.to_string();
                    if self.last_error.as_deref() != Some(msg.as_str()) {
                        tracing::warn!(
                            "discovery cannot start (retrying every {} s): {msg}",
                            RETRY.as_secs()
                        );
                    }
                    self.last_error = Some(msg);
                    return;
                }
            }
        }
        let Some(s) = &self.started else {
            return;
        };
        let responder = s.responder.clone();
        let mut wanted: HashMap<String, DeviceServices> = HashMap::new();
        for d in &plan.devices {
            match services_for(d) {
                Ok(svc) => {
                    wanted.insert(d.key.clone(), svc);
                }
                Err(e) => tracing::warn!("device `{}` cannot be advertised: {e}", d.key),
            }
        }
        let gone: Vec<String> = self
            .registered
            .keys()
            .filter(|k| !wanted.contains_key(*k))
            .cloned()
            .collect();
        for key in gone {
            match responder.withdraw(&key).await {
                Ok(()) => tracing::info!("device `{key}` withdrawn from discovery"),
                Err(e) => tracing::warn!("withdrawing `{key}`: {e}"),
            }
            self.registered.remove(&key);
        }
        for (key, svc) in wanted {
            match self.registered.get(&key) {
                Some(old) if *old == svc => {}
                Some(_) => {
                    let r = match responder.update(svc.clone()).await {
                        Err(DiscError::UnsupportedUpdate { .. }) => {
                            // Host name or addresses changed: start over.
                            let _ = responder.withdraw(&key).await;
                            responder.register(svc.clone()).await
                        }
                        other => other,
                    };
                    match r {
                        Ok(()) => {
                            tracing::info!("device `{key}` updated in discovery");
                            self.registered.insert(key, svc);
                        }
                        Err(e) => tracing::warn!("updating `{key}` in discovery: {e}"),
                    }
                }
                None => match responder.register(svc.clone()).await {
                    Ok(()) => {
                        tracing::info!("device `{key}` registered in discovery");
                        self.registered.insert(key, svc);
                    }
                    Err(e) => tracing::warn!("registering `{key}` in discovery: {e}"),
                },
            }
        }
        self.check_listeners(&plan.interfaces);
        if let Some(started) = &mut self.started {
            if started.browser.as_ref().is_some_and(Browser::is_closed) {
                started.browser = None;
                self.epoch += 1;
            }
            if !plan.browse {
                if started.browser.take().is_some() {
                    self.epoch += 1;
                }
            } else if started.browser.is_none() {
                match started.disco.browse(&[]).await {
                    Ok(browser) => {
                        started.browser = Some(browser);
                        self.epoch += 1;
                    }
                    Err(error) => tracing::warn!("discovery browsing cannot start: {error}"),
                }
            }
        }
    }

    /// On-change full snapshots, debounced by the caller. Draining has a
    /// fixed work budget; queued events are not an inventory source of truth.
    pub(crate) async fn inventory_snapshot(&mut self) -> Option<DiscoverySnapshot> {
        let mut state = if !self.browsing {
            RoutingDiscoveryState::Disabled
        } else if self.last_error.is_some() || self.started.is_some() {
            RoutingDiscoveryState::Failed
        } else {
            RoutingDiscoveryState::Disabled
        };
        let mut responder_revision = 0;
        let mut browser_revision = 0;
        if let Some(started) = &mut self.started {
            if self.browsing {
                responder_revision = started.responder.revision();
                if let Some(browser) = &mut started.browser {
                    for _ in 0..256 {
                        if browser.try_recv().is_none() {
                            break;
                        }
                    }
                    browser_revision = browser.revision();
                    state = match browser.readiness() {
                        BrowseReadiness::Ready => RoutingDiscoveryState::Browsing,
                        BrowseReadiness::Reconciling => RoutingDiscoveryState::Starting,
                        _ => RoutingDiscoveryState::Failed,
                    };
                }
            }
        }
        let key = (self.epoch, responder_revision, browser_revision, state);
        if self.last_inventory == Some(key) {
            return None;
        }
        self.last_inventory = Some(key);
        let mut snapshot = DiscoverySnapshot::empty(state);
        snapshot.browser_epoch = self.epoch;
        let Some(started) = &mut self.started else {
            return Some(snapshot);
        };
        snapshot
            .permitted_interfaces
            .clone_from(&started.interfaces);
        if !self.browsing {
            return Some(snapshot);
        }
        match tokio::time::timeout(Duration::from_millis(500), started.responder.published()).await
        {
            Ok(Ok(published)) => {
                snapshot.own = published
                    .into_iter()
                    .map(|own| OwnAdvertisement {
                        id: own.service.id,
                        host: own.service.host,
                        port: own.service.port,
                        addresses: own.service.addresses,
                        txt: own.service.txt,
                        device: DeviceKey(own.device),
                    })
                    .collect();
            }
            _ => {
                snapshot.health.state = RoutingDiscoveryState::Failed;
                self.last_inventory = None;
                // Without authoritative own records, observed advertisements
                // cannot safely be distinguished from our local devices.
                return Some(snapshot);
            }
        }
        if let Some(browser) = &started.browser {
            snapshot.health.dropped = browser.dropped();
            snapshot.health.lost_events = browser
                .lost_events()
                .saturating_add(started.responder.dropped_events());
            snapshot.health.resolve_failures = browser.resolve_failures();
            for observation in browser.observations() {
                if observation.source.len() > 128
                    || observation.interface.as_ref().is_some_and(|s| s.len() > 64)
                    || observation.service.id.instance.len() > 63
                    || observation.service.host.len() > 255
                    || observation.service.addresses.len() > 64
                    || observation
                        .service
                        .txt
                        .validate(&tessera_discovery::TxtLimits::default())
                        .is_err()
                {
                    snapshot.health.rejected_observations += 1;
                } else {
                    snapshot.observations.push(observation);
                }
            }
        }
        Some(snapshot)
    }

    pub(crate) fn retry_inventory(&mut self) {
        self.last_inventory = None;
    }

    pub(crate) fn browsing(&self) -> bool {
        self.browsing
    }

    /// Warn, once per port, about an advertised control port nobody listens
    /// on: a controller resolves the device, gets no answer and shows it as
    /// unreachable. Another process may answer for the daemon (a lab
    /// responder), so this is a warning, never a reason to withdraw.
    fn check_listeners(&mut self, interfaces: &[String]) {
        let Some(in_use) = serving_udp_ports(interfaces) else {
            return;
        };
        for change in listener_changes(&self.registered, &in_use, &mut self.unserved) {
            match change {
                ListenerChange::Unserved { device, kind, port } => tracing::warn!(
                    "device `{device}`: advertising {} on UDP port {port}, but no socket is \
                     bound to it on the wildcard address or on a media interface's current \
                     address; a controller shows the device as unreachable unless another \
                     process answers there",
                    kind.type_label()
                ),
                ListenerChange::Served { device, kind, port } => tracing::info!(
                    "device `{device}`: UDP port {port} ({}) has a listener now",
                    kind.type_label()
                ),
            }
        }
    }

    /// Withdraw everything and stop the backend.
    pub async fn shutdown(&mut self) {
        if let Some(s) = self.started.take() {
            self.epoch += 1;
            for key in self.registered.keys() {
                let _ = s.responder.withdraw(key).await;
            }
            self.registered.clear();
            self.unserved.clear();
            s.disco.shutdown().await;
            drop(s.responder);
            if let Some(mut events) = s.events {
                if tokio::time::timeout(Duration::from_secs(1), &mut events)
                    .await
                    .is_err()
                {
                    events.abort();
                }
            }
            tracing::info!("discovery stopped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advert(labels: &[&str]) -> DeviceAdvert {
        DeviceAdvert {
            key: "main".into(),
            name: "My Device.1".into(),
            wire_id: [1, 2, 3, 4, 5, 6, 7, 8],
            rate: 48_000,
            bits: 24,
            latency_ns: 4_000_000,
            tx_labels: labels.iter().map(|s| (*s).to_owned()).collect(),
            index: 0,
            bundles: Vec::new(),
        }
    }

    #[test]
    fn a_multicast_flow_is_advertised_and_marks_its_channels() {
        let mut d = advert(&["TX 1", "TX 2"]);
        d.bundles = vec![Bundle {
            id: 3,
            channels: vec![2],
            addr: Ipv4Addr::new(239, 255, 1, 3),
            port: 4321,
            fpp: 32,
        }];
        let s = services_for(&d).unwrap();
        let bund = s
            .services
            .iter()
            .find(|x| x.kind == ServiceKind::Bund)
            .unwrap();
        assert_eq!(bund.instance, "3@My-Device-1");
        assert_eq!(bund.txt.get_str("a.0"), Some("239.255.1.3"));
        let chan = |name: &str| {
            s.services
                .iter()
                .find(|x| x.kind == ServiceKind::Chan && x.instance.starts_with(name))
                .unwrap()
        };
        assert_eq!(chan("TX 2@").txt.get_str("b.3"), Some("1"));
        assert_eq!(chan("TX 1@").txt.get_str("b.3"), None);
    }

    #[test]
    fn hostnames_are_dns_labels() {
        assert_eq!(hostname_of("My Device.1"), "My-Device-1");
        assert_eq!(hostname_of(""), "tessera");
        assert_eq!(hostname_of(&"x".repeat(80)).len(), 31);
    }

    #[test]
    fn a_device_gets_arc_cmc_and_one_chan_record_per_channel() {
        let s = services_for(&advert(&["TX 1", "TX 2"])).unwrap();
        assert_eq!(s.hostname, "My-Device-1");
        assert_eq!(s.services.len(), 4);
    }

    #[test]
    fn a_renamed_channel_has_two_records_and_duplicates_are_made_unique() {
        let s = services_for(&advert(&["Vox", "TX 2", "Vox"])).unwrap();
        // ARC, CMC, [TX 1 + Vox], [TX 2], [TX 3 + Vox 3]
        assert_eq!(s.services.len(), 2 + 2 + 1 + 2);
        s.validate(&tessera_discovery::config::Limits::default())
            .unwrap();
    }

    #[test]
    fn several_devices_do_not_share_ports() {
        let mut a = advert(&["TX 1"]);
        let mut b = advert(&["TX 1"]);
        b.key = "aux".into();
        b.index = 1;
        a.index = 0;
        let (a, b) = (services_for(&a).unwrap(), services_for(&b).unwrap());
        assert_ne!(a.services[0].port, b.services[0].port);
    }

    // Addresses are the native-endian integer of the bytes (little-endian here):
    // 0A0200C0 = 192.0.2.10 (the interface now), 630200C0 = 192.0.2.99 (an
    // address it had before), 0100007F = 127.0.0.1.
    #[cfg(target_endian = "little")]
    const UDP_TABLE: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
 1: 00000000:1158 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 11111 2 0000000000000000 0
 2: 0A0200C0:2260 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 22222 2 0000000000000000 0
 3: 630200C0:1167 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 33333 2 0000000000000000 0
 4: 0100007F:2328 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 44444 2 0000000000000000 0
 5: not-a-row
";

    #[cfg(target_endian = "little")]
    #[test]
    fn only_sockets_a_controller_can_reach_count_as_serving() {
        let now = [Ipv4Addr::new(192, 0, 2, 10)];
        // Wildcard (4440) and the current address (8800). Not: an address the
        // interface had before (4455), loopback (9000), a malformed row.
        assert_eq!(
            udp_ports_served(UDP_TABLE, false, &now),
            HashSet::from([4440, 8800])
        );
        // With no media address known only the wildcard socket counts.
        assert_eq!(
            udp_ports_served(UDP_TABLE, false, &[]),
            HashSet::from([4440])
        );
        assert!(udp_ports_served("", false, &now).is_empty());
    }

    #[test]
    fn only_a_wildcard_ipv6_socket_counts_as_serving() {
        let v6 = "  sl  local_address rem_address\n \
                  1: 00000000000000000000000000000000:1167 0000:0000 07\n \
                  2: 00000000000000000000000001000000:1158 0000:0000 07\n";
        assert_eq!(udp_ports_served(v6, true, &[]), HashSet::from([4455]));
    }

    fn registered(adverts: &[DeviceAdvert]) -> HashMap<String, DeviceServices> {
        adverts
            .iter()
            .map(|a| (a.key.clone(), services_for(a).unwrap()))
            .collect()
    }

    fn unserved_ports(changes: &[ListenerChange]) -> Vec<u16> {
        changes
            .iter()
            .filter_map(|c| match c {
                ListenerChange::Unserved { port, .. } => Some(*port),
                ListenerChange::Served { .. } => None,
            })
            .collect()
    }

    #[test]
    fn an_advertised_port_nobody_listens_on_is_reported_once() {
        let reg = registered(&[advert(&["TX 1", "TX 2"])]);
        let mut seen = HashSet::new();
        let first = listener_changes(&reg, &HashSet::new(), &mut seen);
        // ARC, CMC and the one flow-control port shared by both channels.
        assert_eq!(unserved_ports(&first), vec![4440, 8800, 4455]);
        assert!(listener_changes(&reg, &HashSet::new(), &mut seen).is_empty());
    }

    #[test]
    fn a_listener_that_appears_and_disappears_is_reported_each_time() {
        let reg = registered(&[advert(&["TX 1"])]);
        let mut seen = HashSet::new();
        listener_changes(&reg, &HashSet::new(), &mut seen);
        let up = HashSet::from([4440, 8800, 4455]);
        let served = listener_changes(&reg, &up, &mut seen);
        assert_eq!(served.len(), 3);
        assert!(
            served
                .iter()
                .all(|c| matches!(c, ListenerChange::Served { .. }))
        );
        assert!(listener_changes(&reg, &up, &mut seen).is_empty());
        let lost = listener_changes(&reg, &HashSet::from([4440]), &mut seen);
        assert_eq!(unserved_ports(&lost), vec![8800, 4455]);
    }

    #[test]
    fn a_port_that_is_served_from_the_start_is_not_reported() {
        let reg = registered(&[advert(&["TX 1"])]);
        let mut seen = HashSet::new();
        let up = HashSet::from([4440, 8800, 4455]);
        assert!(listener_changes(&reg, &up, &mut seen).is_empty());
        assert!(seen.is_empty());
    }

    #[test]
    fn a_withdrawn_device_is_forgotten_so_it_is_reported_again_when_it_returns() {
        let reg = registered(&[advert(&["TX 1"])]);
        let mut seen = HashSet::new();
        listener_changes(&reg, &HashSet::new(), &mut seen);
        assert!(listener_changes(&HashMap::new(), &HashSet::new(), &mut seen).is_empty());
        assert!(seen.is_empty());
        assert_eq!(
            unserved_ports(&listener_changes(&reg, &HashSet::new(), &mut seen)).len(),
            3
        );
    }

    #[test]
    fn several_devices_are_checked_on_their_own_ports() {
        let a = advert(&["TX 1"]);
        let mut b = advert(&["TX 1"]);
        b.key = "aux".into();
        b.index = 1;
        let reg = registered(&[a, b]);
        let mut seen = HashSet::new();
        // Only the first device's ports are bound.
        let up = HashSet::from([4440, 8800, 4455]);
        let ch = listener_changes(&reg, &up, &mut seen);
        assert_eq!(unserved_ports(&ch), vec![4450, 8810, 4465]);
    }
}
