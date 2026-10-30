#![allow(unsafe_code)]
//! CMC (`control-cmc.md`), device-info replies and the 1 Hz heartbeat
//! (`conmon.md`). Info replies go to the info multicast group, as real
//! devices send them, and to the requester.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::Duration;

use tessera_control_types::{Query, Reply};
use tokio::net::UdpSocket;

use super::wire;
use super::{Host, Settings, Shared, device};
use crate::engine_thread::EngineHandle;

pub(super) const INFO_GROUP: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(224, 0, 0, 231), 8702);
const HEARTBEAT_GROUP: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(224, 0, 0, 233), 8708);

pub(super) async fn serve_cmc(engine: EngineHandle, socket: UdpSocket, host: Host) {
    let mut buf = vec![0u8; 2048];
    loop {
        let Ok((n, peer)) = socket.recv_from(&mut buf).await else {
            continue;
        };
        let Some((header, content)) = wire::request(&buf[..n]) else {
            continue;
        };
        let Ok((_, d)) = device(&engine).await else {
            continue;
        };
        let reply = match header.opcode {
            0x1001 => wire::cmc_advertisement(d.wire_device_id.0, host.address, super::INFO_PORT),
            // Device View session (§3): echo with our id; last port word zero.
            0x3010 if content.len() >= 10 => {
                let mut c = content.to_vec();
                c[2..10].copy_from_slice(&d.wire_device_id.0);
                let end = c.len();
                if end >= 4 {
                    c[end - 4..end - 2].copy_from_slice(&[0, 0]);
                }
                c
            }
            // Anything else is ignored without a reply (§1).
            _ => continue,
        };
        let _ = socket.send_to(&wire::reply(header, 1, &reply), peer).await;
    }
}

async fn identity(
    engine: &EngineHandle,
    host: &Host,
    settings: &Settings,
    shared: &Shared,
) -> Option<wire::Identity> {
    let (_, d) = device(engine).await.ok()?;
    let (clock, clock_settings, formats) = match engine.query(Query::Status).await.ok()? {
        Reply::Status(s) => (
            s.clocks.into_iter().find(|c| c.interface == host.interface),
            s.clock_settings,
            s.devices
                .into_iter()
                .find(|x| x.key == d.key)
                .and_then(|x| x.formats)
                .map(|f| *f),
        ),
        _ => (None, None, None),
    };
    // The 0x0080/0x0082 lists are the values the typed commands accept now
    // (`device-settings.md` §3.3 items 2-3); only the current value when the
    // state does not say.
    let (rates, encodings) = match formats {
        Some(f) => (
            f.sample_rates,
            f.encodings.into_iter().map(u32::from).collect(),
        ),
        None => (vec![d.sample_rate], vec![u32::from(d.encoding)]),
    };
    let follower_only = clock_settings
        .as_ref()
        .is_some_and(|c| c.role == tessera_control_types::ClockRole::Follower);
    let leader = clock
        .as_ref()
        .and_then(|c| c.grandmaster.as_deref())
        .and_then(|g| {
            let hex: String = g.chars().filter(char::is_ascii_hexdigit).take(12).collect();
            let mut mac = [0u8; 6];
            for (i, b) in mac.iter_mut().enumerate() {
                *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
            }
            Some(mac)
        });
    let version = {
        let mut parts = tessera_control_types::BUILD_VERSION
            .split(|c: char| !c.is_ascii_digit())
            .filter_map(|p| p.parse::<u16>().ok());
        (
            parts.next().unwrap_or(0) as u8,
            parts.next().unwrap_or(0) as u8,
            parts.next().unwrap_or(0),
        )
    };
    Some(wire::Identity {
        device_id: d.wire_device_id.0,
        mac: host.mac,
        address: host.address,
        netmask: host.netmask,
        gateway: host.gateway,
        dns: host.dns,
        link_mbps: super::link_speed(&host.interface),
        traffic: shared.traffic.lock().map(|t| *t).unwrap_or_default(),
        board: "tessera".into(),
        manufacturer: "tessera".into(),
        model: "tesserad".into(),
        version,
        rate: d.sample_rate,
        bits: u32::from(d.encoding),
        ppb: clock
            .as_ref()
            .map_or(0, |c| i32::try_from(c.rate_ppb).unwrap_or(0)),
        port_state: match clock.as_ref().map(|c| &c.state) {
            Some(tessera_control_types::ClockState::Leader) => wire::port_state::LEADER,
            Some(
                tessera_control_types::ClockState::Locked
                | tessera_control_types::ClockState::Acquiring
                | tessera_control_types::ClockState::Holdover,
            ) => wire::port_state::FOLLOWER,
            _ => wire::port_state::LISTENING,
        },
        leader,
        capabilities: settings.capabilities,
        clearing: 0,
        rates,
        encodings,
        // The PTP engine advertises "preferred" below priority 64.
        preferred: clock_settings
            .as_ref()
            .is_some_and(|c| c.priority < tessera_control_types::PREFERRED_LEADER_BELOW),
        follower_only,
        // What the PTP port advertises as its own stratum.
        stratum: match clock_settings.as_ref() {
            Some(c) if !follower_only => crate::ptp::priority_clock(c.priority).stratum,
            _ => tessera_clock::provisional::FOLLOWER_STRATUM,
        },
        // Lab capture mode offers the clock controls of the leader device
        // (0x02B4), so a controller sends those writes (`device-settings.md`
        // §12.3). Never the product value.
        clock_capabilities: if settings.lab {
            0x02B4
        } else {
            wire::CLOCK_CAPABILITIES
        },
    })
}

fn multicast_on(socket: &UdpSocket, local: Ipv4Addr) {
    let addr = nix::libc::in_addr {
        s_addr: u32::from(local).to_be(),
    };
    // SAFETY: `addr` outlives the call and its size is passed; the descriptor
    // is open for the lifetime of `socket`.
    unsafe {
        nix::libc::setsockopt(
            socket.as_raw_fd(),
            nix::libc::IPPROTO_IP,
            nix::libc::IP_MULTICAST_IF,
            (&raw const addr).cast(),
            size_of::<nix::libc::in_addr>() as nix::libc::socklen_t,
        );
    }
    let _ = socket.set_multicast_ttl_v4(1);
}

pub(super) async fn serve_info(
    engine: EngineHandle,
    socket: Arc<UdpSocket>,
    shared: Arc<Shared>,
    host: Host,
    settings: Settings,
) {
    multicast_on(&socket, host.address);
    // Board and product info once at start (`state-machines.md` §1), with
    // the sample-rate and encoding records, so a controller's cached ones
    // are refreshed (`device-settings.md` §3.3 item 7).
    // Last sample rate (0x0081) and encoding (0x0083) sent, each tracked on
    // its own so that answering one never hides a change of the other.
    let mut announced: [(u16, Option<u32>); 2] = [(0x0081, None), (0x0083, None)];
    if let Some(me) = identity(&engine, &host, &settings, &shared).await {
        for request in START_ANNOUNCEMENTS {
            announce(&socket, &settings, &me, &shared, request).await;
        }
        for (request, last) in &mut announced {
            *last = Some(format_value(*request, &me));
        }
    }
    // The clear-configuration mode accepted, reported in 0x0078 until the
    // role restarts (`device-settings.md` §12.5a).
    let mut clearing = 0;
    let mut buf = vec![0u8; 2048];
    let mut tick = tokio::time::interval(FORMAT_CHECK);
    loop {
        let (n, peer) = tokio::select! {
            r = socket.recv_from(&mut buf) => match r {
                Ok(r) => r,
                Err(_) => continue,
            },
            _ = tick.tick() => {
                // A rate or encoding change from any source is announced
                // with its own status record (`device-settings.md` §3.2).
                if let Some(me) = identity(&engine, &host, &settings, &shared).await {
                    for (request, last) in &mut announced {
                        let now = format_value(*request, &me);
                        if last.is_some_and(|v| v != now) {
                            announce(&socket, &settings, &me, &shared, *request).await;
                        }
                        *last = Some(now);
                    }
                }
                continue;
            }
        };
        let Some(request) = wire::info_request(&buf[..n]) else {
            continue;
        };
        // The lock state comes from the access lock (`device-lock.md` §9.3 L10).
        if request == super::access::REQUEST {
            super::access::answer(&engine, &socket, &settings, &shared, peer, INFO_GROUP).await;
            continue;
        }
        let Some(me) = identity(&engine, &host, &settings, &shared).await else {
            continue;
        };
        let mut restart = false;
        // Writes and actions (`device-settings.md` §§6, 9, 12.3-12.6), one
        // typed command each; the reply is built from the state after it.
        let me = match request {
            0x0081 | 0x0083 => match wire::value_write(&buf[..n]) {
                Some(write) => {
                    apply_format(&engine, request, write, peer).await;
                    identity(&engine, &host, &settings, &shared)
                        .await
                        .unwrap_or(me)
                }
                None => me,
            },
            // Identify (`device-settings.md` §6): visible, never audible: a
            // notice on the portal's status page and in the log.
            0x0063 => {
                tracing::warn!("IDENTIFY requested by {peer}");
                engine.report_notice(
                    "identify",
                    format!(
                        "Identify: a controller at {} asked this device to identify itself",
                        peer.ip()
                    ),
                    settings.identify_for,
                );
                me
            }
            // A non-zero change mask at +0x00 or the second one at +0x18
            // makes a write (§12.3). Only preferred leader is applied; any
            // other field is answered with the unchanged record.
            0x0021
                if [32, 32 + 0x18]
                    .iter()
                    .any(|at| wire::be16(&buf[..n], *at).is_some_and(|m| m != 0)) =>
            {
                tracing::info!("clock write from {peer}: {}", hex(&buf[32..n]));
                if let Some(on) = wire::preferred_write(&buf[..n]) {
                    let command = tessera_control_types::Command::SetClockPriority {
                        priority: if on {
                            PREFERRED_PRIORITY
                        } else {
                            DEFAULT_PRIORITY
                        },
                    };
                    let origin = tessera_control_types::Origin::Protocol {
                        peer: peer.to_string(),
                    };
                    if let Err(e) = engine.apply(origin, command).await {
                        tracing::info!("preferred leader {on} from {peer} refused: {e}");
                    }
                }
                identity(&engine, &host, &settings, &shared)
                    .await
                    .unwrap_or(me)
            }
            // Network configuration writes (§§7, 12.8) are not implemented:
            // how 0x0011 reports a staged configuration is unknown, so a
            // controller could not verify one. The unchanged record refuses
            // it by readback (§12.0).
            0x0013 if n > 40 => {
                tracing::info!(
                    "network configuration write from {peer} not applied (not supported): {}",
                    hex(&buf[32..n])
                );
                me
            }
            // Reboot and factory reset (§§6, 12.5): no reply.
            0x0090 => {
                match wire::reset_mode(&buf[..n]) {
                    // A reboot has no typed command: the role asks the access
                    // lock itself (`device-lock.md` §9.2 L8).
                    Some(0) if super::access::locked_now(&engine).await => {
                        tracing::warn!("reboot requested by {peer} refused: device access lock");
                    }
                    Some(0) => {
                        tracing::warn!("reboot requested by {peer}");
                        super::request_restart();
                    }
                    Some(1) => {
                        if clear_configuration(&engine, false, peer).await {
                            super::request_restart();
                        }
                    }
                    mode => tracing::warn!("reset mode {mode:?} requested by {peer} refused"),
                }
                continue;
            }
            // Clear configuration (§§9, 12.5a): a write, never answered with
            // the unchanged record as if nothing happened. Accepted: the
            // record with the accepted mode, then the role restarts.
            0x0077 => match wire::clear_mode(&buf[..n]) {
                Some(mode @ (wire::clear::ALL | wire::clear::KEEP_NETWORK)) => {
                    if clear_configuration(&engine, mode == wire::clear::KEEP_NETWORK, peer).await {
                        clearing = mode;
                        restart = true;
                    }
                    identity(&engine, &host, &settings, &shared)
                        .await
                        .unwrap_or(me)
                }
                Some(mode) => {
                    tracing::warn!("clear configuration mode {mode} from {peer} refused");
                    me
                }
                None => me,
            },
            _ => me,
        };
        let me = wire::Identity { clearing, ..me };
        if settings.lab && (n > 40 || wire::info_reply(request, &me).is_none()) {
            tracing::info!(
                "native lab: info request {request:#06x} from {peer}: {}",
                hex(&buf[..n])
            );
        }
        if let Some((_, last)) = announced.iter_mut().find(|(r, _)| *r == request) {
            *last = Some(format_value(request, &me));
        }
        let sent = announce(&socket, &settings, &me, &shared, request).await;
        if let (Some(m), SocketAddr::V4(peer)) = (sent, peer) {
            let _ = socket.send_to(&m, peer).await;
        }
        if restart {
            super::request_restart();
        }
    }
}

/// The value a format request reports: the sample rate for 0x0081, the
/// encoding for 0x0083.
fn format_value(request: u16, me: &wire::Identity) -> u32 {
    if request == 0x0081 { me.rate } else { me.bits }
}

/// Multicast the status record that answers `request` to the info group;
/// the datagram sent, if the request has an answer.
async fn announce(
    socket: &UdpSocket,
    settings: &Settings,
    me: &wire::Identity,
    shared: &Shared,
    request: u16,
) -> Option<Vec<u8>> {
    let (id, content) = wire::info_reply(request, me)?;
    let m = wire::info_message(
        0xFFFF,
        shared.next_info_sequence(),
        me.device_id,
        &settings.vendor,
        wire::info_opcode(id),
        &content,
    );
    let _ = socket.send_to(&m, INFO_GROUP).await;
    Some(m)
}

/// Apply a factory reset or clear configuration from `peer` as one typed
/// command (`device-settings.md` §6); whether it was accepted. The host
/// must allow it (`[control] allow_factory_reset`); the caller restarts the
/// role after an acceptance.
async fn clear_configuration(engine: &EngineHandle, keep_network: bool, peer: SocketAddr) -> bool {
    let Ok((_, d)) = device(engine).await else {
        return false;
    };
    let command = tessera_control_types::Command::ClearConfiguration {
        device: d.key.clone(),
        keep_network,
    };
    let origin = tessera_control_types::Origin::Protocol {
        peer: peer.to_string(),
    };
    match engine.apply(origin, command).await {
        Ok(_) => {
            tracing::warn!(
                "configuration cleared by {peer} (network settings kept: {keep_network}); restarting the device role"
            );
            true
        }
        Err(e) => {
            tracing::warn!("clear configuration from {peer} refused: {e}");
            false
        }
    }
}

/// The 1 Hz heartbeat, and channel-change notifications as they are asked
/// for (`conmon.md` §§5-6). Every second from start-up, with or without a
/// receive flow, two datagram kinds (`metering.md` §2.6 items 2, 6, 7):
/// [0x8001, 0x8000, 0x8003, 0x8004], then signal presence 0x8002 alone.
/// One header counter for every heartbeat datagram, and a section counter
/// per datagram kind, as real devices keep (§§2, 6.1); notifications count
/// with the other device-info messages.
pub(super) async fn heartbeat(
    engine: EngineHandle,
    socket: Arc<UdpSocket>,
    shared: Arc<Shared>,
    host: Host,
    settings: Settings,
    media: Arc<crate::media::stats::MediaStats>,
) {
    let mut header: u16 = 0;
    let mut beats: u16 = 0;
    let mut signal_beats: u16 = 0;
    let mut traffic = Traffic::default();
    // The RX channel states of the previous beat: a change of a channel's
    // receiving status is announced (§5).
    let mut statuses: Option<Vec<u32>> = None;
    // The first beat follows the board and product info sent at start (§3).
    let mut tick = tokio::time::interval_at(
        tokio::time::Instant::now() + HEARTBEAT_START,
        Duration::from_secs(1),
    );
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let sample = traffic.sample(&host.interface);
                if let Ok(mut t) = shared.traffic.lock() {
                    *t = sample;
                }
                let Some(me) = identity(&engine, &host, &settings, &shared).await else { continue };
                let view = device(&engine).await.ok();
                let flows = shared.flows.lock().map(|f| f.clone()).unwrap_or_default();
                let (slots, late) = latency_slots(&flows, &media, me.rate);
                header = header.wrapping_add(1);
                beats = beats.wrapping_add(1);
                let m = wire::info_message(
                    0xFFFE,
                    header,
                    me.device_id,
                    &settings.vendor,
                    wire::HEARTBEAT_OPCODE,
                    &wire::heartbeat(beats, me.ppb, sample, me.rate, &slots, &late),
                );
                let _ = socket.send_to(&m, HEARTBEAT_GROUP).await;
                if let Some((_, d)) = &view {
                    let (tx, rx) = signal(&media, d);
                    header = header.wrapping_add(1);
                    signal_beats = signal_beats.wrapping_add(1);
                    let m = wire::info_message(
                        0xFFFE,
                        header,
                        me.device_id,
                        &settings.vendor,
                        wire::HEARTBEAT_OPCODE,
                        &wire::heartbeat_signal(signal_beats, &tx, &rx),
                    );
                    let _ = socket.send_to(&m, HEARTBEAT_GROUP).await;
                }
                if let Some((v, _)) = &view {
                    let now: Vec<u32> = v.rx.iter().map(|r| r.status).collect();
                    if let Some(before) = &statuses {
                        let changed = changed_channels(before, &now);
                        if !changed.is_empty() {
                            shared.notify_channels(changed);
                        }
                    }
                    statuses = Some(now);
                }
            }
            () = shared.notice.notified() => {
                let channels: Vec<u16> = shared
                    .notices
                    .lock()
                    .map(|mut n| std::mem::take(&mut *n))
                    .unwrap_or_default();
                if channels.is_empty() {
                    continue;
                }
                let Ok((_, d)) = device(&engine).await else { continue };
                let m = wire::info_message(
                    0xFFFF,
                    shared.next_info_sequence(),
                    d.wire_device_id.0,
                    &settings.vendor,
                    wire::info_opcode(0x0102),
                    &wire::channel_change(&channels),
                );
                let _ = socket.send_to(&m, INFO_GROUP).await;
            }
        }
    }
}

/// RX channel ids (1-based) whose ARC 0x3000 status differs between two
/// beats, a channel present in only one of them included.
fn changed_channels(before: &[u32], now: &[u32]) -> Vec<u16> {
    (0..before.len().max(now.len()))
        .filter(|i| before.get(*i) != now.get(*i))
        .filter_map(|i| u16::try_from(i + 1).ok())
        .collect()
}

/// Delay of the first heartbeat after the role binds its sockets.
const HEARTBEAT_START: Duration = Duration::from_secs(1);
/// Clock priorities for preferred leader on and off: the PTP engine marks a
/// node preferred below 64 (lower wins); 128 is the configuration default.
const PREFERRED_PRIORITY: u8 = 10;
/// How often the info server looks for a sample-rate or encoding change to
/// announce.
const FORMAT_CHECK: Duration = Duration::from_secs(1);
const DEFAULT_PRIORITY: u8 = 128;

/// Requests whose records are multicast at role start: board info, product
/// info (`state-machines.md` §1), sample rate and encoding
/// (`device-settings.md` §3.3 item 7).
const START_ANNOUNCEMENTS: [u16; 4] = [0x0061, 0x00C1, 0x0081, 0x0083];

/// Apply a sample-rate (0x0081) or encoding (0x0083) write as one typed
/// command, and log it at info level whatever the outcome: requester, the
/// whole flags word, value, outcome and the reason of a refusal
/// (`device-settings.md` §3.3 items 4-6). The state refuses every value its
/// format options do not list (a lock, an unsupported value, a bridge pin).
async fn apply_format(
    engine: &EngineHandle,
    request: u16,
    write: wire::ValueWrite,
    peer: SocketAddr,
) {
    let what = if request == 0x0081 {
        "sample-rate"
    } else {
        "encoding"
    };
    let outcome = format_outcome(engine, request, write.value, peer).await;
    let other_bits = if write.flags & !wire::WRITE_FLAG != 0 {
        " (bits besides the write bit set, ignored)"
    } else {
        ""
    };
    let value = write
        .value
        .map_or_else(|| "missing".to_owned(), |v| v.to_string());
    tracing::info!(
        "{what} write from {peer}: flags {:#010x}{other_bits}, value {value}: {outcome}",
        write.flags
    );
}

/// The outcome of one format write, for its log line.
async fn format_outcome(
    engine: &EngineHandle,
    request: u16,
    value: Option<u32>,
    peer: SocketAddr,
) -> String {
    let Some(value) = value else {
        return "refused: the request ends before the value".into();
    };
    let d = match device(engine).await {
        Ok((_, d)) => d,
        Err(e) => return format!("refused: {e}"),
    };
    let (command, before) = if request == 0x0081 {
        let command = tessera_control_types::Command::SetSampleRate {
            device: d.key.clone(),
            rate: tessera_control_types::SampleRate(value),
        };
        (command, d.sample_rate)
    } else {
        let Ok(bits) = u8::try_from(value) else {
            return format!("refused: {value} bits is not supported");
        };
        let command = tessera_control_types::Command::SetEncoding {
            device: d.key.clone(),
            encoding: tessera_control_types::Encoding(bits),
        };
        (command, u32::from(d.encoding))
    };
    let origin = tessera_control_types::Origin::Protocol {
        peer: peer.to_string(),
    };
    match engine.apply(origin, command).await {
        Ok(a) if a.changed => format!("applied (was {before})"),
        Ok(_) => "unchanged (already set)".into(),
        Err(e) => format!("refused: {e}"),
    }
}

/// Receive flow slots for sections 0x8003 and 0x8004, as many as ARC 0x1000
/// advertises (`conmon.md` §9.4, `metering.md` §2.6 items 3-5): slot id - 1
/// of each ARC 0x3200 flow holds the largest lateness of its packets since
/// the previous heartbeat in samples at `rate` (read and reset here; 0
/// without packets) and its late packets since it was created (never reset
/// by a read), each the largest over a group flow's listeners. Empty slots
/// are zero.
fn latency_slots(
    flows: &[wire::RxFlowView],
    media: &crate::media::stats::MediaStats,
    rate: u32,
) -> (Vec<u32>, Vec<u32>) {
    let lateness = media.take_rx_lateness();
    let late = media.rx_late_packets();
    let mut slots = vec![0u32; super::flows::MAX_SESSIONS];
    let mut counts = vec![0u32; super::flows::MAX_SESSIONS];
    for flow in flows {
        let Some(i) = usize::from(flow.id)
            .checked_sub(1)
            .filter(|i| *i < slots.len())
        else {
            continue;
        };
        let largest = |values: &[(String, u64)]| {
            values
                .iter()
                .filter(|(id, _)| flow.flows.contains(id))
                .map(|(_, v)| *v)
                .max()
                .unwrap_or(0)
        };
        let samples = u128::from(largest(&lateness)) * u128::from(rate) / 1_000_000_000;
        slots[i] = u32::try_from(samples.min(i32::MAX as u128)).unwrap_or(0);
        counts[i] = u32::try_from(largest(&late)).unwrap_or(u32::MAX);
    }
    (slots, counts)
}

/// Signal bytes of every TX then RX channel of the device, from the media
/// engine's peaks since the previous heartbeat; a channel no flow carries
/// shows no signal.
fn signal(
    media: &crate::media::stats::MediaStats,
    d: &tessera_state::model::DeviceModel,
) -> (Vec<u8>, Vec<u8>) {
    use tessera_control_types::Direction;
    let peaks = media.take_peaks();
    let level = |dir: Direction, ch: u16| {
        let peak = peaks
            .iter()
            .filter(|(dev, d2, c, _)| *dev == d.key.0 && *d2 == dir && *c == ch)
            .map(|p| p.3)
            .max()
            .unwrap_or(0);
        wire::peak_byte(peak)
    };
    (
        (1..=d.tx_channels)
            .map(|ch| level(Direction::Tx, ch))
            .collect(),
        (1..=d.rx_channels)
            .map(|ch| level(Direction::Rx, ch))
            .collect(),
    )
}

/// Byte rates and error counts of the media interface for heartbeat section
/// 0x8000 and info 0x0040, from the kernel's interface counters, one sample
/// per heartbeat.
#[derive(Debug, Default)]
struct Traffic {
    last: Option<(std::time::Instant, [u64; 4])>,
}

impl Traffic {
    fn sample(&mut self, interface: &str) -> [u32; 4] {
        let read = |name: &str| {
            std::fs::read_to_string(format!("/sys/class/net/{interface}/statistics/{name}"))
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(0)
        };
        let now = [
            read("tx_bytes"),
            read("rx_bytes"),
            read("tx_errors"),
            read("rx_errors"),
        ];
        let at = std::time::Instant::now();
        let rates = match self.last {
            Some((then, before)) => {
                let secs = at.duration_since(then).as_secs_f64().max(0.001);
                let rate = |i: usize| (now[i].saturating_sub(before[i]) as f64 / secs) as u32;
                [
                    rate(0),
                    rate(1),
                    u32::try_from(now[2]).unwrap_or(u32::MAX),
                    u32::try_from(now[3]).unwrap_or(u32::MAX),
                ]
            }
            None => [0, 0, 0, 0],
        };
        self.last = Some((at, now));
        rates
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;

    use super::wire::ValueWrite;
    use super::{
        Host, START_ANNOUNCEMENTS, Settings, Shared, apply_format, changed_channels, identity,
        latency_slots, wire,
    };
    use crate::media::stats::MediaStats;
    use crate::native::testing::TestEngine;
    use tessera_media::RxMetrics;

    const DEVICE: &str = "id='main'\nname='Local'\nrx_channels=2\ntx_channels=2";

    fn host() -> Host {
        Host {
            interface: "audio0".into(),
            address: Ipv4Addr::new(192, 0, 2, 1),
            mac: [2, 0, 0, 0, 0, 1],
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            gateway: Ipv4Addr::UNSPECIFIED,
            dns: Ipv4Addr::UNSPECIFIED,
        }
    }

    fn peer() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 9)), 50_000)
    }

    /// The record the info server answers `request` with now.
    async fn record(t: &TestEngine, request: u16) -> Vec<u8> {
        let me = identity(
            &t.engine,
            &host(),
            &Settings::new("tessera"),
            &Shared::default(),
        )
        .await
        .unwrap();
        wire::info_reply(request, &me).unwrap().1
    }

    async fn write(t: &TestEngine, request: u16, flags: u32, value: Option<u32>) {
        apply_format(&t.engine, request, ValueWrite { flags, value }, peer()).await;
    }

    fn flow(id: u16, listeners: &[&str]) -> wire::RxFlowView {
        wire::RxFlowView {
            id,
            flows: listeners.iter().map(|l| (*l).to_owned()).collect(),
            local: Ipv4Addr::new(10, 0, 0, 3),
            port: 14_336,
            positions: vec![vec![1]],
        }
    }

    #[test]
    fn latency_slots_follow_the_flow_ids_and_only_the_maximum_resets() {
        // `metering.md` §2.6 items 3-5.
        let media = MediaStats::default();
        let metrics: Vec<Arc<RxMetrics>> = (0..4).map(|_| Arc::default()).collect();
        for (n, id) in ["native-rx-4", "native-rx-7", "native-rx-8", "native-rx-9"]
            .iter()
            .enumerate()
        {
            let m = Arc::clone(&metrics[n]);
            media.add_rx(id, "main", &[n as u32], m, Arc::default());
        }
        // 1 ms, 1.15 ms and 0.9 ms at 48 kHz: 48, 55.2 and 43.2 samples.
        metrics[0].max_lateness_ns.record_max(1_000_000);
        metrics[1].max_lateness_ns.record_max(1_150_000);
        metrics[2].max_lateness_ns.record_max(900_000);
        metrics[3].max_lateness_ns.record_max(2_000_000);
        metrics[1].late_packets.add(3);
        metrics[2].late_packets.add(5);
        metrics[3].late_packets.add(8);
        // Slot = id - 1; a group flow (two listeners) reports the larger of
        // each; a flow beyond the advertised slots is left out.
        let flows = [
            flow(1, &["native-rx-4"]),
            flow(3, &["native-rx-7", "native-rx-8"]),
            flow(33, &["native-rx-9"]),
        ];
        let (slots, late) = latency_slots(&flows, &media, 48_000);
        assert_eq!((slots.len(), late.len()), (32, 32));
        assert_eq!(slots[..4], [48, 0, 55, 0]);
        assert_eq!(late[..4], [0, 0, 5, 0]);
        assert!(slots[4..].iter().chain(&late[4..]).all(|v| *v == 0));
        // The maximum is read and reset per heartbeat: no packets, 0. The
        // late count is cumulative.
        let (slots, late) = latency_slots(&flows, &media, 48_000);
        assert!(slots.iter().all(|s| *s == 0));
        assert_eq!(late[..4], [0, 0, 5, 0]);
        // At 96 kHz the same lateness is twice the samples.
        metrics[0].max_lateness_ns.record_max(1_000_000);
        assert_eq!(latency_slots(&flows, &media, 96_000).0[0], 96);
        // No flow: every slot zero, still 32 of them.
        let (slots, late) = latency_slots(&[], &media, 48_000);
        assert_eq!((slots, late), (vec![0; 32], vec![0; 32]));
    }

    #[test]
    fn a_changed_receiving_status_flags_its_channel() {
        let receiving = super::wire::status::RECEIVING;
        let waiting = super::wire::status::WAITING;
        assert!(changed_channels(&[0, receiving], &[0, receiving]).is_empty());
        assert_eq!(changed_channels(&[0, waiting], &[0, receiving]), [2]);
        // A channel added or removed counts as changed.
        assert_eq!(changed_channels(&[0], &[0, 0, 0]), [2, 3]);
        assert_eq!(changed_channels(&[0, 0], &[]), [1, 2]);
    }

    #[tokio::test]
    async fn the_role_start_announces_the_rate_and_encoding_records() {
        // `device-settings.md` §3.3 item 7: with board and product info.
        let t = TestEngine::start(DEVICE);
        let me = identity(
            &t.engine,
            &host(),
            &Settings::new("tessera"),
            &Shared::default(),
        )
        .await
        .unwrap();
        let ids: Vec<u16> = START_ANNOUNCEMENTS
            .iter()
            .map(|r| wire::info_reply(*r, &me).unwrap().0)
            .collect();
        assert_eq!(ids, [0x0060, 0x00C0, 0x0080, 0x0082]);
    }

    #[tokio::test]
    async fn a_format_write_is_one_command_and_the_record_shows_its_outcome() {
        let t = TestEngine::start(DEVICE);
        // Every rate the state accepts, update mode 2 (§3.3 items 2-3).
        let mut expected = vec![0x00, 0x18, 0x00, 0x06, 0x00, 0x00, 0xBB, 0x80];
        expected.extend_from_slice(&[0, 0, 0, 0, 0, 2, 0, 0]);
        for rate in [44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000] {
            expected.extend_from_slice(&rate.to_be_bytes());
        }
        assert_eq!(record(&t, 0x0081).await, expected);
        // A further flag bit leaves it a write (§3.3 item 4).
        write(&t, 0x0081, 0x0000_0003, Some(44_100)).await;
        let after = record(&t, 0x0081).await;
        assert_eq!(&after[4..8], &44_100u32.to_be_bytes());
        assert_eq!(&after[12..14], &[0, 2]);
        // Refused (not supported, missing value): the record is unchanged.
        write(&t, 0x0081, 1, Some(12_345)).await;
        write(&t, 0x0081, 1, None).await;
        assert_eq!(record(&t, 0x0081).await, after);
        write(&t, 0x0083, 1, Some(16)).await;
        let encoding = record(&t, 0x0083).await;
        assert_eq!(&encoding[4..8], &16u32.to_be_bytes());
        write(&t, 0x0083, 1, Some(0x118)).await;
        assert_eq!(record(&t, 0x0083).await, encoding);
    }

    #[tokio::test]
    async fn a_locked_format_is_listed_alone_with_update_mode_0() {
        let t = TestEngine::start(&format!("{DEVICE}\nlocked=['sample_rate','encoding']"));
        let rate = [
            0x00, 0x18, 0x00, 0x01, 0x00, 0x00, 0xBB, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0xBB, 0x80,
        ];
        let encoding = [
            0x00, 0x18, 0x00, 0x01, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x18,
        ];
        assert_eq!(record(&t, 0x0081).await, rate);
        assert_eq!(record(&t, 0x0083).await, encoding);
        write(&t, 0x0081, 1, Some(44_100)).await;
        write(&t, 0x0083, 1, Some(16)).await;
        assert_eq!(record(&t, 0x0081).await, rate);
        assert_eq!(record(&t, 0x0083).await, encoding);
    }
}
