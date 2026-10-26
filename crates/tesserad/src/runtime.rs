//! The runtime reconciler: keeps the clock domain, the media engine and
//! discovery in step with the state engine (ADR 0008).
//!
//! The state engine is the single source of truth. This task asks it for the
//! effective model and configuration ([`EngineHandle::runtime_plan`]) at
//! start-up, after every event that can matter (a reload, a clock role or
//! priority change, a device renamed or re-rated, a channel label) and every
//! few seconds (retries), derives what should be running ([`derive`], a pure
//! function), and applies the differences:
//!
//! * **clock** (`[clock]`, `[network]`, `[engine]`): a new [`ClockPlan`] to the
//!   clock domain, applied live;
//! * **flows** (`[[flow]]`): the resolved [`FlowPlan`]s to the media engine,
//!   which starts, replaces and stops flows as needed; `free_running_tx` is
//!   forwarded;
//! * **audio clients**: when a device's rate, encoding, latency or channel
//!   count changed, its attached clients are detached so they re-attach with
//!   the new ring geometry;
//! * **discovery**: devices are registered, updated and withdrawn.
//!
//! Applying a plan twice does nothing the second time, so a lagging event
//! subscription simply triggers one more pass.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tessera_control_types::{Event, NetworkStatus};
use tessera_state::Received;
use tessera_state::bus::EventBus;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::audio::AudioCtx;
use crate::bridge::{BridgeManager, BridgePlan, plan_bridges};
use crate::disco::{DeviceAdvert, DiscoveryManager, DiscoveryPlan};
use crate::engine_thread::{EngineHandle, RuntimePlan};
use crate::media::MediaHandle;
use crate::media::plan::{FlowPlan, Ignored, plan_flows};
use crate::netif;
use crate::ptp::{ClockHandle, ClockPlan};
use tessera_state::config::MdnsMode;

/// Process-level settings of apply class `restart`, read once at start
/// ([`RuntimeOpts::of`]): a reload that changes them does not reach the
/// reconciler (it is reported as needing a service restart instead).
#[derive(Debug, Clone, Copy, Default)]
pub struct RuntimeOpts {
    /// `[clock] enabled = false` (`--no-ptp`).
    pub no_ptp: bool,
    /// `[daemon] realtime = false` (`--no-rt`).
    pub no_rt: bool,
    /// `[discovery] enabled = false` (`--no-discovery`).
    pub no_discovery: bool,
    /// `SCHED_FIFO` priority of the PTP threads (`[engine] ptp_rt_priority`;
    /// `None` without real time or for 0).
    pub ptp_rt_priority: Option<u8>,
    /// CPU of the PTP threads (the first of `[engine] cpu_affinity`).
    pub cpu: Option<usize>,
}

impl RuntimeOpts {
    /// Read from the configuration in effect at start.
    #[must_use]
    pub fn of(cfg: &tessera_state::Config) -> Self {
        let no_rt = !cfg.daemon.realtime;
        let p = cfg.engine.ptp_rt_priority;
        Self {
            no_ptp: !cfg.clock.enabled,
            no_rt,
            no_discovery: !cfg.discovery.enabled,
            ptp_rt_priority: (!no_rt && p > 0).then_some(p),
            cpu: cfg
                .engine
                .cpu_affinity
                .first()
                .and_then(|c| usize::try_from(*c).ok()),
        }
    }
}

/// What should be running.
#[derive(Debug, Clone)]
pub struct Desired {
    /// The clock domain.
    pub clock: ClockPlan,
    /// The static flows that resolved.
    pub flows: Vec<FlowPlan>,
    /// Flows that did not resolve: `(id, reason)`.
    pub flow_errors: Vec<(String, String)>,
    /// `[clock] free_running_tx`.
    pub free_running_tx: bool,
    /// Discovery.
    pub discovery: DiscoveryPlan,
    /// Device geometry, to detect changes that need clients to re-attach.
    pub geometry: HashMap<String, Geometry>,
    /// The primary interface and the candidates (ADR 0012).
    pub network: NetworkStatus,
    /// Why a runtime selection of the primary is not in effect.
    pub fallback: Option<String>,
    /// The hardware-card bridges to run.
    pub bridges: Vec<BridgePlan>,
    /// Bridges that cannot run on the effective devices, with what their
    /// status rows show.
    pub bridge_errors: Vec<crate::bridge::plan::PlanError>,
}

/// The part of a device that fixes the shape of its audio rings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// Sample rate.
    pub rate: u32,
    /// Wire bits.
    pub bits: u8,
    /// Latency, microseconds.
    pub latency_us: u32,
    /// RX channels.
    pub rx: u16,
    /// TX channels.
    pub tx: u16,
}

/// Work out what should run from the effective model and configuration.
#[must_use]
pub fn derive(rp: &RuntimePlan, opts: RuntimeOpts) -> Desired {
    derive_with(rp, opts, &netif::lookup, &netif::scan())
}

/// [`derive`] with an explicit interface lookup and host interfaces (tests).
#[must_use]
pub fn derive_with(
    rp: &RuntimePlan,
    opts: RuntimeOpts,
    lookup: &dyn Fn(&str) -> std::io::Result<netif::IfInfo>,
    host: &[netif::HostIf],
) -> Desired {
    let cfg = &rp.config;
    let sel = netif::Selection::of(rp);
    let res = netif::resolve(sel, host);
    let network = netif::network_status(&res, sel, host);
    let ifaces = res.interfaces.clone();
    let clock = ClockPlan {
        interfaces: ifaces.clone(),
        role: rp.model.clock.role,
        priority: rp.model.clock.priority,
        timestamping: cfg.clock.timestamping,
        rt_priority: opts.ptp_rt_priority,
        cpu: opts.cpu,
        allow_wireless: cfg.network.allow_wireless,
        enabled: !opts.no_ptp,
    };
    let (mut flows, flow_errors) = plan_flows(
        &cfg.flows,
        &rp.model.devices,
        &ifaces,
        cfg.network.allow_wireless,
        cfg.network.mtu_payload,
        lookup,
    );
    // `[defaults.flow]` and the entry's own keys (apply class `reload`):
    // carried with the plan, used when the flow next (re)starts.
    // Every plan comes from an entry of `cfg.flows` (ids are unique), so
    // each rx plan gets its resolved keepalive.
    let entries: HashMap<&str, &tessera_state::config::FlowConfig> =
        cfg.flows.iter().map(|f| (f.id.as_str(), f)).collect();
    for p in flows
        .iter_mut()
        .filter(|p| p.direction == tessera_control_types::Direction::Rx)
    {
        if let Some(f) = entries.get(p.id.as_str()) {
            p.keepalive = Ignored(Some(crate::tunables::keepalive(
                &f.keepalive(&cfg.defaults.flow),
            )));
        }
    }
    // Advertise by default; honour configuration/runtime opt-outs and skip loopback.
    let off =
        opts.no_discovery || !cfg.discovery.advertise_devices || netif::only_loopback(&ifaces);
    let discovery = DiscoveryPlan {
        mode: if off {
            MdnsMode::Off
        } else {
            cfg.discovery.mdns
        },
        interfaces: ifaces,
        devices: rp
            .model
            .devices
            .iter()
            .enumerate()
            .map(|(i, d)| DeviceAdvert::of(d, i))
            .collect(),
    };
    let geometry = rp
        .model
        .devices
        .iter()
        .map(|d| {
            (
                d.key.0.clone(),
                Geometry {
                    rate: d.sample_rate,
                    bits: d.encoding,
                    latency_us: d.latency_us,
                    rx: d.rx_channels,
                    tx: d.tx_channels,
                },
            )
        })
        .collect();
    let (bridges, bridge_errors) = plan_bridges(cfg, &rp.model.devices, opts.no_rt);
    Desired {
        bridges,
        bridge_errors,
        clock,
        flows,
        flow_errors,
        free_running_tx: cfg.clock.free_running_tx,
        discovery,
        geometry,
        network,
        fallback: res.fallback_reason,
    }
}

/// The reconciler's inputs.
#[derive(Debug)]
pub struct Runtime {
    /// The state engine.
    pub engine: EngineHandle,
    /// Its event bus.
    pub bus: EventBus,
    /// The clock domain.
    pub clock: ClockHandle,
    /// The media engine.
    pub media: MediaHandle,
    /// The audio endpoint, if it is on.
    pub audio: Option<Arc<AudioCtx>>,
    /// The hardware-card bridges.
    pub bridges: Option<Arc<BridgeManager>>,
    /// Switches.
    pub opts: RuntimeOpts,
    /// Daemon shutdown.
    pub shutdown: CancellationToken,
}

#[derive(Default)]
struct Applied {
    clock: Option<ClockPlan>,
    flows: Option<Vec<FlowPlan>>,
    flow_errors: Vec<(String, String)>,
    free_running_tx: Option<bool>,
    geometry: HashMap<String, Geometry>,
    network: Option<NetworkStatus>,
    fallback: Option<String>,
    clock_interfaces: Option<Vec<String>>,
    bridges: Option<BridgeSet>,
}

/// Bridge plans and the bridges that cannot run.
type BridgeSet = (Vec<BridgePlan>, Vec<crate::bridge::plan::PlanError>);

fn relevant(e: &Event) -> bool {
    !matches!(
        e,
        Event::ClockStateChanged { .. }
            | Event::SubscriptionChanged { .. }
            | Event::SubscriptionChangedWithOrigin { .. }
            | Event::LockedValueRefused { .. }
            | Event::RuntimeValueChanged { .. }
    )
}

impl Runtime {
    fn apply_core(&self, st: &mut Applied, d: &Desired) {
        if st.clock.as_ref() != Some(&d.clock) {
            self.clock.apply(d.clock.clone());
            st.clock = Some(d.clock.clone());
        }
        if st.free_running_tx != Some(d.free_running_tx) {
            self.media.set_free_running_tx(d.free_running_tx);
            st.free_running_tx = Some(d.free_running_tx);
        }
        // A changed keepalive alone does not make two plans differ (it waits
        // for the flow's next (re)start), but the engine must still get it.
        let same_keepalives = |f: &[FlowPlan]| {
            f.iter()
                .map(|p| p.keepalive.0)
                .eq(d.flows.iter().map(|p| p.keepalive.0))
        };
        if st.flows.as_ref() != Some(&d.flows)
            || st.flows.as_deref().is_some_and(|f| !same_keepalives(f))
        {
            self.media.set_flows(d.flows.clone());
            st.flows = Some(d.flows.clone());
        }
        let bridges = (d.bridges.clone(), d.bridge_errors.clone());
        if st.bridges.as_ref() != Some(&bridges) {
            if let Some(m) = &self.bridges {
                m.apply(bridges.0.clone(), bridges.1.clone());
            }
            st.bridges = Some(bridges);
        }
        if st.flow_errors != d.flow_errors {
            for (id, e) in &d.flow_errors {
                if !st.flow_errors.iter().any(|(i, m)| i == id && m == e) {
                    tracing::error!("flow `{id}` is not valid here: {e}");
                }
            }
            st.flow_errors = d.flow_errors.clone();
        }
        if let Some(audio) = &self.audio {
            for (key, g) in &d.geometry {
                if st.geometry.get(key).is_some_and(|old| old != g) {
                    tracing::info!("device `{key}` changed shape; audio clients are detached");
                    audio.detach_device(key);
                }
            }
        }
        st.geometry = d.geometry.clone();
        if st.network.as_ref() != Some(&d.network) && self.engine.report_network(d.network.clone())
        {
            st.network = Some(d.network.clone());
        }
        let ifaces = if d.clock.enabled {
            d.clock.interfaces.clone()
        } else {
            Vec::new()
        };
        if st.clock_interfaces.as_ref() != Some(&ifaces)
            && self.engine.report_clock_interfaces(ifaces.clone())
        {
            st.clock_interfaces = Some(ifaces);
        }
        if st.fallback != d.fallback {
            let using = d.network.primary.as_deref().unwrap_or("no interface");
            match &d.fallback {
                Some(why) => tracing::warn!("{why}; using `{using}` until it is back"),
                None => tracing::info!("primary interface: `{using}`"),
            }
            st.fallback.clone_from(&d.fallback);
        }
    }
}

/// The reconciler runs a pass at least this often.
const PASS_EVERY: Duration = Duration::from_secs(5);
/// How often the interfaces in use are looked at between passes.
const IFACE_CHECK: Duration = Duration::from_secs(1);

/// The interfaces the clock and the flows use.
fn watched_interfaces(rp: &RuntimePlan, d: &Desired) -> Vec<String> {
    let mut v = d.clock.interfaces.clone();
    // A selected primary that is missing: notice its return at once.
    if let Some(s) = &rp.model.network.selected {
        if !v.contains(s) {
            v.push(s.clone());
        }
    }
    for f in &d.flows {
        if !v.contains(&f.interface) {
            v.push(f.interface.clone());
        }
    }
    v
}

/// Index and address of each interface (`None`: missing or unusable). A pass
/// runs as soon as this changes: an interface removed, re-created (an adapter
/// plugged back in gets a new index) or re-addressed, so that the flows are
/// rebuilt on it without waiting for [`PASS_EVERY`].
fn iface_identity(names: &[String]) -> Vec<Option<(u32, std::net::Ipv4Addr)>> {
    names
        .iter()
        .map(|n| netif::lookup(n).ok().map(|i| (i.index, i.addr)))
        .collect()
}

/// Run the reconciler until shutdown. `first` is signalled after the first
/// pass has applied the clock and the flows (before discovery, which can
/// take a while).
pub async fn run(rt: Runtime, first: oneshot::Sender<()>) {
    let mut sub = rt.bus.subscribe(256);
    let mut disco = DiscoveryManager::new();
    let mut st = Applied::default();
    let mut first = Some(first);
    let mut watched: Vec<String> = Vec::new();
    loop {
        match rt.engine.runtime_plan().await {
            Ok(rp) => {
                let d = derive(&rp, rt.opts);
                watched = watched_interfaces(&rp, &d);
                rt.apply_core(&mut st, &d);
                if let Some(f) = first.take() {
                    let _ = f.send(());
                }
                tokio::select! {
                    () = rt.shutdown.cancelled() => break,
                    () = disco.sync(&d.discovery) => {}
                }
            }
            Err(e) => {
                tracing::warn!("runtime reconciler: {e}");
                if let Some(f) = first.take() {
                    let _ = f.send(());
                }
            }
        }
        // Wait for the next trigger: a relevant event, an interface that
        // changed, or the retry timer.
        let seen = iface_identity(&watched);
        let iface_changed = async {
            for _ in 0..(PASS_EVERY.as_millis() / IFACE_CHECK.as_millis()).max(1) {
                tokio::time::sleep(IFACE_CHECK).await;
                if iface_identity(&watched) != seen {
                    return;
                }
            }
        };
        let wake = async {
            loop {
                match sub.recv_async().await {
                    Ok(Received::Event(e)) if relevant(&e) => return,
                    Ok(Received::Event(_)) => {}
                    Ok(Received::Lagged(_)) | Err(_) => return,
                }
            }
        };
        tokio::select! {
            () = rt.shutdown.cancelled() => break,
            () = wake => {
                // Coalesce a burst of events into one pass.
                tokio::time::sleep(Duration::from_millis(50)).await;
                while sub.recv_async_now_or_empty() {}
            }
            () = iface_changed => {}
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(3), disco.shutdown()).await;
}

/// Drain whatever is queued without waiting.
trait DrainNow {
    fn recv_async_now_or_empty(&mut self) -> bool;
}

impl DrainNow for tessera_state::Subscription {
    fn recv_async_now_or_empty(&mut self) -> bool {
        self.try_recv().is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::net::Ipv4Addr;
    use tessera_control_types::{ClockRole, DeviceKey, Direction};
    use tessera_state::config::{Config, FlowConfig, FlowFormat};
    use tessera_state::model::{ClockModel, DeviceModel, Model, WireDeviceId};

    fn rp() -> RuntimePlan {
        let dev = DeviceModel {
            key: DeviceKey("main".into()),
            wire_device_id: WireDeviceId([7; 8]),
            name: "studio".into(),
            sample_rate: 48_000,
            encoding: 24,
            latency_us: 4_000,
            rx_channels: 2,
            tx_channels: 2,
            rx_labels: BTreeMap::new(),
            tx_labels: BTreeMap::new(),
            subscriptions: BTreeMap::new(),
            locked: BTreeSet::new(),
            latency_bounds_us: [1_000, 40_000],
            allow_unsafe_latency: false,
        };
        let mut config = Config::default();
        config.network.interfaces = vec!["eth0".into()];
        config.flows.push(FlowConfig {
            id: "f".into(),
            direction: Direction::Tx,
            device: "main".into(),
            channels: Vec::new(),
            peer: Some(Ipv4Addr::new(10, 0, 0, 2)),
            port: 5000,
            interface: None,
            local_address: None,
            format: FlowFormat::Reference,
            frames_per_packet: None,
            send_offset_us: 0,
            any_source: false,
            dscp: 46,
            max_lag_us: None,
            rx_keepalive_ms: None,
            rx_keepalive_timeout_ms: None,
            rx_keepalive_payload: None,
        });
        RuntimePlan {
            model: Model {
                flows: config.flows.clone(),
                devices: vec![dev],
                clock: ClockModel {
                    role: ClockRole::Leader,
                    priority: 10,
                    locked: BTreeSet::new(),
                },
                network: tessera_state::model::NetworkModel::default(),
            },
            config,
        }
    }

    fn lookup(n: &str) -> std::io::Result<netif::IfInfo> {
        Ok(netif::IfInfo {
            name: n.into(),
            index: 2,
            addr: Ipv4Addr::new(10, 0, 0, 1),
            mac: [2, 0, 0, 0, 0, 1],
            wireless: false,
        })
    }

    #[test]
    fn the_effective_clock_settings_win_over_the_configured_ones() {
        let r = rp();
        let d = derive_with(&r, RuntimeOpts::of(&r.config), &lookup, &[]);
        assert_eq!(d.clock.role, ClockRole::Leader);
        assert_eq!(d.clock.priority, 10);
        assert_eq!(d.clock.interfaces, vec!["eth0".to_owned()]);
        assert!(d.clock.enabled);
        assert_eq!(d.clock.rt_priority, Some(70));
        assert_eq!(d.flows.len(), 1);
        assert!(d.flow_errors.is_empty());
    }

    #[test]
    fn devices_are_advertised_by_default_and_can_be_disabled() {
        let mut r = rp();
        assert!(r.config.discovery.advertise_devices);
        let d = derive_with(&r, RuntimeOpts::default(), &lookup, &[]);
        assert_eq!(d.discovery.mode, MdnsMode::Auto, "on by default");
        assert_eq!(d.discovery.devices.len(), 1);
        r.config.discovery.advertise_devices = false;
        let d = derive_with(&r, RuntimeOpts::default(), &lookup, &[]);
        assert_eq!(d.discovery.mode, MdnsMode::Off);
        assert_eq!(d.discovery.devices.len(), 1);
        r.config.discovery.advertise_devices = true;
        let d = derive_with(&r, RuntimeOpts::default(), &lookup, &[]);
        assert_eq!(d.discovery.mode, MdnsMode::Auto);
        assert_eq!(d.discovery.devices.len(), 1);
    }

    #[test]
    fn switches_turn_things_off() {
        let o = RuntimeOpts {
            no_ptp: true,
            no_rt: true,
            no_discovery: true,
            ..RuntimeOpts::default()
        };
        let d = derive_with(&rp(), o, &lookup, &[]);
        assert!(!d.clock.enabled);
        assert_eq!(d.clock.rt_priority, None);
        assert_eq!(d.discovery.mode, MdnsMode::Off);
    }

    #[test]
    fn loopback_only_has_no_discovery() {
        let mut r = rp();
        r.config.network.interfaces = vec!["lo".into()];
        let d = derive_with(&r, RuntimeOpts::default(), &lookup, &[]);
        assert_eq!(d.discovery.mode, MdnsMode::Off);
    }

    #[test]
    fn a_reshaped_device_shows_up_in_the_geometry() {
        let a = derive_with(&rp(), RuntimeOpts::default(), &lookup, &[]);
        let mut r = rp();
        r.model.devices[0].sample_rate = 96_000;
        let b = derive_with(&r, RuntimeOpts::default(), &lookup, &[]);
        assert_ne!(a.geometry["main"], b.geometry["main"]);
        // The flow follows the device rate.
        assert_eq!(b.flows[0].rate, 96_000);
        assert_ne!(a.flows, b.flows);
    }

    #[test]
    fn rx_flows_carry_their_keepalive_without_it_rebuilding_them() {
        let mut r = rp();
        r.config.flows[0].direction = tessera_control_types::Direction::Rx;
        r.config.flows[0].rx_keepalive_ms = Some(100);
        let o = RuntimeOpts::of(&r.config);
        let a = derive_with(&r, o, &lookup, &[]);
        let k = a.flows[0].keepalive.0.expect("resolved");
        assert_eq!(k.interval_ns, 100_000_000, "the entry's own key");
        assert_eq!(k.timeout_ns, 10_000_000_000, "[defaults.flow]");
        r.config.defaults.flow.rx_keepalive_timeout_ms = 0;
        let b = derive_with(&r, o, &lookup, &[]);
        assert_eq!(b.flows[0].keepalive.0.unwrap().timeout_ns, 0);
        assert_eq!(
            a.flows, b.flows,
            "a keepalive change waits for the next (re)start"
        );
    }

    #[test]
    fn restart_class_switches_are_read_at_start_not_from_a_reload() {
        let r = rp();
        let o = RuntimeOpts::of(&r.config);
        let mut reloaded = rp();
        reloaded.config.clock.enabled = false;
        reloaded.config.discovery.enabled = false;
        reloaded.config.daemon.realtime = false;
        reloaded.config.engine.ptp_rt_priority = 10;
        reloaded.config.engine.cpu_affinity = vec![3];
        let d = derive_with(&reloaded, o, &lookup, &[]);
        assert!(d.clock.enabled);
        assert_eq!(d.clock.rt_priority, Some(70));
        assert_eq!(d.clock.cpu, None);
    }

    #[test]
    fn a_channel_count_change_reshapes_flows_geometry_and_adverts() {
        let mut r = rp();
        r.config.discovery.advertise_devices = true;
        let o = RuntimeOpts::of(&r.config);
        let a = derive_with(&r, o, &lookup, &[]);
        r.model.devices[0].rx_channels = 4;
        r.model.devices[0].tx_channels = 4;
        let b = derive_with(&r, o, &lookup, &[]);
        assert_ne!(a.geometry["main"], b.geometry["main"], "clients re-attach");
        assert_eq!(a.flows[0].channels, vec![0, 1]);
        assert_eq!(b.flows[0].channels, vec![0, 1, 2, 3], "the flow is rebuilt");
        assert_ne!(a.discovery.devices, b.discovery.devices, "re-announced");
        assert_eq!(b.discovery.devices[0].tx_labels.len(), 4);
    }

    #[test]
    fn only_some_events_trigger_a_pass() {
        use tessera_control_types::{ClockState, ClockStatus, Timestamping};
        assert!(!relevant(&Event::ClockStateChanged {
            status: ClockStatus::new("e", ClockState::Locked, Timestamping::Software)
        }));
        assert!(relevant(&Event::ConfigReloaded));
    }
}
