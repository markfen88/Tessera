//! The device access lock on the device role (`device-lock.md` §9.3): the
//! lock-state record 0x1009 that answers 0x1008 (L10) and is multicast on
//! every change (L11), the lab-only lockable bit and log-only lock-service
//! listener (L12), and the protocol answers to refused writes (L7-L9).
//!
//! The state engine decides (L5); this module only reads its decision and
//! maps refusals to each protocol's answer. Setting or clearing the lock from
//! the network (UDP 8002, §9.4 L15) is BLOCKED: nothing here answers 8002.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;

use tessera_control_types::{CommandError, DeviceKey, Event, Query, Reply};
use tessera_state::{EventBus, Received};
use tokio::net::UdpSocket;

use super::wire::{self, Header};
use super::{Settings, Shared};
use crate::engine_thread::EngineHandle;

/// Info request for the lock state, and its answer (`device-lock.md` §2).
pub(super) const REQUEST: u16 = 0x1008;
pub(super) const RESPONSE: u16 = 0x1009;
/// Board-info capability bit "supports device lock" (§3). Clear by default
/// while the lock service is blocked (L12): a lab capability word may set it.
pub const LOCKABLE: u32 = 0x0800_0000;
/// ARC result of a write the lock refuses: "configuration locked" (§6a,
/// number `guess`, L7).
pub(super) const ARC_LOCKED: u16 = 0x00F3;
/// Flow-control result of a new flow the lock refuses: "access control
/// failure" (§6a, number `guess`, L9).
pub(super) const FLOW_LOCKED: u16 = 0x0341;
/// The vendor's lock service port (§2). Bound only by the lab listener.
pub const LOCK_PORT: u16 = 8002;
/// Bytes of a lock-service datagram the lab listener logs.
const LAB_LOG_BYTES: usize = 64;

/// Content of 0x1009 (L10): 16 bytes, content 0x00 = 1 when locked (position
/// `likely`, value `guess`); reset status 0, no identifiers, identifier size
/// 8, list offset 0, policy words 0. The unlocked form is the one a
/// third-party decoder accepts (§4.1).
#[must_use]
pub(super) fn lock_status(locked: bool) -> Vec<u8> {
    let mut c = vec![0u8; 16];
    c[1] = u8::from(locked);
    c[7] = 8;
    c
}

/// The whole 0x1009 datagram (32-byte info header, then the content).
#[must_use]
pub(super) fn lock_message(
    sequence: u16,
    device_id: [u8; 8],
    vendor: &[u8; 8],
    locked: bool,
) -> Vec<u8> {
    wire::info_message(
        0xFFFF,
        sequence,
        device_id,
        vendor,
        wire::info_opcode(RESPONSE),
        &lock_status(locked),
    )
}

/// Whether `device`'s access lock is set; `None` when the engine does not
/// answer (the caller then answers nothing rather than guess).
pub(super) async fn locked(engine: &EngineHandle, device: &DeviceKey) -> Option<bool> {
    match engine
        .query(Query::Device {
            device: device.clone(),
        })
        .await
    {
        Ok(Reply::Device(d)) => Some(d.access_locked),
        _ => None,
    }
}

/// The first device's key, wire id and lock state.
async fn state(engine: &EngineHandle) -> Option<(DeviceKey, [u8; 8], bool)> {
    let rp = engine.runtime_plan().await.ok()?;
    let d = rp.model.devices.first()?;
    let locked = locked(engine, &d.key).await?;
    Some((d.key.clone(), d.wire_device_id.0, locked))
}

/// Is the first device locked now? Unknown counts as locked: the callers
/// refuse an action that cannot be undone (a reboot) when in doubt.
pub(super) async fn locked_now(engine: &EngineHandle) -> bool {
    state(engine).await.is_none_or(|(_, _, locked)| locked)
}

/// The ARC answer to a write the engine refused, when the access lock
/// refused it (L7): result 0x00F3, no content.
#[must_use]
pub(super) fn arc_refusal(h: Header, error: &CommandError) -> Option<Vec<u8>> {
    matches!(error, CommandError::AccessLocked { .. }).then(|| wire::reply(h, ARC_LOCKED, &[]))
}

/// Answer 0x1008 (L10) as the other info requests are answered: to the info
/// group (`group`), and the same datagram back to the requester.
pub(super) async fn answer(
    engine: &EngineHandle,
    socket: &UdpSocket,
    settings: &Settings,
    shared: &Shared,
    peer: SocketAddr,
    group: SocketAddrV4,
) {
    let Some((_, device_id, locked)) = state(engine).await else {
        return;
    };
    let m = lock_message(
        shared.next_info_sequence(),
        device_id,
        &settings.vendor,
        locked,
    );
    let _ = socket.send_to(&m, group).await;
    if let SocketAddr::V4(peer) = peer {
        let _ = socket.send_to(&m, peer).await;
    }
}

/// Follow the lock (L9, L11): on every change of the first device's lock,
/// send one 0x1009 with the new content to the info group (`group`; `eng`
/// L1, `guess`) and tell the flow port, which keeps serving only the flows
/// of the moment of locking. Subscribes at once, so no change after this
/// call is missed. The future never returns.
pub(super) fn watch(
    engine: EngineHandle,
    bus: &EventBus,
    socket: Arc<UdpSocket>,
    shared: Arc<Shared>,
    settings: Settings,
    flows: Arc<super::flows::FlowPort>,
    group: SocketAddrV4,
) -> impl std::future::Future<Output = ()> + Send + 'static {
    let mut events = bus.subscribe(32);
    async move {
        let mut last = state(&engine).await.map(|(_, _, locked)| locked);
        loop {
            let changed = match events.recv_async().await {
                Ok(Received::Event(Event::AccessLockChanged { device, locked, .. })) => {
                    let Ok(rp) = engine.runtime_plan().await else {
                        continue;
                    };
                    let Some(d) = rp.model.devices.first() else {
                        continue;
                    };
                    if d.key != device {
                        continue;
                    }
                    (d.wire_device_id.0, locked)
                }
                // Missed events: compare with what was announced last.
                Ok(Received::Lagged(_)) => match state(&engine).await {
                    Some((_, id, locked)) if last != Some(locked) => (id, locked),
                    _ => continue,
                },
                Ok(Received::Event(_)) => continue,
                Err(_) => return std::future::pending().await,
            };
            let (device_id, locked) = changed;
            flows.lock_changed(locked).await;
            let m = lock_message(
                shared.next_info_sequence(),
                device_id,
                &settings.vendor,
                locked,
            );
            let _ = socket.send_to(&m, group).await;
            tracing::info!(
                "device access lock {}: announced to controllers",
                if locked { "set" } else { "removed" }
            );
            last = Some(locked);
        }
    }
}

/// Lab capture only (L12): log what arrives on the lock service port
/// (source, length, first 64 bytes) and never answer. Outside lab mode the
/// port is never bound. A port that cannot be bound is logged once.
pub(super) async fn lab_listener(address: Ipv4Addr) {
    let socket = match UdpSocket::bind(SocketAddrV4::new(address, LOCK_PORT)).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("native lab: cannot listen on UDP {LOCK_PORT}: {e}");
            return std::future::pending().await;
        }
    };
    tracing::info!("native lab: logging UDP {LOCK_PORT} datagrams on {address}; never answered");
    let mut buf = vec![0u8; 2048];
    loop {
        let Ok((n, peer)) = socket.recv_from(&mut buf).await else {
            continue;
        };
        let head: String = buf[..n.min(LAB_LOG_BYTES)]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        tracing::info!("native lab: UDP {LOCK_PORT} from {peer}, {n} bytes: {head}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::testing::TestEngine;
    use std::time::Duration;

    const ID: [u8; 8] = [0x00, 0x1D, 0xC1, 0xFF, 0xFE, 0x12, 0x34, 0x56];
    const DEVICE: &str = "id='main'\nname='Local'\nrx_channels=2\ntx_channels=2";

    async fn next(socket: &UdpSocket) -> Vec<u8> {
        let mut buf = vec![0u8; 2048];
        let (n, _) = tokio::time::timeout(Duration::from_secs(10), socket.recv_from(&mut buf))
            .await
            .expect("a 0x1009 in time")
            .unwrap();
        buf.truncate(n);
        buf
    }

    async fn local() -> (UdpSocket, SocketAddrV4) {
        let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let SocketAddr::V4(at) = s.local_addr().unwrap() else {
            unreachable!()
        };
        (s, at)
    }

    /// 0x1008 is answered from the access lock, to the group and the asker.
    #[tokio::test]
    async fn a_lock_state_request_is_answered_in_both_forms() {
        let t = TestEngine::start(DEVICE);
        let (group, group_at) = local().await;
        let (asker, asker_at) = local().await;
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let settings = Settings::new("tessera");
        let shared = Shared::default();
        for (step, locked) in [false, true, false].into_iter().enumerate() {
            if step > 0 {
                t.set_access_lock(locked).await;
            }
            answer(
                &t.engine,
                &socket,
                &settings,
                &shared,
                SocketAddr::V4(asker_at),
                group_at,
            )
            .await;
            let to_group = next(&group).await;
            assert_eq!(to_group, next(&asker).await, "the same datagram twice");
            assert_eq!(wire::info_request(&to_group), Some(RESPONSE));
            assert_eq!(&to_group[32..], lock_status(locked).as_slice());
        }
    }

    /// Every change is announced once, with the new content (L11).
    #[tokio::test]
    async fn every_lock_change_is_announced_unasked() {
        let t = TestEngine::start(DEVICE);
        let (group, group_at) = local().await;
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let shared = Arc::new(Shared::default());
        let flows = Arc::new(super::super::flows::FlowPort::new(
            UdpSocket::bind("127.0.0.1:0").await.unwrap(),
            t.engine.clone(),
            Arc::clone(&shared),
            Arc::new(crate::media::stats::MediaStats::default()),
        ));
        let task = tokio::spawn(watch(
            t.engine.clone(),
            &t.bus,
            socket,
            shared,
            Settings::new("tessera"),
            flows,
            group_at,
        ));
        t.set_access_lock(true).await;
        assert_eq!(&next(&group).await[32..], lock_status(true).as_slice());
        t.set_access_lock(false).await;
        assert_eq!(&next(&group).await[32..], lock_status(false).as_slice());
        // A reset is a change too.
        t.set_access_lock(true).await;
        assert_eq!(&next(&group).await[32..], lock_status(true).as_slice());
        let rp = t.engine.runtime_plan().await.unwrap();
        t.engine
            .apply(
                tessera_control_types::Origin::Cli { uid: 0 },
                tessera_control_types::Command::ResetAccessLock {
                    device: rp.model.devices[0].key.clone(),
                },
            )
            .await
            .unwrap();
        let reset = next(&group).await;
        assert_eq!(&reset[32..], lock_status(false).as_slice());
        assert_eq!(&reset[8..16], &rp.model.devices[0].wire_device_id.0);
        task.abort();
    }

    #[tokio::test]
    async fn a_reboot_is_refused_only_while_locked() {
        let t = TestEngine::start(DEVICE);
        assert!(!locked_now(&t.engine).await);
        t.set_access_lock(true).await;
        assert!(locked_now(&t.engine).await);
    }

    /// The two 0x1009 datagrams byte for byte: header with start code
    /// 0xFFFF, length 48, the sequence, process id 0, device id, vendor and
    /// opcode `07 2A 10 09 00 00 00 00`, then 16 bytes of content.
    #[test]
    fn lock_state_datagrams_have_the_specified_bytes() {
        let header = |sequence: u8| {
            let mut h = vec![0xFF, 0xFF, 0x00, 0x30, 0x00, sequence, 0x00, 0x00];
            h.extend_from_slice(&ID);
            h.extend_from_slice(b"tessera ");
            h.extend_from_slice(&[0x07, 0x2A, 0x10, 0x09, 0, 0, 0, 0]);
            h
        };
        let mut unlocked = header(7);
        unlocked.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(lock_message(7, ID, b"tessera ", false), unlocked);
        let mut locked = header(8);
        locked.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(lock_message(8, ID, b"tessera ", true), locked);
        assert_eq!(locked.len(), 48);
    }

    #[test]
    fn the_lockable_bit_stays_clear_by_default() {
        assert_eq!(super::super::DEFAULT_CAPABILITIES & LOCKABLE, 0);
        assert_eq!(LOCKABLE.to_be_bytes()[0], 0x08, "byte 0x14 bit 3");
    }

    #[test]
    fn only_an_access_lock_refusal_becomes_0x00f3() {
        let h = Header {
            start: 0x27FF,
            sequence: 3,
            opcode: 0x1001,
        };
        let locked = CommandError::AccessLocked {
            device: DeviceKey("main".into()),
        };
        let reply = arc_refusal(h, &locked).unwrap();
        assert_eq!(reply, [0x27, 0xFF, 0, 10, 0, 3, 0x10, 0x01, 0x00, 0xF3]);
        assert!(arc_refusal(h, &CommandError::locked("name")).is_none());
        assert!(arc_refusal(h, &CommandError::RateLimited).is_none());
    }
}
