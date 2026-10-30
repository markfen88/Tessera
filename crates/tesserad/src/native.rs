//! Daemon-owned device role (`--native-device`): the ARC, CMC and device-info
//! listeners and receive-flow negotiation, in place of the compatibility
//! runtime on a host that opts in (ADR 0016, paired handover, receive side).
//!
//! One authority: a subscription or label change from the network enters the
//! state engine as the same typed command the CLI and the portal use, and the
//! saved subscriptions in state remain the only desired-route store. The
//! receiver planner ([`receiver`]) derives flows from them and installs the
//! complete set with `SetProtocolFlows`; it keeps no routes of its own.
//!
//! The portal's routing operations reach this server like any controller:
//! through the local ARC endpoint the daemon advertises. [`local`] gives the
//! operation actor the context a registered runtime would otherwise supply.
//!
//! Specs: `spec/protocol/control-arc.md`, `control-cmc.md`, `conmon.md`,
//! `control-flows.md`, `state-machines.md` §§1-2, `discovery-mdns.md`.

mod access;
mod arc;
mod flows;
mod info;
mod receiver;
pub(crate) mod wire;

use std::collections::BTreeSet;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tessera_control_types::{
    Command, CommandError, FlowSpec, Origin, Query, Reply, SubscriptionState,
};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

use crate::engine_thread::EngineHandle;
use crate::routing_inventory::DiscoverySnapshot;

/// Well-known ports of the role (`discovery-mdns.md`, `control-cmc.md` §1).
pub const ARC_PORT: u16 = 4440;
pub const FLOW_PORT: u16 = 4455;
pub const INFO_PORT: u16 = 8700;
pub const CMC_PORT: u16 = 8800;

/// Settings fixed at start (`--native-device`, `--native-vendor`).
#[derive(Debug, Clone)]
pub struct Settings {
    /// The 8-byte vendor field of device-info messages, space padded
    /// (`conmon.md` §2).
    pub vendor: [u8; 8],
    /// Board-info capability word (`device-settings.md` §1.1). The default
    /// claims nothing the role cannot do.
    pub capabilities: u32,
    /// Lab only (`--native-lab-*`): advertise the lab capability words so a
    /// controller sends its write requests, and log every request the role
    /// does not handle.
    pub lab: bool,
    /// How long an identify request stays visible (`device-settings.md` §6:
    /// a configurable duration, 10 s by default; `--native-identify-secs`).
    pub identify_for: Duration,
    /// Lab only (`--native-lab-lock-listener`): log datagrams to the lock
    /// service port, never answering (`device-lock.md` §9.3 L12).
    pub lock_listener: bool,
}

impl Settings {
    #[must_use]
    pub fn new(vendor: &str) -> Self {
        let mut v = [b' '; 8];
        for (d, s) in v.iter_mut().zip(vendor.bytes().filter(u8::is_ascii)) {
            *d = s;
        }
        Self {
            vendor: v,
            capabilities: DEFAULT_CAPABILITIES,
            lab: false,
            identify_for: Duration::from_secs(10),
            lock_listener: false,
        }
    }

    /// Lab capture mode: advertise `capabilities` and writable settings.
    #[must_use]
    pub fn lab(mut self, capabilities: u32) -> Self {
        self.capabilities = capabilities;
        self.lab = true;
        self
    }

    /// In lab mode, also log the lock service port; ignored outside it.
    #[must_use]
    pub fn with_lock_listener(mut self, on: bool) -> Self {
        self.lock_listener = on && self.lab;
        self
    }
}

/// Flow-id prefix of manual multicast transmit flows in the protocol set.
pub const MULTICAST_PREFIX: &str = "native-mc-";

/// The capability word the role advertises by default.
/// Claims identify, sample-rate and encoding configuration, reboot and
/// factory reset (the low byte group, `device-settings.md` §1.1; factory
/// reset is answered, and applied where the host allows it); never lock
/// (0x08000000: the lock is enforced and reported, but controllers cannot set
/// it until the lock service exists, `device-lock.md` §9.3 L12) or static
/// addressing (0x00004000), which tessera does not implement. Holds
/// [`capability::SAMPLE_RATE`] and [`capability::ENCODING`] (the info server
/// answers both writes), never [`capability::PULL_UP`].
pub const DEFAULT_CAPABILITIES: u32 = 0x0000_10DB;
// `device-settings.md` §3.3 item 1, checked when the daemon is built.
const _: () = assert!(
    DEFAULT_CAPABILITIES & (capability::SAMPLE_RATE | capability::ENCODING)
        == capability::SAMPLE_RATE | capability::ENCODING
        && DEFAULT_CAPABILITIES & capability::PULL_UP == 0
);

/// Board-info capability bits with a known meaning (`device-settings.md`
/// §§1.1, 3.3 item 1; 0x10 `likely`, 0x08 and 0x200 `guess`).
pub mod capability {
    /// Sample-rate configuration: set exactly while 0x0081 writes are
    /// handled.
    pub const SAMPLE_RATE: u32 = 0x0000_0008;
    /// Encoding configuration: set exactly while 0x0083 writes are handled.
    pub const ENCODING: u32 = 0x0000_0010;
    /// Sample-rate pull-up/down: clear, tessera has none.
    pub const PULL_UP: u32 = 0x0000_0200;
}

static RESTART: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// A controller asked for a reboot: the daemon shuts down cleanly and exits
/// so that its service manager starts it again (`device-settings.md` §6).
pub(crate) fn request_restart() {
    RESTART.notify_one();
}

/// Resolves when a restart has been requested.
pub(crate) async fn restart_requested() {
    RESTART.notified().await;
}

/// What the routing operation actor needs in place of a registered runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Local {
    pub interface: String,
    pub address: Ipv4Addr,
    pub name: String,
    pub rx_channels: u16,
    /// Changes whenever the listeners are bound again.
    pub incarnation: String,
}

static LOCAL: RwLock<Option<Local>> = RwLock::new(None);
static DISCOVERY: Mutex<Option<Arc<DiscoverySnapshot>>> = Mutex::new(None);
static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// RX channels the receiver planner feeds from a multicast group.
static GROUP_INPUTS: Mutex<BTreeSet<u16>> = Mutex::new(BTreeSet::new());

fn set_group_inputs(inputs: BTreeSet<u16>) {
    if let Ok(mut g) = GROUP_INPUTS.lock() {
        *g = inputs;
    }
}

/// The serving native role, if this daemon has one bound now.
pub(crate) fn local() -> Option<Local> {
    LOCAL.read().ok()?.clone()
}

fn set_local(value: Option<Local>) {
    if let Ok(mut l) = LOCAL.write() {
        *l = value;
    }
}

/// Keep the latest discovery snapshot for channel resolution. A no-op
/// unless the role runs.
pub(crate) fn observe_discovery(snapshot: &DiscoverySnapshot) {
    if ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
        if let Ok(mut d) = DISCOVERY.lock() {
            *d = Some(Arc::new(snapshot.clone()));
        }
    }
}

fn discovery() -> Option<Arc<DiscoverySnapshot>> {
    DISCOVERY.lock().ok()?.clone()
}

/// The primary interface, its address and MAC, as the role binds to it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Host {
    interface: String,
    address: Ipv4Addr,
    mac: [u8; 6],
    netmask: Ipv4Addr,
    gateway: Ipv4Addr,
    dns: Ipv4Addr,
}

async fn host(engine: &EngineHandle) -> Result<Host, String> {
    let rp = engine.runtime_plan().await.map_err(|e| e.to_string())?;
    let resolution = crate::netif::resolve(crate::netif::Selection::of(&rp), &crate::netif::scan());
    let interface = resolution
        .primary()
        .ok_or("no primary interface")?
        .to_owned();
    let info = crate::netif::lookup(&interface).map_err(|e| e.to_string())?;
    Ok(Host {
        netmask: netmask(&interface).unwrap_or(Ipv4Addr::UNSPECIFIED),
        gateway: gateway(&interface).unwrap_or(Ipv4Addr::UNSPECIFIED),
        dns: dns().unwrap_or(Ipv4Addr::UNSPECIFIED),
        interface,
        address: info.addr,
        mac: info.mac,
    })
}

/// The host's DNS server (network info 0x0011 +0x1C, `conmon.md` §4.4):
/// the upstream list of a local resolver service first, then
/// `/etc/resolv.conf`. Unspecified when there is none, as one real device
/// sends; never the gateway in its place (`device-settings.md` §7).
fn dns() -> Option<Ipv4Addr> {
    ["/run/systemd/resolve/resolv.conf", "/etc/resolv.conf"]
        .iter()
        .find_map(|path| nameserver(&std::fs::read_to_string(path).ok()?))
}

/// The first IPv4 `nameserver` of a resolv.conf text that is not a local
/// stub resolver (loopback).
fn nameserver(text: &str) -> Option<Ipv4Addr> {
    text.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        if words.next() != Some("nameserver") {
            return None;
        }
        let ip: Ipv4Addr = words.next()?.parse().ok()?;
        (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
    })
}

/// Link speed of `interface` in Mb/s, as the kernel reports it; 1000, the
/// value the role sent before it read the kernel, when it reports none
/// (Wi-Fi, some virtual interfaces).
fn link_speed(interface: &str) -> u16 {
    std::fs::read_to_string(format!("/sys/class/net/{interface}/speed"))
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
        .map_or(1000, |v| u16::try_from(v).unwrap_or(u16::MAX))
}

fn netmask(interface: &str) -> Option<Ipv4Addr> {
    nix::ifaddrs::getifaddrs().ok()?.find_map(|ifa| {
        if ifa.interface_name != interface || ifa.address?.as_sockaddr_in().is_none() {
            return None;
        }
        Some(ifa.netmask?.as_sockaddr_in()?.ip())
    })
}

/// The default gateway of `interface`, from the kernel routing table.
fn gateway(interface: &str) -> Option<Ipv4Addr> {
    let table = std::fs::read_to_string("/proc/net/route").ok()?;
    table.lines().skip(1).find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 || f[0] != interface || f[1] != "00000000" {
            return None;
        }
        let raw = u32::from_str_radix(f[2], 16).ok()?;
        Some(Ipv4Addr::from(raw.swap_bytes()))
    })
}

/// The latency properties of `model` (ARC 0x1100/0x1102,
/// `device-settings.md` §§4.2, 4.4). The configured latency is reported as
/// the default receive latency (0x8204); the active latency stands in when
/// the configuration cannot be read.
async fn latency_properties(
    engine: &EngineHandle,
    model: &tessera_state::model::DeviceModel,
) -> wire::Properties {
    let configured = engine.runtime_plan().await.ok().and_then(|rp| {
        rp.config
            .devices
            .iter()
            .find(|d| d.id == model.key.0)
            .map(|d| d.latency_us)
    });
    let ns = |us: u32| us.saturating_mul(1000);
    let [lo, hi] = model.latency_range_us();
    wire::Properties {
        default_latency_ns: ns(configured.unwrap_or(model.latency_us)),
        latency_ns: ns(model.latency_us),
        latency_range_ns: [ns(lo), ns(hi)],
        latency_writable: !model
            .locked
            .contains(&tessera_state::config::DeviceLock::Latency),
    }
}

/// The ARC view of the first device and its model, read fresh.
async fn device(
    engine: &EngineHandle,
) -> Result<(wire::DeviceView, tessera_state::model::DeviceModel), CommandError> {
    let rp = engine.runtime_plan().await?;
    let d = rp
        .model
        .devices
        .first()
        .cloned()
        .ok_or_else(|| CommandError::Internal {
            message: "no device".into(),
        })?;
    let states = match engine.query(Query::Status).await? {
        Reply::Status(s) => s
            .devices
            .into_iter()
            .find(|x| x.key == d.key)
            .map(|x| x.subscriptions)
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let groups = GROUP_INPUTS.lock().map(|g| g.clone()).unwrap_or_default();
    let rx = (1..=d.rx_channels)
        .map(|ch| {
            let source = d
                .subscriptions
                .get(&ch)
                .map(|s| (s.tx_channel.clone(), s.tx_device.clone()));
            let state = states
                .iter()
                .find(|s| s.rx_channel.get() == ch)
                .map(|s| &s.state);
            let status = match (&source, state) {
                (None, _) => wire::status::NONE,
                (Some(_), Some(SubscriptionState::Receiving)) if groups.contains(&ch) => {
                    wire::status::RECEIVING_MULTICAST
                }
                (Some(_), Some(SubscriptionState::Receiving)) => wire::status::RECEIVING,
                (Some(_), Some(SubscriptionState::InProgress | SubscriptionState::NoMedia)) => {
                    wire::status::WAITING
                }
                (
                    Some(_),
                    Some(
                        SubscriptionState::FlowRefused { .. }
                        | SubscriptionState::RateMismatch { .. }
                        | SubscriptionState::EncodingMismatch { .. },
                    ),
                ) => wire::status::TX_FAIL,
                (Some(_), _) => wire::status::UNRESOLVED,
            };
            wire::RxRow {
                label: d.rx_label(ch),
                source,
                status,
            }
        })
        .collect();
    let view = wire::DeviceView {
        name: d.name.clone(),
        rate: d.sample_rate,
        bits: u16::from(d.encoding),
        latency_ns: d.latency_us.saturating_mul(1000),
        tx_labels: (1..=d.tx_channels).map(|ch| d.tx_label(ch)).collect(),
        rx,
    };
    Ok((view, d))
}

/// State shared by the listeners and the planner.
#[derive(Debug, Default)]
struct Shared {
    /// Receive flows for ARC 0x3200.
    flows: Mutex<Vec<wire::RxFlowView>>,
    /// Wakes the planner after a subscription write.
    changed: tokio::sync::Notify,
    /// RX channels whose state controllers should read again.
    notices: Mutex<Vec<u16>>,
    notice: tokio::sync::Notify,
    /// Transmit sessions for ARC 0x2200.
    tx_flows: Mutex<Vec<wire::TxFlowView>>,
    /// The receive and transmit halves of the protocol flow set, which is
    /// always installed whole.
    rx_specs: Mutex<Vec<FlowSpec>>,
    tx_specs: Mutex<Vec<FlowSpec>>,
    installing: tokio::sync::Mutex<()>,
    /// Header sequence of every device-info message (replies and
    /// notifications); heartbeats count apart (`conmon.md` §2).
    info_sequence: std::sync::atomic::AtomicU16,
    /// The heartbeat's latest interface sample (TX bytes/s, RX bytes/s, TX
    /// errors, RX errors) for info 0x0040 (`device-settings.md` §8).
    traffic: Mutex<[u32; 4]>,
}

impl Shared {
    /// Replace one or both halves and install the complete protocol flow
    /// set. A refusal changes neither stored half.
    async fn install(
        &self,
        engine: &EngineHandle,
        rx: Option<Vec<FlowSpec>>,
        tx: Option<Vec<FlowSpec>>,
    ) -> Result<(), CommandError> {
        let _one = self.installing.lock().await;
        let current = |m: &Mutex<Vec<FlowSpec>>| m.lock().map(|v| v.clone()).unwrap_or_default();
        let rx = rx.unwrap_or_else(|| current(&self.rx_specs));
        let tx = tx.unwrap_or_else(|| current(&self.tx_specs));
        let flows = rx.iter().chain(&tx).cloned().collect();
        engine
            .apply(Origin::Internal, Command::SetProtocolFlows { flows })
            .await?;
        if let Ok(mut m) = self.rx_specs.lock() {
            *m = rx;
        }
        if let Ok(mut m) = self.tx_specs.lock() {
            *m = tx;
        }
        Ok(())
    }

    fn notify_channels(&self, channels: impl IntoIterator<Item = u16>) {
        if let Ok(mut n) = self.notices.lock() {
            n.extend(channels);
        }
        self.notice.notify_one();
    }

    /// The header sequence of the next device-info message.
    fn next_info_sequence(&self) -> u16 {
        self.info_sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1)
    }
}

async fn bind(address: Ipv4Addr, port: u16) -> std::io::Result<UdpSocket> {
    UdpSocket::bind(SocketAddrV4::new(address, port)).await
}

/// Serve the role until `shutdown`. Binds again when the primary address
/// changes; a port held by another process is retried, never shared.
pub async fn run(
    engine: EngineHandle,
    media: Arc<crate::media::stats::MediaStats>,
    settings: Settings,
    events: tessera_state::EventBus,
    shutdown: CancellationToken,
) {
    ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
    if settings.capabilities & access::LOCKABLE != 0 {
        // Only a lab word can set it (`device-lock.md` §9.3 L12).
        tracing::warn!(
            "native lab: the lockable bit is advertised; a controller's Lock and Unlock cannot \
             work (the lock service is not implemented)"
        );
    }
    let shared = Arc::new(Shared::default());
    let mut warned = false;
    while !shutdown.is_cancelled() {
        let current = match host(&engine).await {
            Ok(h) => h,
            Err(e) => {
                if !warned {
                    tracing::warn!("native device role waits for the primary interface: {e}");
                    warned = true;
                }
                sleep_or(&shutdown, Duration::from_secs(2)).await;
                continue;
            }
        };
        let sockets = async {
            Ok::<_, std::io::Error>((
                bind(current.address, ARC_PORT).await?,
                bind(current.address, CMC_PORT).await?,
                bind(current.address, INFO_PORT).await?,
                bind(current.address, FLOW_PORT).await?,
            ))
        }
        .await;
        let (arc_socket, cmc_socket, info_socket, flow_socket) = match sockets {
            Ok(s) => s,
            Err(e) => {
                if !warned {
                    tracing::warn!(
                        "native device role cannot bind its ports on {} ({e}); is the compatibility runtime still running?",
                        current.address
                    );
                    warned = true;
                }
                sleep_or(&shutdown, Duration::from_secs(2)).await;
                continue;
            }
        };
        warned = false;
        let (_, d) = match device(&engine).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("native device role: {e}");
                sleep_or(&shutdown, Duration::from_secs(2)).await;
                continue;
            }
        };
        let mut nonce = [0u8; 8];
        let _ = getrandom::fill(&mut nonce);
        set_local(Some(Local {
            interface: current.interface.clone(),
            address: current.address,
            name: d.name.clone(),
            rx_channels: d.rx_channels,
            incarnation: format!(
                "native-{}",
                nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
            ),
        }));
        tracing::info!(
            "native device role serving on {} ({})",
            current.address,
            current.interface
        );
        let stop = shutdown.child_token();
        let host_changed = {
            let (engine, stop, current) = (engine.clone(), stop.clone(), current.clone());
            async move {
                loop {
                    sleep_or(&stop, Duration::from_secs(5)).await;
                    if stop.is_cancelled() {
                        return;
                    }
                    if host(&engine)
                        .await
                        .is_ok_and(|h| h.address != current.address)
                    {
                        return;
                    }
                }
            }
        };
        let info_socket = Arc::new(info_socket);
        let flow_port = Arc::new(flows::FlowPort::new(
            flow_socket,
            engine.clone(),
            Arc::clone(&shared),
            Arc::clone(&media),
        ));
        flow_port.restore().await;
        let lock_watch = access::watch(
            engine.clone(),
            &events,
            Arc::clone(&info_socket),
            Arc::clone(&shared),
            settings.clone(),
            Arc::clone(&flow_port),
            info::INFO_GROUP,
        );
        let lab_lock_listener = async {
            if settings.lab && settings.lock_listener {
                access::lab_listener(current.address).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            () = lock_watch => {}
            () = lab_lock_listener => {}
            () = shutdown.cancelled() => {}
            () = host_changed => tracing::info!("primary address changed; binding the native role again"),
            () = arc::serve(engine.clone(), Arc::clone(&shared), Arc::clone(&flow_port), arc_socket, settings.lab) => {}
            () = info::serve_cmc(engine.clone(), cmc_socket, current.clone()) => {}
            () = info::serve_info(engine.clone(), Arc::clone(&info_socket), Arc::clone(&shared), current.clone(), settings.clone()) => {}
            () = info::heartbeat(engine.clone(), Arc::clone(&info_socket), Arc::clone(&shared), current.clone(), settings.clone(), Arc::clone(&media)) => {}
            () = Arc::clone(&flow_port).serve() => {}
            () = flow_port.expire_silent() => {}
            () = receiver::run(engine.clone(), Arc::clone(&shared), Arc::clone(&flow_port), current.address, Arc::clone(&media)) => {}
        }
        stop.cancel();
        set_local(None);
    }
}

async fn sleep_or(token: &CancellationToken, d: Duration) {
    tokio::select! {
        () = token.cancelled() => {}
        () = tokio::time::sleep(d) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::nameserver;
    use std::net::Ipv4Addr;

    #[test]
    fn the_dns_server_is_the_first_nameserver_that_is_not_a_local_stub() {
        let stub = "# managed\nnameserver 127.0.0.53\noptions edns0 trust-ad\nsearch lan\n";
        assert_eq!(nameserver(stub), None);
        let upstream = "nameserver 127.0.0.53\nnameserver fd00::1\nnameserver 192.0.2.2\nnameserver 192.0.2.3\n";
        assert_eq!(nameserver(upstream), Some(Ipv4Addr::new(192, 0, 2, 2)));
        assert_eq!(nameserver("nameserver\tbogus\n#nameserver 1.1.1.1\n"), None);
        assert_eq!(nameserver(""), None);
    }
}
