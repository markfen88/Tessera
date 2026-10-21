//! From configuration to running flows (control plane).
//!
//! [`plan_flows`] is a pure function: `[[flow]]` entries plus the effective
//! device model plus an interface lookup give [`FlowPlan`]s (fully resolved
//! flows) and per-flow error messages. [`build_rx`] / [`build_tx`] /
//! [`build_sock`] then allocate everything a flow needs (flow engine, sockets,
//! scratch buffers) so the real-time worker never has to (ADR 0004: the control
//! thread builds objects and hands ownership over).
//!
//! **PROVISIONAL**: static flows are tessera-only test scaffolding until
//! ARC/CMC flow control exists (see `docs/configuration.md`).

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::Arc;

use tessera_control_types::Direction;
use tessera_media::packet::{PacketFormat, frames_for_period_ns, max_frames_per_packet};
use tessera_media::{RxConfig, RxFlow, RxMetrics, TxConfig, TxFlow};
use tessera_state::config::{FlowConfig, FlowFormat};
use tessera_state::model::DeviceModel;

use super::format::AnyFormat;
use super::keepalive::{Keepalive, KeepaliveConfig};
use super::stats::RxFlowCounters;
use super::udp::{RxSock, TxSock};
use super::worker::{RxRt, TxRt, TxSockRt};
use crate::netif::IfInfo;

/// The default packet period when a tx flow does not say `frames_per_packet`
/// (ADR 0011: 32 frames at 48 kHz), nanoseconds.
pub const DEFAULT_PACKET_PERIOD_NS: u64 = 666_667;

/// A value that never makes two plans differ.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ignored<T>(pub T);

impl<T> PartialEq for Ignored<T> {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl<T> Eq for Ignored<T> {}

/// A fully resolved static flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowPlan {
    /// Flow id.
    pub id: String,
    /// Device key.
    pub device: String,
    /// `rx` or `tx`.
    pub direction: Direction,
    /// Device channels (0-based) in wire order.
    pub channels: Vec<u32>,
    /// Sample rate.
    pub rate: u32,
    /// Wire bits per sample.
    pub bits: u8,
    /// Device latency, nanoseconds.
    pub latency_ns: u64,
    /// Largest latency the device may be configured to, nanoseconds.
    pub max_latency_ns: u64,
    /// Local address to bind.
    pub local: Ipv4Addr,
    /// rx: local port; tx: destination port.
    pub port: u16,
    /// tx: destination; rx: source filter.
    pub peer: Option<Ipv4Addr>,
    /// Packet format.
    pub format: FlowFormat,
    /// tx: frames per packet.
    pub fpp: u32,
    /// tx: send offset, nanoseconds.
    pub send_offset_ns: u64,
    /// tx: DSCP.
    pub dscp: u8,
    /// tx: how late a packet may still be sent, nanoseconds, as configured
    /// (`None`: [`default_lag_ns`](Self::default_lag_ns)).
    pub max_lag_ns: Option<u64>,
    /// tx: the default lag, half the device latency but at least one packet
    /// period. Not part of the flow's identity: a latency change does not
    /// rebuild a running tx flow.
    pub default_lag_ns: Ignored<u64>,
    /// The interface the sockets are restricted to.
    pub interface: String,
    /// Its kernel index: a re-created interface (same name, new index) has
    /// new sockets, so the flow is rebuilt.
    pub ifindex: u32,
    /// Largest datagram.
    pub mtu_payload: u32,
    /// rx: the receiver keepalive, resolved from the `[[flow]]` entry and
    /// `[defaults.flow]` (`None`: [`Tuning::keepalive`]). Not part of the
    /// flow's identity: a changed keepalive takes effect when the flow next
    /// (re)starts, it does not rebuild a running flow.
    pub keepalive: Ignored<Option<KeepaliveConfig>>,
}

/// Knobs of the data path (apply class `restart`, `crate::tunables`).
#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    /// Datagrams per `recvmmsg`.
    pub rx_batch: usize,
    /// Datagrams per `sendmmsg`.
    pub tx_batch: usize,
    /// Kernel socket buffer size, bytes.
    pub socket_buffer: usize,
    /// Frames per delivery chunk.
    pub chunk_frames: usize,
    /// Receiver keepalives of rx flows.
    pub keepalive: KeepaliveConfig,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            rx_batch: 32,
            tx_batch: 32,
            socket_buffer: 2 << 20,
            chunk_frames: 512,
            keepalive: KeepaliveConfig::default(),
        }
    }
}

/// Resolve the configured flows. Flows that cannot be resolved are returned
/// as `(id, message)`; the others as plans.
///
/// `interfaces` is the effective list of media interfaces; `lookup` finds an
/// interface by name.
#[must_use]
pub fn plan_flows(
    flows: &[FlowConfig],
    devices: &[DeviceModel],
    interfaces: &[String],
    allow_wireless: bool,
    mtu_payload: u32,
    lookup: &dyn Fn(&str) -> io::Result<IfInfo>,
) -> (Vec<FlowPlan>, Vec<(String, String)>) {
    let mut plans = Vec::new();
    let mut errors = Vec::new();
    for f in flows {
        match plan_one(f, devices, interfaces, allow_wireless, mtu_payload, lookup) {
            Ok(p) => plans.push(p),
            Err(e) => errors.push((f.id.clone(), e)),
        }
    }
    (plans, errors)
}

fn plan_one(
    f: &FlowConfig,
    devices: &[DeviceModel],
    interfaces: &[String],
    allow_wireless: bool,
    mtu_payload: u32,
    lookup: &dyn Fn(&str) -> io::Result<IfInfo>,
) -> Result<FlowPlan, String> {
    let dev = devices
        .iter()
        .find(|d| d.key.0 == f.device)
        .ok_or_else(|| format!("no device `{}`", f.device))?;
    let count = match f.direction {
        Direction::Rx => dev.rx_channels,
        Direction::Tx => dev.tx_channels,
    };
    let channels: Vec<u32> = if f.channels.is_empty() {
        (0..u32::from(count)).collect()
    } else {
        f.channels.iter().map(|c| u32::from(*c) - 1).collect()
    };
    if channels.is_empty() {
        return Err("the flow carries no channels".into());
    }
    if channels.iter().any(|c| *c >= u32::from(count)) {
        return Err(format!(
            "a channel is beyond the device's {count} channel(s) in that direction"
        ));
    }
    let iface_name = f
        .interface
        .clone()
        .or_else(|| interfaces.first().cloned())
        .ok_or("no media interface is configured or available")?;
    let info = lookup(&iface_name).map_err(|e| e.to_string())?;
    if info.wireless && !allow_wireless {
        return Err(format!(
            "`{iface_name}` is a wireless interface (set [network] allow_wireless to use it)"
        ));
    }
    let local = f.local_address.unwrap_or(info.addr);
    let probe = AnyFormat::build(f.format, channels.len(), dev.sample_rate, dev.encoding)?;
    let fit = max_frames_per_packet(
        probe.header_len(),
        channels.len(),
        probe.bytes_per_sample(),
        mtu_payload as usize,
    );
    if fit == 0 {
        return Err(format!(
            "{} channel(s) do not fit one frame into a {mtu_payload}-byte packet",
            channels.len()
        ));
    }
    let fpp = match f.direction {
        Direction::Rx => 0,
        Direction::Tx => {
            let want = f.frames_per_packet.map_or_else(
                || frames_for_period_ns(dev.sample_rate, DEFAULT_PACKET_PERIOD_NS).min(fit),
                |n| n as usize,
            );
            if want > fit {
                return Err(format!(
                    "{want} frames per packet exceed the {fit} that fit {} channel(s) in {mtu_payload} bytes",
                    channels.len()
                ));
            }
            u32::try_from(want).map_err(|_| "frames_per_packet is too large".to_owned())?
        }
    };
    // Only what a direction uses goes into the plan, so that a change of
    // something else (the device latency for a tx flow, say) does not rebuild
    // the flow.
    let (rx, tx) = (f.direction == Direction::Rx, f.direction == Direction::Tx);
    let latency_ns = u64::from(dev.latency_us) * 1_000;
    let period_ns = u64::from(fpp) * 1_000_000_000 / u64::from(dev.sample_rate.max(1));
    Ok(FlowPlan {
        id: f.id.clone(),
        device: f.device.clone(),
        direction: f.direction,
        channels,
        rate: dev.sample_rate,
        bits: dev.encoding,
        latency_ns: if rx { latency_ns } else { 0 },
        max_latency_ns: if rx {
            u64::from(dev.latency_bounds_us[1].max(dev.latency_us)) * 1_000
        } else {
            0
        },
        local,
        port: f.port,
        peer: if f.any_source { None } else { f.peer },
        format: f.format,
        fpp,
        send_offset_ns: if tx {
            u64::from(f.send_offset_us) * 1_000
        } else {
            0
        },
        dscp: if tx { f.dscp } else { 0 },
        max_lag_ns: f.max_lag_us.filter(|_| tx).map(|u| u64::from(u) * 1_000),
        default_lag_ns: Ignored(period_ns.max(latency_ns / 2)),
        interface: info.name.clone(),
        ifindex: info.index,
        mtu_payload,
        keepalive: Ignored(None),
    })
}

/// The key that identifies a shared TX socket.
#[must_use]
pub fn sock_key(p: &FlowPlan) -> (Ipv4Addr, String, u32, u8, u32) {
    (
        p.local,
        p.interface.clone(),
        p.ifindex,
        p.dscp,
        p.mtu_payload,
    )
}

/// Build an rx flow. `key` and `device_ix` are assigned by the caller.
///
/// # Errors
/// An invalid configuration or a socket failure, as text.
pub fn build_rx(
    p: &FlowPlan,
    key: u64,
    device_ix: u32,
    t: &Tuning,
    reuse: Option<UdpSocket>,
) -> Result<RxBuilt, String> {
    let fmt = AnyFormat::build(p.format, p.channels.len(), p.rate, p.bits)?;
    let fit = max_frames_per_packet(
        fmt.header_len(),
        p.channels.len(),
        fmt.bytes_per_sample(),
        p.mtu_payload as usize,
    );
    let mut cfg = RxConfig::new(p.rate, p.channels.len());
    cfg.latency_ns = p.latency_ns;
    cfg.max_latency_ns = p.max_latency_ns;
    cfg.max_frames_per_packet = fit.max(1);
    let cap = fmt.encoded_len(cfg.max_frames_per_packet);
    let flow = RxFlow::new(cfg, fmt).map_err(|e| e.to_string())?;
    let metrics = flow.metrics();
    let counters = Arc::new(RxFlowCounters::default());
    let sock = match reuse {
        Some(s) => RxSock::from_socket(s, t.socket_buffer, t.rx_batch, cap),
        None => RxSock::bind(
            SocketAddrV4::new(p.local, p.port),
            &p.interface,
            t.socket_buffer,
            t.rx_batch,
            cap,
        ),
    }
    .map_err(|e| format!("cannot listen on {}:{}: {e}", p.local, p.port))?;
    let dup = sock
        .try_clone_socket()
        .map_err(|e| format!("cannot duplicate the socket: {e}"))?;
    let rcvbuf = sock.effective_rcvbuf();
    let chunk = t.chunk_frames.max(1);
    Ok(RxBuilt {
        rcvbuf,
        dup,
        metrics: Arc::clone(&metrics),
        counters: Arc::clone(&counters),
        rt: Box::new(RxRt {
            key,
            device: device_ix,
            rate: p.rate,
            sock,
            flow,
            chan: p.channels.clone(),
            peer: p.peer,
            cursor: None,
            scratch: vec![0; chunk],
            scratch_f: vec![0.0; chunk],
            metrics: Arc::clone(&metrics),
            last_rx: 0,
            seen: false,
            stalled: false,
            keepalive: Keepalive::new(p.keepalive.0.unwrap_or(t.keepalive)),
            counters: Arc::clone(&counters),
        }),
    })
}

/// A built rx flow with what the control plane keeps.
#[derive(Debug)]
pub struct RxBuilt {
    /// The flow for the worker.
    pub rt: Box<RxRt>,
    /// The flow engine's counters.
    pub metrics: Arc<RxMetrics>,
    /// A second descriptor of the flow's socket: the control plane hands it to
    /// a replacement flow on the same port, so the port is never unbound.
    pub dup: UdpSocket,
    /// The flow's keepalive counters.
    pub counters: Arc<RxFlowCounters>,
    /// The kernel's receive buffer of the socket, bytes.
    pub rcvbuf: usize,
}

/// Build a tx flow (the shared socket is built by [`build_sock`]).
///
/// # Errors
/// An invalid configuration, as text.
pub fn build_tx(p: &FlowPlan, key: u64, device_ix: u32, sock_key: u64) -> Result<TxBuilt, String> {
    let dest = p
        .peer
        .ok_or_else(|| "a tx flow needs a destination address".to_owned())?;
    let fmt = AnyFormat::build(p.format, p.channels.len(), p.rate, p.bits)?;
    let mut cfg = TxConfig::new(p.rate, p.channels.len(), p.fpp as usize);
    cfg.send_offset_ns = p.send_offset_ns;
    cfg.max_lag_ns = p.max_lag_ns.or(Some(p.default_lag_ns.0.max(1)));
    cfg.max_payload = p.mtu_payload as usize;
    let flow = TxFlow::new(cfg, fmt).map_err(|e| e.to_string())?;
    let metrics = flow.metrics();
    let send_failures = Arc::new(tessera_media::Counter::new());
    Ok(TxBuilt {
        rt: Box::new(TxRt {
            key,
            device: device_ix,
            sock_key,
            sock: usize::MAX,
            flow,
            chan: p.channels.clone(),
            dest: SocketAddrV4::new(dest, p.port),
            send_failures: Arc::clone(&send_failures),
            scratch_f: vec![0.0; p.fpp as usize],
        }),
        metrics,
        send_failures,
    })
}

/// A built tx flow with the counters the control plane keeps.
#[derive(Debug)]
pub struct TxBuilt {
    /// The flow for the worker.
    pub rt: Box<TxRt>,
    /// The flow engine's counters.
    pub metrics: Arc<tessera_media::TxMetrics>,
    /// Datagrams of this flow the kernel refused.
    pub send_failures: Arc<tessera_media::Counter>,
}

/// Build a TX socket for flows with this plan's local address and DSCP.
///
/// # Errors
/// A socket failure, as text.
pub fn build_sock(p: &FlowPlan, key: u64, t: &Tuning) -> Result<Box<TxSockRt>, String> {
    let sock = TxSock::open(
        p.local,
        &p.interface,
        p.dscp,
        t.socket_buffer,
        t.tx_batch,
        p.mtu_payload as usize,
    )
    .map_err(|e| format!("cannot open a send socket on {}: {e}", p.local))?;
    Ok(Box::new(TxSockRt {
        key,
        sock,
        queued: 0,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use tessera_control_types::DeviceKey;
    use tessera_state::model::WireDeviceId;

    fn dev() -> DeviceModel {
        DeviceModel {
            key: DeviceKey("main".into()),
            wire_device_id: WireDeviceId([1; 8]),
            name: "n".into(),
            sample_rate: 48_000,
            encoding: 24,
            latency_us: 4_000,
            rx_channels: 8,
            tx_channels: 4,
            rx_labels: BTreeMap::new(),
            tx_labels: BTreeMap::new(),
            subscriptions: BTreeMap::new(),
            locked: BTreeSet::new(),
            latency_bounds_us: [1_000, 40_000],
            allow_unsafe_latency: false,
        }
    }

    fn flow(direction: Direction) -> FlowConfig {
        FlowConfig {
            id: "f".into(),
            direction,
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
        }
    }

    fn lookup(name: &str) -> io::Result<IfInfo> {
        if name == "eth0" {
            Ok(IfInfo {
                name: name.into(),
                index: 2,
                addr: Ipv4Addr::new(10, 0, 0, 1),
                mac: [2, 0, 0, 0, 0, 1],
                wireless: false,
            })
        } else if name == "wlan0" {
            Ok(IfInfo {
                name: name.into(),
                index: 3,
                addr: Ipv4Addr::new(10, 0, 0, 9),
                mac: [2, 0, 0, 0, 0, 9],
                wireless: true,
            })
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, "no such interface"))
        }
    }

    fn plan(f: FlowConfig, ifaces: &[&str]) -> (Vec<FlowPlan>, Vec<(String, String)>) {
        let ifaces: Vec<String> = ifaces.iter().map(|s| (*s).to_owned()).collect();
        plan_flows(&[f], &[dev()], &ifaces, false, 1400, &lookup)
    }

    #[test]
    fn defaults_come_from_the_device_and_the_first_interface() {
        let (p, e) = plan(flow(Direction::Tx), &["eth0"]);
        assert!(e.is_empty(), "{e:?}");
        let p = &p[0];
        assert_eq!(p.channels, vec![0, 1, 2, 3], "all tx channels, 0-based");
        assert_eq!(p.local, Ipv4Addr::new(10, 0, 0, 1));
        assert_eq!(p.fpp, 32, "667 us at 48 kHz");
        // A tx flow does not use the receive latency: it is not in its plan.
        assert_eq!((p.rate, p.bits, p.latency_ns), (48_000, 24, 0));
        assert_eq!(p.interface, "eth0");
        let (p, _) = plan(flow(Direction::Rx), &["eth0"]);
        assert_eq!(p[0].channels.len(), 8);
        assert_eq!(p[0].fpp, 0);
        assert_eq!(p[0].latency_ns, 4_000_000);
        assert_eq!(p[0].max_latency_ns, 40_000_000);
    }

    #[test]
    fn explicit_channels_are_one_based_in_config_and_zero_based_in_the_plan() {
        let mut f = flow(Direction::Rx);
        f.channels = vec![3, 1];
        let (p, e) = plan(f, &["eth0"]);
        assert!(e.is_empty());
        assert_eq!(p[0].channels, vec![2, 0], "wire order is kept");
    }

    #[test]
    fn the_packet_must_fit_the_mtu() {
        let mut f = flow(Direction::Tx);
        f.frames_per_packet = Some(200);
        f.channels = vec![1, 2, 3, 4];
        let (p, e) = plan(f, &["eth0"]);
        assert!(p.is_empty());
        assert!(e[0].1.contains("exceed"), "{e:?}");
        // The default shrinks to fit instead of failing.
        let mut d = dev();
        d.tx_channels = 64;
        let (p, e) = plan_flows(
            &[flow(Direction::Tx)],
            &[d],
            &["eth0".to_owned()],
            false,
            1400,
            &lookup,
        );
        assert!(e.is_empty(), "{e:?}");
        assert!(p[0].fpp >= 1 && p[0].fpp < 32, "{}", p[0].fpp);
    }

    #[test]
    fn unresolvable_flows_are_reported_not_fatal() {
        let (p, e) = plan(flow(Direction::Tx), &["nope0"]);
        assert!(p.is_empty() && e.len() == 1);
        let (_, e) = plan(flow(Direction::Tx), &["wlan0"]);
        assert!(e[0].1.contains("wireless"), "{e:?}");
        let (_, e) = plan(flow(Direction::Tx), &[]);
        assert!(e[0].1.contains("no media interface"), "{e:?}");
        let mut f = flow(Direction::Tx);
        f.device = "ghost".into();
        let (_, e) = plan(f, &["eth0"]);
        assert!(e[0].1.contains("no device"), "{e:?}");
        let mut f = flow(Direction::Tx);
        f.channels = vec![9];
        let (_, e) = plan(f, &["eth0"]);
        assert!(e[0].1.contains("beyond"), "{e:?}");
    }

    #[test]
    fn an_interface_on_the_flow_wins() {
        let mut f = flow(Direction::Rx);
        f.interface = Some("eth0".into());
        f.local_address = Some(Ipv4Addr::new(10, 0, 0, 7));
        let (p, e) = plan(f, &["wlan0"]);
        assert!(e.is_empty(), "{e:?}");
        assert_eq!(p[0].local, Ipv4Addr::new(10, 0, 0, 7));
    }

    #[test]
    fn a_flow_replacing_another_on_the_same_port_takes_over_its_socket() {
        let mut f = flow(Direction::Rx);
        f.port = 0;
        let (mut p, _) = plan(f, &["eth0"]);
        p[0].local = Ipv4Addr::LOCALHOST;
        p[0].port = 0;
        p[0].interface = "lo".into();
        let first = build_rx(&p[0], 1, 0, &Tuning::default(), None).unwrap();
        let port = first.rt.sock.local_port().unwrap();
        // Another bind of that port would fail ...
        let mut again = p[0].clone();
        again.port = port;
        assert!(build_rx(&again, 2, 0, &Tuning::default(), None).is_err());
        // ... taking the first flow's socket over works, with new buffers,
        // while the first flow is still alive (it is retired a moment later).
        let second = build_rx(
            &again,
            2,
            0,
            &Tuning::default(),
            Some(first.dup.try_clone().unwrap()),
        )
        .unwrap();
        assert_eq!(second.rt.sock.local_port().unwrap(), port);
        drop(first.rt);
        drop(first.dup);
        assert_eq!(second.rt.sock.local_port().unwrap(), port, "still bound");
    }

    #[test]
    fn what_a_direction_does_not_use_does_not_make_two_plans_differ() {
        let (tx_a, _) = plan(flow(Direction::Tx), &["eth0"]);
        let mut slower = dev();
        slower.latency_us = 20_000;
        let (tx_b, _) = plan_flows(
            &[flow(Direction::Tx)],
            &[slower.clone()],
            &["eth0".to_owned()],
            false,
            1400,
            &lookup,
        );
        assert_eq!(tx_a, tx_b, "a latency change must not rebuild a tx flow");
        // ... but the lag default follows it, for flows that start later.
        assert_ne!(tx_a[0].default_lag_ns.0, tx_b[0].default_lag_ns.0);
        let (rx_a, _) = plan(flow(Direction::Rx), &["eth0"]);
        let (rx_b, _) = plan_flows(
            &[flow(Direction::Rx)],
            &[slower],
            &["eth0".to_owned()],
            false,
            1400,
            &lookup,
        );
        assert_ne!(rx_a, rx_b, "an rx flow does depend on the latency");
        // A rate change rebuilds both.
        let mut fast = dev();
        fast.sample_rate = 96_000;
        let (tx_c, _) = plan_flows(
            &[flow(Direction::Tx)],
            &[fast],
            &["eth0".to_owned()],
            false,
            1400,
            &lookup,
        );
        assert_ne!(tx_a, tx_c);
    }

    #[test]
    fn the_default_lag_is_half_the_latency_but_at_least_one_packet_period() {
        let (p, _) = plan(flow(Direction::Tx), &["eth0"]);
        // 4 ms latency: 2 ms; a packet period is 667 us.
        assert_eq!(p[0].default_lag_ns.0, 2_000_000);
        let mut quick = dev();
        quick.latency_us = 1_000;
        let (p, _) = plan_flows(
            &[flow(Direction::Tx)],
            &[quick],
            &["eth0".to_owned()],
            false,
            1400,
            &lookup,
        );
        assert_eq!(
            p[0].default_lag_ns.0, 666_666,
            "never below one packet period"
        );
        let mut f = flow(Direction::Tx);
        f.max_lag_us = Some(3_000);
        let (p, _) = plan(f, &["eth0"]);
        assert_eq!(p[0].max_lag_ns, Some(3_000_000));
    }

    #[test]
    fn built_flows_have_sockets_and_scratch() {
        let mut f = flow(Direction::Rx);
        f.port = 0;
        let (mut p, _) = plan(f, &["eth0"]);
        p[0].local = Ipv4Addr::LOCALHOST;
        p[0].port = 0;
        p[0].interface = "lo".into();
        let b = build_rx(&p[0], 1, 0, &Tuning::default(), None).unwrap();
        let (rx, m) = (b.rt, b.metrics);
        assert_eq!(rx.chan.len(), 8);
        assert_eq!(m.snapshot().packets, 0);
        assert_eq!(rx.scratch.len(), 512);
        assert!(b.rcvbuf > 0);
        let mut t = p[0].clone();
        t.direction = Direction::Tx;
        t.fpp = 32;
        t.channels = vec![0, 1];
        t.peer = Some(Ipv4Addr::LOCALHOST);
        t.port = 9;
        let sock = build_sock(&t, 7, &Tuning::default()).unwrap();
        assert_eq!(sock.sock.batch(), 32);
        let tx = build_tx(&t, 2, 0, 7).unwrap().rt;
        assert_eq!(tx.scratch_f.len(), 32);
        assert_eq!(tx.sock_key, 7);
        t.peer = None;
        assert!(build_tx(&t, 3, 0, 7).is_err());
    }
}
