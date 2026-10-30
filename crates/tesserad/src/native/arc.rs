//! The ARC listener (`control-arc.md`): legacy query replies from the live
//! state, and subscription and label writes as typed engine commands.
//! A write is applied before the reply, because controllers read the RX list
//! again right after it (§5.1, §9.4).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tessera_control_types::{Channel, Command, Direction, LatencyUs, Origin};
use tessera_state::config::DeviceLock;
use tokio::net::UdpSocket;

use super::Shared;
use super::wire::{self, Header};
use crate::engine_thread::EngineHandle;

/// Receive flows advertised in 0x1000 (`control-arc.md` §4.1).
const MAX_RX_FLOWS: u16 = super::flows::MAX_SESSIONS as u16;

pub(super) async fn serve(
    engine: EngineHandle,
    shared: Arc<Shared>,
    port: Arc<super::flows::FlowPort>,
    socket: UdpSocket,
    lab: bool,
) {
    let mut buf = vec![0u8; 2048];
    let mut unsupported = Unsupported::default();
    loop {
        let Ok((n, peer)) = socket.recv_from(&mut buf).await else {
            continue;
        };
        let Some((header, content)) = wire::request(&buf[..n]) else {
            continue;
        };
        let content = content.to_vec();
        let logged = lab
            && !matches!(
                header.opcode,
                0x1000
                    | 0x1002
                    | 0x1003
                    | 0x1100
                    | 0x1102
                    | 0x2000
                    | 0x2010
                    | 0x2200
                    | 0x3000
                    | 0x3200
                    | 0x3300
            );
        if logged {
            let hex: String = buf[..n].iter().map(|b| format!("{b:02x}")).collect();
            tracing::info!("native lab: ARC {:#06x} from {peer}: {hex}", header.opcode);
        }
        if let Some(out) = answer(&engine, &shared, &port, header, &content, peer).await {
            if !logged && wire::be16(&out, 8) == Some(wire::UNIMPLEMENTED) {
                unsupported.log(header.opcode, peer, &buf[..n]);
            }
            let _ = socket.send_to(&out, peer).await;
        }
    }
}

/// Requests answered "unsupported" (0x0030), logged at info level with
/// their bytes so that a write form tessera does not know leaves a trace
/// (`device-settings.md` §4.4 item 4): every refused latency write (0x1101),
/// any other opcode once a minute at most, with the number left out since
/// the previous line.
#[derive(Debug, Default)]
struct Unsupported {
    last: HashMap<u16, (Instant, u32)>,
}

impl Unsupported {
    const EVERY: Duration = Duration::from_secs(60);

    /// Log one request; whether a line was written.
    fn log(&mut self, opcode: u16, peer: SocketAddr, bytes: &[u8]) -> bool {
        let now = Instant::now();
        let skipped = match self.last.get_mut(&opcode) {
            Some((at, skipped)) if opcode != 0x1101 && now.duration_since(*at) < Self::EVERY => {
                *skipped += 1;
                return false;
            }
            Some((_, skipped)) => std::mem::take(skipped),
            None => 0,
        };
        self.last.insert(opcode, (now, 0));
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let earlier = if skipped > 0 {
            format!(" ({skipped} more since the previous line)")
        } else {
            String::new()
        };
        tracing::info!("native ARC {opcode:#06x} from {peer} answered unsupported{earlier}: {hex}");
        true
    }
}

async fn answer(
    engine: &EngineHandle,
    shared: &Shared,
    port: &super::flows::FlowPort,
    h: Header,
    content: &[u8],
    peer: SocketAddr,
) -> Option<Vec<u8>> {
    let (view, model) = match super::device(engine).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("native ARC: {e}");
            return None;
        }
    };
    let ok = |c: &[u8]| Some(wire::reply(h, 1, c));
    let paged =
        |(c, more): (Vec<u8>, bool)| Some(wire::reply(h, if more { wire::MORE } else { 1 }, &c));
    match h.opcode {
        0x1000 => ok(&wire::counts(
            &view,
            MAX_RX_FLOWS,
            !model.locked.contains(&DeviceLock::TxChannelLabels),
        )),
        0x1002 => ok(&wire::name(&view)),
        0x1003 => ok(&wire::names_block(
            &view,
            "tessera",
            tessera_control_types::BUILD_VERSION,
        )),
        0x1100 => ok(&wire::property_values(
            content,
            &wire::Properties {
                default_latency_ns: super::default_latency_ns(engine, &model).await,
                latency_ns: model.latency_us.saturating_mul(1000),
            },
        )),
        0x1102 => ok(&wire::property_directory()),
        0x2000 | 0x2010 => match wire::page_start(content) {
            Some(first) => paged(wire::tx_list(&view, first, h.opcode == 0x2010)),
            None => Some(wire::reply(h, wire::FAILURE, &[])),
        },
        // The flow lists answer a request without a page index with the first
        // page, as before they were paged: no controller's request form for
        // them is recorded (§§4.9, 4.13), and refusing would hide the flows.
        0x2200 => {
            let flows = shared
                .tx_flows
                .lock()
                .map(|f| f.clone())
                .unwrap_or_default();
            let first = wire::page_start(content).unwrap_or(1);
            paged(wire::tx_flows(&view, &flows, first))
        }
        0x3000 => match wire::page_start(content) {
            Some(first) => paged(wire::rx_list(&view, first)),
            None => Some(wire::reply(h, wire::FAILURE, &[])),
        },
        0x3200 => {
            let flows = shared.flows.lock().map(|f| f.clone()).unwrap_or_default();
            let first = wire::page_start(content).unwrap_or(1);
            paged(wire::rx_flows(&view, &flows, first))
        }
        0x3300 => ok(&wire::RECEIVE_PORTS),
        // Meaning unknown; the vendor controller asks 0x2204 in its query
        // rounds. Success with an empty list, as the runtime answered while
        // the controller's latency graph worked (`runtime-parity.md` 10g).
        0x2204 | 0x3320 => ok(&[0, 0]),
        // Multicast flows are the device's configuration: refused while its
        // access lock is set (`device-lock.md` §9.2 L7). They install as the
        // daemon's own flow set, so the role asks the lock first.
        0x2201 | 0x2202 if super::access::locked(engine, &model.key).await == Some(true) => {
            Some(wire::reply(h, super::access::ARC_LOCKED, &[]))
        }
        // Manual multicast flows (`control-arc.md` §§4.10-4.11).
        0x2201 => {
            let created = port
                .create_multicast(wire::multicast_creates(content))
                .await;
            if created.is_empty() {
                Some(wire::reply(h, wire::FAILURE, &[]))
            } else {
                ok(&wire::multicast_created(&created))
            }
        }
        0x2202 => {
            if port
                .delete_multicast(&wire::multicast_deletes(content))
                .await
            {
                ok(&[])
            } else {
                Some(wire::reply(h, wire::FAILURE, &[]))
            }
        }
        // Device latency (`device-settings.md` §12.1): 0x8205 is the setting;
        // 0x8301 (the active value) may accompany it only with the same value.
        0x1101 => {
            let items = wire::property_writes(content);
            let find = |id: u16| items.iter().find(|(i, _)| *i == id).map(|(_, v)| *v);
            let Some(ns) = find(0x8205) else {
                return Some(wire::reply(h, wire::UNIMPLEMENTED, &[]));
            };
            if find(0x8301).is_some_and(|active| active != ns) {
                return Some(wire::reply(h, wire::UNIMPLEMENTED, &[]));
            }
            let command = Command::SetLatency {
                device: model.key.clone(),
                latency: LatencyUs(ns / 1000),
            };
            let origin = Origin::Protocol {
                peer: peer.to_string(),
            };
            if let Err(e) = engine.apply(origin, command).await {
                tracing::info!("native ARC latency {ns} ns from {peer}: {e}");
                return super::access::arc_refusal(h, &e)
                    .or_else(|| Some(wire::reply(h, wire::UNIMPLEMENTED, &[])));
            }
            let (_, after) = super::device(engine).await.ok()?;
            let ids: Vec<u16> = items.iter().map(|(id, _)| *id).collect();
            ok(&wire::property_list(
                &ids,
                &wire::Properties {
                    default_latency_ns: super::default_latency_ns(engine, &after).await,
                    latency_ns: after.latency_us.saturating_mul(1000),
                },
            ))
        }
        // Device rename (`device-settings.md` §12.7).
        0x1001 => {
            let name = wire::rename(content);
            let Some(name) = name else {
                return Some(wire::reply(h, wire::UNIMPLEMENTED, &[]));
            };
            let command = Command::SetDeviceName {
                device: model.key.clone(),
                name: name.to_owned(),
            };
            let origin = Origin::Protocol {
                peer: peer.to_string(),
            };
            match engine.apply(origin, command).await {
                Ok(_) => ok(&[]),
                Err(e) => {
                    tracing::info!("native ARC rename to {name:?} from {peer}: {e}");
                    super::access::arc_refusal(h, &e)
                        .or_else(|| Some(wire::reply(h, wire::UNIMPLEMENTED, &[])))
                }
            }
        }
        0x3010 => {
            let items = wire::subscriptions(content);
            let origin = Origin::Protocol {
                peer: peer.to_string(),
            };
            let mut touched = Vec::new();
            let mut access_locked = None;
            for (rx, source) in items {
                let Some(channel) = Channel::new(rx) else {
                    continue;
                };
                let command = match source {
                    Some((tx_channel, tx_device)) => Command::Subscribe {
                        device: model.key.clone(),
                        rx_channel: channel,
                        tx_channel,
                        tx_device,
                    },
                    None => Command::Unsubscribe {
                        device: model.key.clone(),
                        rx_channel: channel,
                    },
                };
                match engine.apply(origin.clone(), command).await {
                    Ok(_) => touched.push(rx),
                    Err(e) => {
                        tracing::info!("native ARC subscription on RX {rx} from {peer}: {e}");
                        access_locked = access_locked.or_else(|| super::access::arc_refusal(h, &e));
                    }
                }
            }
            if !touched.is_empty() {
                shared.changed.notify_one();
                shared.notify_channels(touched);
            }
            // Items are applied independently; the reply does not report
            // per-item success (§5.1), except for the access lock, which
            // refuses them all (`device-lock.md` §9.2 L7).
            access_locked.or_else(|| ok(&[]))
        }
        0x3014 => {
            if let Some((rx, channel)) =
                wire::removal(content).and_then(|rx| Some((rx, Channel::new(rx)?)))
            {
                let command = Command::Unsubscribe {
                    device: model.key.clone(),
                    rx_channel: channel,
                };
                let origin = Origin::Protocol {
                    peer: peer.to_string(),
                };
                match engine.apply(origin, command).await {
                    Ok(_) => {
                        shared.changed.notify_one();
                        shared.notify_channels([rx]);
                    }
                    Err(e) => {
                        if let Some(refusal) = super::access::arc_refusal(h, &e) {
                            return Some(refusal);
                        }
                    }
                }
            }
            ok(&[])
        }
        0x3001 => {
            let origin = Origin::Protocol {
                peer: peer.to_string(),
            };
            let mut renamed = Vec::new();
            let mut access_locked = None;
            for (rx, label) in wire::renames(content) {
                let Some(channel) = Channel::new(rx) else {
                    continue;
                };
                let command = Command::SetChannelLabel {
                    device: model.key.clone(),
                    direction: Direction::Rx,
                    channel,
                    label: Some(label),
                };
                match engine.apply(origin.clone(), command).await {
                    Ok(_) => renamed.push(rx),
                    Err(e) => {
                        access_locked = access_locked.or_else(|| super::access::arc_refusal(h, &e))
                    }
                }
            }
            if renamed.is_empty() {
                access_locked.or_else(|| Some(wire::reply(h, wire::FAILURE, &[])))
            } else {
                shared.notify_channels(renamed);
                ok(&[])
            }
        }
        // TX channel rename (§4.7): each item is one label command; the
        // `_netaudio-chan` records follow the model (old friendly-name record
        // withdrawn, new one announced). No channel-change multicast: that
        // mask names RX channels (§4.7, `conmon.md` §5).
        0x2013 => {
            let origin = Origin::Protocol {
                peer: peer.to_string(),
            };
            let mut applied = false;
            let mut access_locked = None;
            for (tx, label) in wire::tx_renames(content) {
                let Some(channel) = Channel::new(tx) else {
                    continue;
                };
                let command = Command::SetChannelLabel {
                    device: model.key.clone(),
                    direction: Direction::Tx,
                    channel,
                    label: Some(label),
                };
                match engine.apply(origin.clone(), command).await {
                    Ok(_) => applied = true,
                    Err(e) => {
                        tracing::info!("native ARC rename of TX {tx} from {peer}: {e}");
                        access_locked = access_locked.or_else(|| super::access::arc_refusal(h, &e));
                    }
                }
            }
            if applied {
                ok(&[0, 0])
            } else {
                access_locked.or_else(|| Some(wire::reply(h, wire::FAILURE, &[])))
            }
        }
        _ => Some(wire::reply(h, wire::UNIMPLEMENTED, &[])),
    }
}
