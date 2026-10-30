//! The flow-control port (`control-flows.md`): one socket for this device's
//! requests as a receiver and for requests it answers as a transmitter.
//!
//! Transmitter sessions are kept per receiver destination (address and
//! port). Several sessions to one receiver may carry the same channels, so
//! each of a receiver's inputs can have a flow of its own: a change to one
//! never touches another. A request to an existing destination replaces its
//! channel list (§1). A request whose flow name carries a newer requester
//! incarnation (`<8 hex>-<n>`, the form this daemon's receiver uses) retires
//! that requester's older sessions, which a restarted receiver cannot stop.
//! Sessions become daemon-owned transmit flows in the one protocol flow set.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tessera_control_types::{DeviceKey, Direction, FlowSpec};
use tokio::net::UdpSocket;
use tokio::sync::oneshot;

use super::Shared;
use super::wire::{self, FlowReply};
use crate::engine_thread::EngineHandle;

/// Wait for a flow-control reply; the requester does not resend (§7.3 shows
/// receivers repeating only on their own retry cadence).
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);
/// Concurrent transmit sessions, as advertised in ARC 0x1000.
pub(super) const MAX_SESSIONS: usize = 32;
/// Largest frames per packet a transmitter accepts (§1).
const MAX_FPP: u16 = 256;
/// A unicast session whose receiver sent nothing back for this long is
/// retired (§5: the reference waits 4 s, a hardware transmitter about 1.3 s;
/// generous so a short receive-side outage never drops a live receiver).
const SILENT_NS: u64 = 10_000_000_000;
/// A unicast session that has never heard from its receiver is released this
/// long after it was created or last requested (`state-machines.md` §3,
/// Tessera policy), so a receiver that vanished before its first keepalive
/// does not hold one of the [`MAX_SESSIONS`] for ever.
const NEVER_HEARD_NS: u64 = 30_000_000_000;
/// A deleted multicast flow keeps sending this long after its bundle record
/// is withdrawn, so that its receivers move to unicast before the media
/// stops (`state-machines.md` §3, Tessera policy, make before break). On the
/// pair a receiver took about 3.7 s to notice the withdrawal; 8 s leaves room.
const RETIRE_GRACE_NS: u64 = 8_000_000_000;

struct Pending {
    from: Ipv4Addr,
    sequence: u16,
    opcode: u16,
    reply: oneshot::Sender<FlowReply>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Session {
    /// TX flow id, shared with the multicast flows and kept for the
    /// session's life: ARC 0x2200 lists it and the handle carries it.
    id: u16,
    /// Random per session, so that a handle of an earlier session or daemon
    /// incarnation never matches this one (§§2-3: 0x0103).
    instance: u32,
    /// Monotonic time the session was created or last re-requested.
    created_ns: u64,
    address: Ipv4Addr,
    port: u16,
    channels: Vec<u16>,
    fpp: u16,
    receiver: String,
    flow_name: String,
}

impl Session {
    fn handle(&self) -> [u8; wire::FLOW_HANDLE_LEN] {
        wire::flow_handle(self.id, self.instance)
    }
    fn spec_id(&self) -> String {
        let hex: String = self.handle().iter().map(|b| format!("{b:02x}")).collect();
        format!("native-tx-{hex}")
    }
    fn spec(&self, device: &DeviceKey) -> FlowSpec {
        let mut s = FlowSpec::new(self.spec_id(), device.clone(), Direction::Tx, self.port);
        s.channels = self.channels.clone();
        s.peer = Some(self.address.to_string());
        s.frames_per_packet = Some(u32::from(self.fpp));
        s
    }
}

/// A manual multicast transmit flow (`control-arc.md` §§4.10-4.11): the
/// controller chooses the id; the device chooses the group.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Multicast {
    id: u16,
    channels: Vec<u16>,
    group: Ipv4Addr,
}

/// Fixed multicast media port (`control-arc.md` §4.10, `discovery-mdns.md` §3.4).
const MULTICAST_PORT: u16 = 4321;
/// Frames per packet of a multicast flow (`discovery-mdns.md` §3.4).
const MULTICAST_FPP: u16 = 32;

impl Multicast {
    fn spec(&self, device: &DeviceKey) -> FlowSpec {
        let mut s = FlowSpec::new(
            format!("{}{}", super::MULTICAST_PREFIX, self.id),
            device.clone(),
            Direction::Tx,
            MULTICAST_PORT,
        );
        s.channels = self.channels.clone();
        s.peer = Some(self.group.to_string());
        s.frames_per_packet = Some(u32::from(MULTICAST_FPP));
        s
    }

    /// The same flow while it retires: an id outside the `<prefix><id>` form,
    /// so it is neither advertised as a bundle nor restored at start.
    fn retiring_spec(&self, device: &DeviceKey) -> FlowSpec {
        let mut s = self.spec(device);
        s.id = format!("{}retiring-{}", super::MULTICAST_PREFIX, self.id);
        s
    }
}

/// A deleted multicast flow that still sends until `until_ns` (monotonic).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Retiring {
    flow: Multicast,
    until_ns: u64,
}

/// The transmit half of the protocol flow set: unicast sessions requested by
/// receivers and manual multicast flows created by controllers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Tx {
    sessions: Vec<Session>,
    multicast: Vec<Multicast>,
    /// Deleted multicast flows in their grace period ([`RETIRE_GRACE_NS`]):
    /// still sent, no longer advertised or listed.
    retiring: Vec<Retiring>,
}

impl Tx {
    fn specs(&self, device: &DeviceKey) -> Vec<FlowSpec> {
        self.sessions
            .iter()
            .map(|s| s.spec(device))
            .chain(self.multicast.iter().map(|m| m.spec(device)))
            .chain(self.retiring.iter().map(|r| r.flow.retiring_spec(device)))
            .collect()
    }

    /// Delete the multicast flows `ids`: each keeps sending as a retiring flow
    /// until `now + RETIRE_GRACE_NS`. Whether any existed.
    fn retire(&mut self, ids: &[u16], now: u64) -> bool {
        let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.multicast)
            .into_iter()
            .partition(|m| ids.contains(&m.id));
        self.multicast = kept;
        let any = !gone.is_empty();
        self.retiring.extend(gone.into_iter().map(|flow| Retiring {
            flow,
            until_ns: now.saturating_add(RETIRE_GRACE_NS),
        }));
        any
    }

    /// Drop retiring flows whose grace period is over at `now`.
    fn drop_retired(&mut self, now: u64) {
        self.retiring.retain(|r| now < r.until_ns);
    }

    /// The ARC 0x2200 list, in id order. The local flow name is
    /// `<flow id>_<process id>` (`control-arc.md` §4.9), process id 0 as in
    /// the CMC record.
    fn views(&self) -> Vec<wire::TxFlowView> {
        let mut views: Vec<wire::TxFlowView> = self
            .multicast
            .iter()
            .map(|m| wire::TxFlowView {
                id: m.id,
                multicast: true,
                local_name: format!("{}_0", m.id),
                receiver: String::new(),
                receiver_flow: String::new(),
                address: m.group,
                port: MULTICAST_PORT,
                channels: m.channels.clone(),
            })
            .chain(self.sessions.iter().map(|s| wire::TxFlowView {
                id: s.id,
                multicast: false,
                local_name: format!("{}_0", s.id),
                receiver: s.receiver.clone(),
                receiver_flow: s.flow_name.clone(),
                address: s.address,
                port: s.port,
                channels: s.channels.clone(),
            }))
            .collect();
        views.sort_by_key(|v| v.id);
        views
    }

    /// Whether a transmit flow, unicast or multicast, holds `id`.
    fn in_use(&self, id: u16) -> bool {
        self.sessions.iter().any(|s| s.id == id) || self.multicast.iter().any(|m| m.id == id)
    }

    /// The lowest free TX flow id. Unicast and multicast flows share the
    /// [`MAX_SESSIONS`] ARC 0x1000 advertises; `None` when all are in use
    /// (`control-flows.md` §4: 0x0315; `state-machines.md` §3).
    fn free_id(&self) -> Option<u16> {
        (1..=MAX_SESSIONS as u16).find(|id| !self.in_use(*id))
    }
}

/// A receiver destination served when the device access lock was set, and
/// its channels (`device-lock.md` §9.2 L9).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Served {
    address: Ipv4Addr,
    port: u16,
    /// Empty: every channel.
    channels: Vec<u16>,
}

impl Served {
    fn of(sessions: &[Session]) -> Vec<Self> {
        sessions
            .iter()
            .map(|s| Self {
                address: s.address,
                port: s.port,
                channels: s.channels.clone(),
            })
            .collect()
    }
}

/// While locked, may `address:port` be served `channels`? Only a destination
/// served at the moment of locking, with no channel it did not have then
/// (empty positions aside).
fn served_at_lock(basis: &[Served], address: Ipv4Addr, port: u16, channels: &[u16]) -> bool {
    basis.iter().any(|s| {
        s.address == address
            && s.port == port
            && (s.channels.is_empty() || channels.iter().all(|c| *c == 0 || s.channels.contains(c)))
    })
}

/// The requester incarnation of a flow name in this daemon's form.
fn incarnation(flow_name: &str) -> Option<&str> {
    let (head, _) = flow_name.split_once('-')?;
    (head.len() == 8 && head.bytes().all(|b| b.is_ascii_hexdigit())).then_some(head)
}

pub(super) struct FlowPort {
    socket: UdpSocket,
    pending: Mutex<Vec<Pending>>,
    tx: tokio::sync::Mutex<Tx>,
    media: Arc<crate::media::stats::MediaStats>,
    engine: EngineHandle,
    shared: Arc<Shared>,
    /// While the device access lock is set: the destinations it keeps
    /// serving (L9). `None` while unlocked.
    lock_basis: Mutex<Option<Vec<Served>>>,
}

impl FlowPort {
    pub(super) fn new(
        socket: UdpSocket,
        engine: EngineHandle,
        shared: Arc<Shared>,
        media: Arc<crate::media::stats::MediaStats>,
    ) -> Self {
        Self {
            media,
            socket,
            pending: Mutex::new(Vec::new()),
            tx: tokio::sync::Mutex::new(Tx::default()),
            engine,
            shared,
            lock_basis: Mutex::new(None),
        }
    }

    /// The device access lock was set or removed: remember the destinations
    /// served at this moment, or forget them (`device-lock.md` §9.2 L9).
    pub(super) async fn lock_changed(&self, locked: bool) {
        let tx = self.tx.lock().await;
        if let Ok(mut basis) = self.lock_basis.lock() {
            *basis = locked.then(|| Served::of(&tx.sessions));
        }
    }

    /// The destinations a locked device keeps serving; `None` when unlocked.
    /// A lock seen before its change was announced starts from now.
    fn lock_basis(&self, locked: bool, tx: &Tx) -> Option<Vec<Served>> {
        let mut basis = self.lock_basis.lock().ok()?;
        if !locked {
            *basis = None;
            return None;
        }
        Some(
            basis
                .get_or_insert_with(|| Served::of(&tx.sessions))
                .clone(),
        )
    }

    /// One request and its matched reply, or `None` on timeout. No resend.
    pub(super) async fn exchange(
        &self,
        to: SocketAddrV4,
        request: &[u8],
        sequence: u16,
        opcode: u16,
    ) -> Option<FlowReply> {
        let (reply, rx) = oneshot::channel();
        if let Ok(mut p) = self.pending.lock() {
            p.push(Pending {
                from: *to.ip(),
                sequence,
                opcode,
                reply,
            });
        }
        let sent = self.socket.send_to(request, to).await.is_ok();
        let outcome = if sent {
            tokio::time::timeout(REPLY_TIMEOUT, rx)
                .await
                .ok()
                .and_then(Result::ok)
        } else {
            None
        };
        if let Ok(mut p) = self.pending.lock() {
            p.retain(|x| !(x.from == *to.ip() && x.sequence == sequence && x.opcode == opcode));
        }
        outcome
    }

    pub(super) async fn serve(self: Arc<Self>) {
        let mut buf = vec![0u8; 2048];
        loop {
            let Ok((n, from)) = self.socket.recv_from(&mut buf).await else {
                continue;
            };
            let packet = &buf[..n];
            if wire::request(packet).is_some() {
                if let Some(out) = self.answer(packet).await {
                    let _ = self.socket.send_to(&out, from).await;
                }
                continue;
            }
            let SocketAddr::V4(from) = from else { continue };
            let (Some(sequence), Some(opcode)) = (wire::be16(packet, 4), wire::be16(packet, 6))
            else {
                continue;
            };
            let Some(reply) = wire::flow_reply(packet, sequence, opcode) else {
                continue;
            };
            let waiting = self.pending.lock().ok().and_then(|mut p| {
                let i = p.iter().position(|x| {
                    x.from == *from.ip() && x.sequence == sequence && x.opcode == opcode
                })?;
                Some(p.swap_remove(i))
            });
            if let Some(w) = waiting {
                let _ = w.reply.send(reply);
            }
        }
    }

    /// Answer a request as a transmitter (§§1-4).
    async fn answer(&self, packet: &[u8]) -> Option<Vec<u8>> {
        let (h, content) = wire::request(packet)?;
        let rp = self.engine.runtime_plan().await.ok()?;
        let d = rp.model.devices.first()?.clone();
        // Channel id 0 is an empty position, sent as silence (§1); at least
        // one position must carry a channel.
        let valid =
            |ids: &[u16]| ids.iter().any(|c| *c != 0) && ids.iter().all(|c| *c <= d.tx_channels);
        let locked = super::access::locked(&self.engine, &d.key).await?;
        let mut tx = self.tx.lock().await;
        let basis = self.lock_basis(locked, &tx);
        // While locked, a request that would start serving a receiver is
        // refused before anything else (`device-lock.md` §9.2 L9).
        let refused_by_lock = |address: Ipv4Addr, port: u16, channels: &[u16]| {
            basis
                .as_ref()
                .is_some_and(|b| !served_at_lock(b, address, port, channels))
        };
        let before = tx.clone();
        let result: Result<Vec<u8>, u16> = match h.opcode {
            0x0100 => {
                let r = wire::received_flow_request(packet)?;
                // The rate has a code of its own (§4); 0x0302 covers the
                // encoding (Tessera's choice, §4), channels and fpp.
                if refused_by_lock(r.address, r.port, &r.channels) {
                    Err(super::access::FLOW_LOCKED)
                } else if r.rate != d.sample_rate {
                    Err(0x0301)
                } else if r.bits != u32::from(d.encoding)
                    || !valid(&r.channels)
                    || r.fpp == 0
                    || r.fpp > MAX_FPP
                {
                    Err(0x0302)
                } else {
                    if let Some(new) = incarnation(&r.flow_name) {
                        tx.sessions.retain(|s| {
                            s.address != r.address
                                || incarnation(&s.flow_name).is_none_or(|old| old == new)
                        });
                    }
                    let now = tessera_audio_ipc::raw_monotonic_ns();
                    let free = tx.free_id();
                    if let Some(s) = tx
                        .sessions
                        .iter_mut()
                        .find(|s| s.address == r.address && s.port == r.port)
                    {
                        // The same destination is the same flow and gets
                        // the same answer (§1; `interop-fixes.md` F1).
                        s.channels = r.channels;
                        s.fpp = r.fpp;
                        s.receiver = r.receiver;
                        s.flow_name = r.flow_name;
                        s.created_ns = now;
                        Ok(wire::flow_accepted(s.handle()))
                    } else if let Some(id) = free {
                        let mut instance = [0u8; 4];
                        getrandom::fill(&mut instance).ok()?;
                        let s = Session {
                            id,
                            instance: u32::from_be_bytes(instance),
                            created_ns: now,
                            address: r.address,
                            port: r.port,
                            channels: r.channels,
                            fpp: r.fpp,
                            receiver: r.receiver,
                            flow_name: r.flow_name,
                        };
                        let reply = wire::flow_accepted(s.handle());
                        tx.sessions.push(s);
                        Ok(reply)
                    } else {
                        Err(0x0315)
                    }
                }
            }
            0x0101 => {
                let handle = content.get(..wire::FLOW_HANDLE_LEN)?;
                match tx.sessions.iter().position(|s| s.handle() == handle) {
                    Some(i) => {
                        tx.sessions.remove(i);
                        Ok(Vec::new())
                    }
                    None => Err(0x0103),
                }
            }
            0x0102 => {
                let handle = content.get(..wire::FLOW_HANDLE_LEN)?;
                let n = usize::from(wire::be16(content, 6)?);
                let ids = (0..n)
                    .map(|i| wire::be16(content, 8 + 2 * i))
                    .collect::<Option<Vec<u16>>>()?;
                match tx.sessions.iter_mut().find(|s| s.handle() == handle) {
                    Some(s) if refused_by_lock(s.address, s.port, &ids) => {
                        Err(super::access::FLOW_LOCKED)
                    }
                    Some(_) if !valid(&ids) => Err(0x0302),
                    Some(s) => {
                        // Channel list replaced, deadline refreshed
                        // (`state-machines.md` §3).
                        s.channels = ids;
                        s.created_ns = tessera_audio_ipc::raw_monotonic_ns();
                        Ok(Vec::new())
                    }
                    None => Err(0x0103),
                }
            }
            _ => return None,
        };
        let result = if *tx == before {
            result
        } else {
            match self.install(&tx, &d.key).await {
                Ok(()) => result,
                Err(()) => {
                    *tx = before;
                    Err(0x0315)
                }
            }
        };
        self.publish(&tx);
        Some(match result {
            Ok(content) => wire::reply(h, 1, &content),
            Err(code) => wire::reply(h, code, &[]),
        })
    }
}

impl FlowPort {
    /// Install the transmit half; on refusal nothing changes.
    async fn install(&self, tx: &Tx, device: &DeviceKey) -> Result<(), ()> {
        match self
            .shared
            .install(&self.engine, None, Some(tx.specs(device)))
            .await
        {
            Ok(()) => {
                tracing::info!(
                    "native transmit flows: {}",
                    tx.views()
                        .iter()
                        .map(|v| format!("{} {}:{} {:?}", v.id, v.address, v.port, v.channels))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                Ok(())
            }
            Err(e) => {
                tracing::warn!("cannot install native transmit flows: {e}");
                Err(())
            }
        }
    }

    fn publish(&self, tx: &Tx) {
        if let Ok(mut views) = self.shared.tx_flows.lock() {
            *views = tx.views();
        }
    }

    /// Take back the manual multicast flows stored in the protocol flow set
    /// (they persist with it) before anything installs the transmit half.
    pub(super) async fn restore(&self) {
        let Ok(rp) = self.engine.runtime_plan().await else {
            return;
        };
        let Some(d) = rp.model.devices.first() else {
            return;
        };
        let mut tx = self.tx.lock().await;
        tx.multicast = rp
            .model
            .flows
            .iter()
            .filter(|f| f.device == d.key.0 && f.direction == Direction::Tx)
            .filter_map(|f| {
                Some(Multicast {
                    id: f.id.strip_prefix(super::MULTICAST_PREFIX)?.parse().ok()?,
                    channels: f.channels.clone(),
                    group: f.peer.filter(Ipv4Addr::is_multicast)?,
                })
            })
            .collect();
        if let Ok(mut m) = self.shared.tx_specs.lock() {
            *m = tx.specs(&d.key);
        }
        self.publish(&tx);
        // Locked before this start: the unicast sessions stored in the flow
        // set are the ones served when it was locked (`device-lock.md` §9.2
        // L6, L9), so their receivers may ask again after a restart.
        if super::access::locked(&self.engine, &d.key).await == Some(true) {
            let served = rp
                .model
                .flows
                .iter()
                .filter(|f| {
                    f.device == d.key.0
                        && f.direction == Direction::Tx
                        && f.id.starts_with("native-tx-")
                })
                .filter_map(|f| {
                    Some(Served {
                        address: f.peer?,
                        port: f.port,
                        channels: f.channels.clone(),
                    })
                })
                .collect();
            if let Ok(mut basis) = self.lock_basis.lock() {
                *basis = Some(served);
            }
        }
    }

    /// ARC 0x2201: create the requested multicast flows that are valid; the
    /// ids created, empty when none was (`control-arc.md` §4.10).
    pub(super) async fn create_multicast(&self, requests: Vec<(u16, Vec<u16>)>) -> Vec<u16> {
        let Ok(rp) = self.engine.runtime_plan().await else {
            return Vec::new();
        };
        let Some(d) = rp.model.devices.first().cloned() else {
            return Vec::new();
        };
        let per_flow = d.tx_channels.clamp(1, 8);
        let mut tx = self.tx.lock().await;
        let before = tx.clone();
        let mut created = Vec::new();
        for (id, channels) in requests {
            // The id must be free among all transmit flows (§4.10).
            let usable = id >= 1
                && usize::from(id) <= MAX_SESSIONS
                && !tx.in_use(id)
                && !channels.is_empty()
                && channels.len() <= usize::from(per_flow)
                && channels.iter().all(|c| *c >= 1 && *c <= d.tx_channels);
            if !usable {
                continue;
            }
            // A new flow on this id replaces one still retiring: one sender
            // per group.
            tx.retiring.retain(|r| r.flow.id != id);
            tx.multicast.push(Multicast {
                id,
                channels,
                group: group(d.wire_device_id.0, id),
            });
            created.push(id);
        }
        if created.is_empty() || self.install(&tx, &d.key).await.is_err() {
            *tx = before;
            created.clear();
        }
        self.publish(&tx);
        created
    }

    /// Retire unicast sessions whose receiver has gone silent: heard at least
    /// once, then nothing for [`SILENT_NS`]; or never heard from within
    /// [`NEVER_HEARD_NS`] of its last request. Multicast flows are kept;
    /// deleted ones stop once their grace period is over.
    pub(super) async fn expire_silent(&self) {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let heard = self.media.tx_keepalives();
            let now = tessera_audio_ipc::raw_monotonic_ns();
            let Ok(rp) = self.engine.runtime_plan().await else {
                continue;
            };
            let Some(d) = rp.model.devices.first().cloned() else {
                continue;
            };
            let mut tx = self.tx.lock().await;
            let before = tx.clone();
            tx.sessions.retain(|s| {
                let heard_at = heard
                    .iter()
                    .find(|(id, _)| *id == s.spec_id())
                    .map_or(0, |(_, t)| *t);
                let last = heard_at.max(s.created_ns);
                let alive = if heard_at == 0 {
                    now.saturating_sub(s.created_ns) < NEVER_HEARD_NS
                } else {
                    now.saturating_sub(last) < SILENT_NS
                };
                if !alive {
                    // A receiver that sends keepalives the host firewall
                    // drops looks the same as one that vanished.
                    let why = if heard_at == 0 {
                        "no keepalive ever received (a firewall may drop them)"
                    } else {
                        "receiver silent"
                    };
                    tracing::info!(
                        "native transmit session to {}:{} ({}) retired: {why}",
                        s.address,
                        s.port,
                        s.receiver
                    );
                }
                alive
            });
            tx.drop_retired(now);
            if *tx != before {
                if self.install(&tx, &d.key).await.is_err() {
                    *tx = before;
                }
                self.publish(&tx);
            }
        }
    }

    /// ARC 0x2202: delete these multicast flows; whether any existed. The
    /// bundle record goes at once; the media keeps going for
    /// [`RETIRE_GRACE_NS`] so that receivers move to unicast first.
    pub(super) async fn delete_multicast(&self, ids: &[u16]) -> bool {
        let Ok(rp) = self.engine.runtime_plan().await else {
            return false;
        };
        let Some(d) = rp.model.devices.first().cloned() else {
            return false;
        };
        let mut tx = self.tx.lock().await;
        let before = tx.clone();
        let deleted = tx.retire(ids, tessera_audio_ipc::raw_monotonic_ns());
        if deleted && self.install(&tx, &d.key).await.is_err() {
            *tx = before;
            self.publish(&tx);
            return false;
        }
        self.publish(&tx);
        deleted
    }
}

/// The multicast group of flow `id`: 239.255.x.id, x from the device id, so
/// two devices rarely share a group. The reference picks a random address and
/// checks for collisions over mDNS (`state-machines.md` §3); not done here.
fn group(device_id: [u8; 8], id: u16) -> Ipv4Addr {
    let x = device_id.iter().fold(0u8, |a, b| a ^ b);
    Ipv4Addr::new(239, 255, x, (id & 0xFF) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::testing::TestEngine;

    #[test]
    fn only_this_daemons_flow_names_carry_an_incarnation() {
        assert_eq!(incarnation("ab12cd34-7"), Some("ab12cd34"));
        assert_eq!(incarnation("2_0"), None);
        assert_eq!(incarnation("2"), None);
        assert_eq!(incarnation("xyz-1"), None);
    }

    fn multicast(id: u16) -> Multicast {
        Multicast {
            id,
            channels: vec![1],
            group: group([0; 8], id),
        }
    }

    #[test]
    fn unicast_and_multicast_flows_take_ids_from_one_set() {
        let mut tx = Tx::default();
        assert_eq!(tx.free_id(), Some(1));
        tx.multicast = vec![multicast(1), multicast(3)];
        assert_eq!(tx.free_id(), Some(2));
        assert!(tx.in_use(3) && !tx.in_use(2));
        tx.multicast = (1..=MAX_SESSIONS as u16).map(multicast).collect();
        assert_eq!(tx.free_id(), None);
    }

    #[test]
    fn a_deleted_multicast_flow_keeps_sending_unadvertised_for_its_grace() {
        let device = DeviceKey("main".into());
        let mut tx = Tx {
            multicast: vec![multicast(3)],
            ..Tx::default()
        };
        assert!(tx.retire(&[3], 1_000));
        assert!(!tx.retire(&[3], 1_000), "already gone");
        // No longer listed or holding its id ...
        assert!(tx.views().is_empty() && !tx.in_use(3));
        // ... but still sent, under an id that is neither advertised as a
        // bundle nor restored at start.
        let ids: Vec<String> = tx.specs(&device).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["native-mc-retiring-3"]);
        assert!(
            ids[0]
                .strip_prefix(crate::native::MULTICAST_PREFIX)
                .and_then(|n| n.parse::<u16>().ok())
                .is_none()
        );
        tx.drop_retired(1_000 + RETIRE_GRACE_NS - 1);
        assert_eq!(tx.retiring.len(), 1);
        tx.drop_retired(1_000 + RETIRE_GRACE_NS);
        assert!(tx.specs(&device).is_empty());
    }

    #[tokio::test]
    async fn a_new_multicast_flow_replaces_a_retiring_one_with_its_id() {
        let (_t, port) = transmitter().await;
        assert_eq!(port.create_multicast(vec![(3, vec![1])]).await, [3]);
        assert!(port.delete_multicast(&[3]).await);
        assert_eq!(port.tx.lock().await.retiring.len(), 1);
        assert_eq!(port.create_multicast(vec![(3, vec![2])]).await, [3]);
        let tx = port.tx.lock().await;
        assert!(tx.retiring.is_empty(), "one sender per group");
        assert_eq!(tx.multicast[0].channels, [2]);
    }

    /// A transmitter with its own engine (device 2 RX, 2 TX).
    async fn transmitter() -> (TestEngine, FlowPort) {
        let t = TestEngine::start("id='main'\nname='Local'\nrx_channels=2\ntx_channels=2");
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = FlowPort::new(
            socket,
            t.engine.clone(),
            Arc::new(Shared::default()),
            Arc::new(crate::media::stats::MediaStats::default()),
        );
        (t, port)
    }

    /// The device's rate and bits.
    async fn format(t: &TestEngine) -> (u32, u32) {
        let rp = t.engine.runtime_plan().await.unwrap();
        let d = &rp.model.devices[0];
        (d.sample_rate, u32::from(d.encoding))
    }

    /// A 0x0100 in the observed layout (`control-flows.md` §7.1).
    fn request(rate: u32, bits: u32, channels: &[u16], fpp: u16, port: u16) -> Vec<u8> {
        wire::flow_request(&wire::FlowRequest {
            start: 0x1102,
            sequence: 7,
            rate,
            bits,
            channels,
            fpp,
            receiver: "rx-host-0000002",
            flow_name: "2",
            address: Ipv4Addr::new(192, 0, 2, 2),
            port,
        })
    }

    fn code(reply: &[u8]) -> u16 {
        wire::be16(reply, 8).unwrap()
    }

    /// 0x0101 or 0x0102 quoting `handle`.
    fn control(opcode: u16, handle: &[u8], channels: Option<&[u16]>) -> Vec<u8> {
        let mut content = handle.to_vec();
        if let Some(ids) = channels {
            content.extend_from_slice(&(ids.len() as u16).to_be_bytes());
            for id in ids {
                content.extend_from_slice(&id.to_be_bytes());
            }
        }
        let h = wire::Header {
            start: 0x1102,
            sequence: 8,
            opcode,
        };
        wire::reply(h, 0, &content)
    }

    async fn tx_flows(t: &TestEngine) -> Vec<tessera_state::config::FlowConfig> {
        let rp = t.engine.runtime_plan().await.unwrap();
        rp.model
            .flows
            .into_iter()
            .filter(|f| f.id.starts_with("native-tx-"))
            .collect()
    }

    #[tokio::test]
    async fn a_flow_request_is_answered_as_real_transmitters_answer() {
        let (t, port) = transmitter().await;
        let (rate, bits) = format(&t).await;
        let ask = request(rate, bits, &[1, 2], 32, 14339);
        let reply = port.answer(&ask).await.unwrap();
        // 24 bytes: header echoing start code, sequence and opcode, result 1,
        // then flow id, instance, u16 1, u16 1, 4 zero bytes (§7.2).
        assert_eq!(reply.len(), 24);
        assert_eq!(&reply[..10], &[0x11, 0x02, 0, 24, 0, 7, 1, 0, 0, 1]);
        assert_eq!(&reply[10..12], &[0, 1]);
        assert_eq!(&reply[16..], &[0, 1, 0, 1, 0, 0, 0, 0]);
        let flows = tx_flows(&t).await;
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].channels, [1, 2]);
        assert_eq!(
            (flows[0].port, flows[0].frames_per_packet),
            (14339, Some(32))
        );
        assert_eq!(flows[0].peer, Some(Ipv4Addr::new(192, 0, 2, 2)));
        // A repeat to the same destination is the same flow with the same
        // answer (§1; `interop-fixes.md` F1).
        assert_eq!(port.answer(&ask).await.unwrap(), reply);
        assert_eq!(tx_flows(&t).await.len(), 1);
        // Another destination is another flow with the next id.
        let other = port
            .answer(&request(rate, bits, &[2], 32, 14340))
            .await
            .unwrap();
        assert_eq!(code(&other), 1);
        assert_eq!(&other[10..12], &[0, 2]);
        let views = port.tx.lock().await.views();
        assert_eq!(
            views
                .iter()
                .map(|v| (v.id, v.local_name.as_str()))
                .collect::<Vec<_>>(),
            [(1, "1_0"), (2, "2_0")]
        );
    }

    #[tokio::test]
    async fn an_empty_position_is_accepted_and_sent_as_silence() {
        // A hardware receiver's first request after a one-channel
        // subscription asks for ids 1, 0 (`control-flows.md` §§1, 7.1).
        let (t, port) = transmitter().await;
        let (rate, bits) = format(&t).await;
        let reply = port
            .answer(&request(rate, bits, &[1, 0], 32, 14339))
            .await
            .unwrap();
        assert_eq!(code(&reply), 1);
        let flows = tx_flows(&t).await;
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].channels, [1, 0]);
    }

    #[tokio::test]
    async fn refusals_carry_their_codes_and_change_nothing() {
        let (t, port) = transmitter().await;
        let (rate, bits) = format(&t).await;
        let other_rate = if rate == 48_000 { 96_000 } else { 48_000 };
        let other_bits = if bits == 24 { 16 } else { 24 };
        for (ask, expected) in [
            // Sample rate mismatch, also when the encoding differs too.
            (request(other_rate, bits, &[1], 32, 14339), 0x0301),
            (request(other_rate, other_bits, &[1], 32, 14339), 0x0301),
            // Encoding mismatch (Tessera's code, §4).
            (request(rate, other_bits, &[1], 32, 14339), 0x0302),
            // Channel out of range; only empty positions.
            (request(rate, bits, &[3], 32, 14339), 0x0302),
            (request(rate, bits, &[0, 0], 32, 14339), 0x0302),
            // fpp 0 or above 256.
            (request(rate, bits, &[1], 0, 14339), 0x0302),
            (request(rate, bits, &[1], 257, 14339), 0x0302),
        ] {
            let reply = port.answer(&ask).await.unwrap();
            assert_eq!(code(&reply), expected);
            assert_eq!(reply.len(), wire::HEADER_LEN);
        }
        assert!(tx_flows(&t).await.is_empty());
        assert!(port.tx.lock().await.sessions.is_empty());
    }

    #[tokio::test]
    async fn stop_and_update_need_the_live_handle() {
        let (t, port) = transmitter().await;
        let (rate, bits) = format(&t).await;
        let reply = port
            .answer(&request(rate, bits, &[1], 32, 14339))
            .await
            .unwrap();
        let handle = reply[10..16].to_vec();
        // A handle with the right id but another instance is stale (§§2-3).
        let mut stale = handle.clone();
        stale[5] ^= 0xFF;
        let refused = port.answer(&control(0x0102, &stale, Some(&[2]))).await;
        assert_eq!(code(&refused.unwrap()), 0x0103);
        let refused = port.answer(&control(0x0101, &stale, None)).await;
        assert_eq!(code(&refused.unwrap()), 0x0103);
        // Update: the channel list is replaced.
        let updated = port
            .answer(&control(0x0102, &handle, Some(&[2, 1])))
            .await
            .unwrap();
        assert_eq!((code(&updated), updated.len()), (1, wire::HEADER_LEN));
        assert_eq!(tx_flows(&t).await[0].channels, [2, 1]);
        let bad = port.answer(&control(0x0102, &handle, Some(&[5]))).await;
        assert_eq!(code(&bad.unwrap()), 0x0302);
        // Stop: the flow is gone, a second stop finds nothing.
        let stopped = port.answer(&control(0x0101, &handle, None)).await;
        assert_eq!(code(&stopped.unwrap()), 1);
        assert!(tx_flows(&t).await.is_empty());
        let again = port.answer(&control(0x0101, &handle, None)).await;
        assert_eq!(code(&again.unwrap()), 0x0103);
    }

    #[tokio::test]
    async fn unicast_and_multicast_flows_share_the_advertised_limit() {
        let (t, port) = transmitter().await;
        let (rate, bits) = format(&t).await;
        let ids = 2..=MAX_SESSIONS as u16;
        let created = port
            .create_multicast(ids.map(|id| (id, vec![1])).collect())
            .await;
        assert_eq!(created.len(), MAX_SESSIONS - 1);
        // The one free id goes to a unicast session...
        let first = port
            .answer(&request(rate, bits, &[1], 32, 14339))
            .await
            .unwrap();
        assert_eq!((code(&first), &first[10..12]), (1, &[0u8, 1][..]));
        // ...then every flow is in use (0x0315), for either kind.
        let full = port
            .answer(&request(rate, bits, &[1], 32, 14340))
            .await
            .unwrap();
        assert_eq!(code(&full), 0x0315);
        assert!(port.create_multicast(vec![(1, vec![1])]).await.is_empty());
        assert!(port.delete_multicast(&[2]).await);
        let second = port
            .answer(&request(rate, bits, &[1], 32, 14340))
            .await
            .unwrap();
        assert_eq!((code(&second), &second[10..12]), (1, &[0u8, 2][..]));
    }
}
