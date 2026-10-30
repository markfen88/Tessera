//! Byte layouts of the device role: legacy ARC server replies, CMC, the
//! device-info/heartbeat multicasts and the receiver side of flow control.
//!
//! Sources: `spec/protocol/control-arc.md` §§1, 2.2, 4, 5, 6, 9.2-9.4;
//! `control-cmc.md` §§1, 3; `conmon.md` §§2-6, 9; `control-flows.md` §§1-4,
//! 7.1-7.2. Unknown constants keep the values the specs record for the
//! legacy (2.7.41) family that tessera advertises. Pure functions only.

use std::net::Ipv4Addr;

/// Shared 10-byte header of ARC, CMC and flow control (`control-arc.md` §1).
pub const HEADER_LEN: usize = 10;
/// Generic failure (`control-arc.md` §2.2).
pub const FAILURE: u16 = 0xFFFF;
/// Success, more pages follow (§2.2).
pub const MORE: u16 = 0x8112;
/// What a device answers to an opcode it does not implement (§9.3).
pub const UNIMPLEMENTED: u16 = 0x0030;
/// Channel items per list page (§6).
const CHANNEL_SLOTS: usize = 32;
/// Flow items per list page (§6).
const FLOW_SLOTS: usize = 16;
/// Soft size limit of a reply's variable data (§6).
const SOFT_LIMIT: usize = 800;

/// A parsed request header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub start: u16,
    pub sequence: u16,
    pub opcode: u16,
}

/// Parse a request. Requests with a bad length or opcode2 != 0 are dropped
/// (§1: the reference ignores them).
#[must_use]
pub fn request(bytes: &[u8]) -> Option<(Header, &[u8])> {
    if bytes.len() < HEADER_LEN || usize::from(be16(bytes, 2)?) != bytes.len() {
        return None;
    }
    if be16(bytes, 8)? != 0 {
        return None;
    }
    Some((
        Header {
            start: be16(bytes, 0)?,
            sequence: be16(bytes, 4)?,
            opcode: be16(bytes, 6)?,
        },
        &bytes[HEADER_LEN..],
    ))
}

/// A reply to `header` with result `code` and `content` (start code echoed).
#[must_use]
pub fn reply(header: Header, code: u16, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + content.len());
    let len = u16::try_from(HEADER_LEN + content.len()).unwrap_or(u16::MAX);
    for v in [header.start, len, header.sequence, header.opcode, code] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out.extend_from_slice(content);
    out
}

/// A big-endian u16 at `at`, if present.
#[must_use]
pub fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

/// The NUL-terminated string at packet offset `offset` of a request whose
/// content starts at packet offset 10. Offsets below 10 mean absent (§5.1).
#[must_use]
pub fn string_at(content: &[u8], offset: u16) -> Option<&str> {
    let at = usize::from(offset).checked_sub(HEADER_LEN)?;
    let tail = content.get(at..)?;
    let end = tail.iter().position(|b| *b == 0)?;
    std::str::from_utf8(&tail[..end])
        .ok()
        .filter(|s| !s.is_empty())
}

/// Growable reply content whose stored offsets are packet offsets.
struct Content(Vec<u8>);

impl Content {
    fn new() -> Self {
        Self(Vec::new())
    }
    fn offset(&self) -> u16 {
        u16::try_from(self.0.len() + HEADER_LEN).unwrap_or(u16::MAX)
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn set16(&mut self, at: usize, v: u16) {
        self.0[at..at + 2].copy_from_slice(&v.to_be_bytes());
    }
    fn string(&mut self, s: &str) -> u16 {
        let at = self.offset();
        self.0.extend_from_slice(s.as_bytes());
        self.0.push(0);
        at
    }
    fn pad(&mut self, to: usize) {
        while !(self.0.len() + HEADER_LEN).is_multiple_of(to) {
            self.0.push(0);
        }
    }
}

/// The device as the ARC replies describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceView {
    pub name: String,
    pub rate: u32,
    pub bits: u16,
    pub latency_ns: u32,
    pub tx_labels: Vec<String>,
    /// One row per RX channel, in channel order (id = index + 1).
    pub rx: Vec<RxRow>,
}

/// One RX channel in an ARC 0x3000 reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxRow {
    pub label: String,
    /// (TX channel, TX device).
    pub source: Option<(String, String)>,
    pub status: u32,
}

/// A receive flow in an ARC 0x3200 reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxFlowView {
    /// 1-based, kept for the flow's lifetime; its heartbeat slot is id - 1
    /// (`metering.md` §2.6 item 5).
    pub id: u16,
    /// Keys in the protocol flow set of the listeners that make up the flow
    /// (not on the wire): one for a unicast flow, one per input for a group.
    pub flows: Vec<String>,
    pub local: Ipv4Addr,
    pub port: u16,
    /// Local RX channels fed by each wire position.
    pub positions: Vec<Vec<u16>>,
}

/// Status codes of ARC 0x3000 (§4.4).
pub mod status {
    pub const NONE: u32 = 0;
    pub const UNRESOLVED: u32 = 0x0000_0001;
    pub const WAITING: u32 = 0x0000_0008;
    pub const TX_FAIL: u32 = 0x0000_0015;
    pub const RECEIVING: u32 = 0x0101_0009;
    pub const RECEIVING_MULTICAST: u32 = 0x0101_000A;
}

/// 0x1000: counts and capabilities, 34 bytes (§4.1). `tx_rename`: TX
/// channels can be renamed with 0x2013 now (not while their labels are
/// locked).
#[must_use]
pub fn counts(view: &DeviceView, max_rx_flows: u16, tx_rename: bool) -> Vec<u8> {
    let tx = u16::try_from(view.tx_labels.len()).unwrap_or(u16::MAX);
    let rx = u16::try_from(view.rx.len()).unwrap_or(u16::MAX);
    let mut c = Content::new();
    // +0 unknown byte, +1 flags: 0x20 multicast flows can be created, 0x10
    // TX channels can be renamed (`device-settings.md` §12.9 step 1).
    c.u16(0x0020 | if tx_rename { 0x0010 } else { 0 });
    c.u16(tx);
    c.u16(rx);
    c.u16(4);
    c.u16(tx.min(8));
    c.u16(8);
    c.u16(32);
    c.u16(max_rx_flows);
    c.u16(tx.saturating_add(rx));
    c.u16(1);
    c.u16(1);
    c.0.extend_from_slice(&[0; 12]);
    c.0
}

/// 0x1002: the friendly name (§4.2).
#[must_use]
pub fn name(view: &DeviceView) -> Vec<u8> {
    let mut c = Content::new();
    c.string(&view.name);
    c.0
}

/// 0x1003: device names block, legacy layout (§4.3).
#[must_use]
pub fn names_block(view: &DeviceView, board: &str, revision: &str) -> Vec<u8> {
    let mut c = Content::new();
    c.0.resize(0x26, 0);
    c.set16(0x1E, 0x2729);
    c.set16(0x22, 0x1102);
    let friendly = c.string(&view.name);
    let factory = c.string(&view.name);
    let board_at = c.string(board);
    let revision_at = c.string(revision);
    c.set16(0x06, board_at);
    c.set16(0x08, revision_at);
    c.set16(0x0C, friendly);
    c.set16(0x0E, factory);
    c.set16(0x10, friendly);
    c.0
}

/// Paging parameters of a list request (§6, §9.2): one-based first index.
#[must_use]
pub fn page_start(content: &[u8]) -> Option<usize> {
    let first = be16(content, 2)?;
    (first != 0).then_some(usize::from(first))
}

fn descriptor(c: &mut Content, view: &DeviceView) -> u16 {
    let at = c.offset();
    c.u32(view.rate);
    c.0.extend_from_slice(&[1, 1]);
    c.u16(view.bits);
    c.u16(0x0400);
    c.u16(view.bits);
    c.u16(view.bits);
    c.u16(0x000E);
    at
}

/// 0x3000: RX channels with subscription state (§4.4), from `first`.
/// Returns the content and whether more pages follow.
#[must_use]
pub fn rx_list(view: &DeviceView, first: usize) -> (Vec<u8>, bool) {
    let rows: Vec<(usize, &RxRow)> = view
        .rx
        .iter()
        .enumerate()
        .skip(first - 1)
        .take(CHANNEL_SLOTS)
        .collect();
    let mut c = Content::new();
    c.0.extend_from_slice(&[rows.len() as u8, 0]);
    c.0.resize(2 + rows.len() * 20, 0);
    if rows.is_empty() {
        return (c.0, false);
    }
    let shared = descriptor(&mut c, view);
    let mut count = 0;
    for (n, (index, row)) in rows.iter().enumerate() {
        let item = 2 + n * 20;
        let (tx_channel, tx_device) = match &row.source {
            Some((channel, device)) => (c.string(channel), c.string(device)),
            None => (0, 0),
        };
        let label = c.string(&row.label);
        c.set16(item, (*index + 1) as u16);
        c.set16(item + 2, 6);
        c.set16(item + 4, shared);
        c.set16(item + 6, tx_channel);
        c.set16(item + 8, tx_device);
        c.set16(item + 10, label);
        c.0[item + 12..item + 16].copy_from_slice(&row.status.to_be_bytes());
        count += 1;
        if c.0.len() > SOFT_LIMIT {
            break;
        }
    }
    c.0[1] = count as u8;
    let more = first - 1 + count < view.rx.len();
    (c.0, more)
}

/// The factory name of TX channel `id`: the name of its `_netaudio-chan`
/// record flagged `default` (`discovery-mdns.md` §3.3). 0x2000 must report
/// the same name, because a controller may subscribe by it (§5.1).
#[must_use]
pub fn tx_factory_name(id: u16) -> String {
    format!("TX {id}")
}

/// 0x2000 (factory names) or 0x2010 (friendly names): TX channels (§§4.5-4.6).
/// A page ends after the item whose strings pass the soft limit (§6).
#[must_use]
pub fn tx_list(view: &DeviceView, first: usize, friendly: bool) -> (Vec<u8>, bool) {
    let size = if friendly { 6 } else { 8 };
    let rows: Vec<(usize, &String)> = view
        .tx_labels
        .iter()
        .enumerate()
        .skip(first - 1)
        .take(CHANNEL_SLOTS)
        .collect();
    let mut c = Content::new();
    c.0.extend_from_slice(&[rows.len() as u8, rows.len() as u8]);
    c.0.resize(2 + rows.len() * size, 0);
    if rows.is_empty() {
        return (c.0, false);
    }
    let shared = if friendly {
        c.0.extend_from_slice(&[0; 4]);
        0
    } else {
        descriptor(&mut c, view)
    };
    let mut count = 0;
    for (n, (index, label)) in rows.iter().enumerate() {
        let item = 2 + n * size;
        let id = (*index + 1) as u16;
        let text = if friendly {
            (*label).clone()
        } else {
            tx_factory_name(id)
        };
        let at = c.string(&text);
        c.set16(item, id);
        if friendly {
            c.set16(item + 2, id);
            c.set16(item + 4, at);
        } else {
            c.set16(item + 2, 7);
            c.set16(item + 4, shared);
            c.set16(item + 6, at);
        }
        count += 1;
        if c.0.len() > SOFT_LIMIT {
            break;
        }
    }
    c.0[1] = count as u8;
    let more = first - 1 + count < view.tx_labels.len();
    (c.0, more)
}

/// 0x3200: RX flows (§4.13) in id order from the one-based item `first`, at
/// most 16 per page and paged like the other lists (§6), so that every flow
/// with a heartbeat slot is listed (`metering.md` §2.6 item 5). Returns the
/// content and whether more pages follow.
#[must_use]
pub fn rx_flows(view: &DeviceView, flows: &[RxFlowView], first: usize) -> (Vec<u8>, bool) {
    let page: Vec<&RxFlowView> = flows.iter().skip(first - 1).take(FLOW_SLOTS).collect();
    let words = view.rx.len().div_ceil(16).max(1);
    let mut c = Content::new();
    c.0.extend_from_slice(&[page.len() as u8, page.len() as u8]);
    c.0.resize(2 + page.len() * 2, 0);
    let mut count = 0;
    for (n, flow) in page.iter().enumerate() {
        c.pad(4);
        let socket = c.offset();
        // `08 02` as in the real devices' flow records and the old runtime's
        // list (`metering.md` §2.6 item 5); 0x8002 is the reference's value
        // for TX flows (§4.9).
        c.u16(0x0802);
        c.u16(flow.port);
        c.0.extend_from_slice(&flow.local.octets());
        let second = c.offset();
        for v in [9u16, 1, 0x0800, 0] {
            c.u16(v);
        }
        c.u32(view.latency_ns);
        c.u32(0);
        let mut masks = Vec::with_capacity(flow.positions.len());
        for locals in &flow.positions {
            masks.push(c.offset());
            let mut mask = vec![0u16; words];
            for local in locals {
                let bit = usize::from(local.saturating_sub(1));
                if let Some(w) = mask.get_mut(bit / 16) {
                    *w |= 1 << (bit % 16);
                }
            }
            for w in mask {
                c.u16(w);
            }
        }
        c.pad(4);
        let main = c.offset();
        c.u16(flow.id);
        c.u16(1);
        c.u32(view.rate);
        c.u16(0);
        c.u16(view.bits);
        c.u16(1);
        c.u16(flow.positions.len() as u16);
        c.u16(words as u16);
        c.u16(socket);
        for m in masks {
            c.u16(m);
        }
        c.u16(second);
        c.set16(2 + n * 2, main);
        count += 1;
        if c.0.len() > SOFT_LIMIT {
            break;
        }
    }
    c.0[1] = count as u8;
    let more = first - 1 + count < flows.len();
    (c.0, more)
}

/// First and last local media receive port (`control-arc.md` §4.12): inside
/// the first range of the 0x3300 answer and the published unicast audio range
/// 14336-15359.
pub const RX_PORT_FIRST: u16 = 0x3800;
/// See [`RX_PORT_FIRST`].
pub const RX_PORT_LAST: u16 = 0x38FD;

/// 0x3300: the answer the legacy family requires (§4.12); its first range is
/// the receive ports above. Field meanings are unproven.
pub const RECEIVE_PORTS: [u8; 8] = {
    let a = RX_PORT_FIRST.to_be_bytes();
    let b = RX_PORT_LAST.to_be_bytes();
    [a[0], a[1], b[0], b[1], 0x38, 0xFE, 0x38, 0xFF]
};

/// A requested subscription: local channel and (TX channel, TX device), or
/// `None` to unsubscribe.
pub type SubscriptionItem = (u16, Option<(String, String)>);

/// Items of 0x3010 (§5.1). Channel 0 or a half source is skipped.
#[must_use]
pub fn subscriptions(content: &[u8]) -> Vec<SubscriptionItem> {
    let count = usize::from(*content.get(1).unwrap_or(&0));
    let mut out = Vec::new();
    for n in 0..count {
        let at = 2 + n * 6;
        let (Some(rx), Some(channel), Some(device)) = (
            be16(content, at),
            be16(content, at + 2),
            be16(content, at + 4),
        ) else {
            break;
        };
        if rx == 0 {
            continue;
        }
        let channel = string_at(content, channel);
        let device = string_at(content, device);
        match (channel, device) {
            (Some(c), Some(d)) => out.push((rx, Some((c.to_owned(), d.to_owned())))),
            (None, None) => out.push((rx, None)),
            _ => {}
        }
    }
    out
}

/// The name of an ARC 0x1001 device rename: the content from its first byte
/// up to the NUL (`device-settings.md` §12.7, corrected: there is no u16
/// before the name). `None` for an empty name: a reset to the default name
/// is only a `guess` there, and is refused.
#[must_use]
pub fn rename(content: &[u8]) -> Option<&str> {
    let end = content
        .iter()
        .position(|b| *b == 0)
        .unwrap_or(content.len());
    std::str::from_utf8(&content[..end])
        .ok()
        .filter(|s| !s.is_empty())
}

/// Items of an ARC 0x2013 TX channel rename (`control-arc.md` §4.7): 6-byte
/// items of (u16 unknown, u16 TX channel id, u16 packet offset of the new
/// name). Channel 0 and an absent or empty name are skipped (the reference
/// ignores an empty name; whether it would reset the name is UNKNOWN).
#[must_use]
pub fn tx_renames(content: &[u8]) -> Vec<(u16, String)> {
    let count = usize::from(*content.get(1).unwrap_or(&0));
    (0..count)
        .map_while(|n| {
            let at = 2 + n * 6;
            Some((be16(content, at + 2)?, be16(content, at + 4)?))
        })
        .filter_map(|(tx, offset)| {
            // Offset 0 after subtracting the header is "no name" (§4.7).
            if tx == 0 || usize::from(offset) <= HEADER_LEN {
                return None;
            }
            Some((tx, string_at(content, offset)?.to_owned()))
        })
        .collect()
}

/// Requests of an ARC 0x2201 multicast create (`control-arc.md` §4.10): per
/// descriptor the controller-chosen flow id and the TX channel ids. A
/// descriptor that is not of type 2 (multicast) is skipped.
#[must_use]
pub fn multicast_creates(content: &[u8]) -> Vec<(u16, Vec<u16>)> {
    let count = usize::from(*content.get(1).unwrap_or(&0));
    (0..count)
        .filter_map(|n| {
            let at = usize::from(be16(content, 2 + 2 * n)?).checked_sub(HEADER_LEN)?;
            let id = be16(content, at)?;
            if be16(content, at + 2)? != 2 {
                return None;
            }
            let channels = usize::from(be16(content, at + 14)?);
            let ids = (0..channels)
                .map(|i| be16(content, at + 16 + 2 * i))
                .collect::<Option<Vec<u16>>>()?;
            Some((id, ids))
        })
        .collect()
}

/// Reply content of a successful 0x2201: count, 0, the created flow ids.
#[must_use]
pub fn multicast_created(ids: &[u16]) -> Vec<u8> {
    let mut c = Content::new();
    c.u16(ids.len() as u16);
    c.u16(0);
    for id in ids {
        c.u16(*id);
    }
    c.0
}

/// Flow ids of an ARC 0x2202 multicast delete (`control-arc.md` §4.11).
#[must_use]
pub fn multicast_deletes(content: &[u8]) -> Vec<u16> {
    let count = usize::from(be16(content, 0).unwrap_or(0));
    (0..count)
        .filter_map(|i| be16(content, 4 + 2 * i))
        .collect()
}

/// 0x3014: the alternative removal form; the channel is the u16 at +4 (§5.3).
#[must_use]
pub fn removal(content: &[u8]) -> Option<u16> {
    be16(content, 4).filter(|c| *c != 0)
}

/// 0x3001: RX renames, 4-byte items of (channel, name offset) (§4.8).
#[must_use]
pub fn renames(content: &[u8]) -> Vec<(u16, String)> {
    let count = usize::from(*content.get(1).unwrap_or(&0));
    (0..count)
        .filter_map(|n| {
            let at = 2 + n * 4;
            let rx = be16(content, at)?;
            let name = string_at(content, be16(content, at + 2)?)?;
            (rx != 0).then(|| (rx, name.to_owned()))
        })
        .collect()
}

/// CMC 0x1001 reply content, 22 bytes (`control-cmc.md` §1).
#[must_use]
pub fn cmc_advertisement(device_id: [u8; 8], address: Ipv4Addr, info_port: u16) -> Vec<u8> {
    let mut c = Content::new();
    c.u16(0);
    c.0.extend_from_slice(&device_id);
    c.u16(1);
    c.u16(0);
    c.0.extend_from_slice(&address.octets());
    c.u16(info_port);
    c.u16(0);
    c.0
}

/// Device-info multicast header (`conmon.md` §2), 32 bytes, then content.
#[must_use]
pub fn info_message(
    start: u16,
    sequence: u16,
    device_id: [u8; 8],
    vendor: &[u8; 8],
    opcode: [u8; 8],
    content: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + content.len());
    out.extend_from_slice(&start.to_be_bytes());
    out.extend_from_slice(&u16::try_from(32 + content.len()).unwrap_or(0).to_be_bytes());
    out.extend_from_slice(&sequence.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&device_id);
    out.extend_from_slice(vendor);
    out.extend_from_slice(&opcode);
    out.extend_from_slice(content);
    out
}

/// The message id of an info request (`conmon.md` §§2-3), if well formed.
#[must_use]
pub fn info_request(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < 32 || be16(bytes, 0)? != 0xFFFF {
        return None;
    }
    be16(bytes, 26)
}

/// Opcode of an info message we send with message id `id` (§3).
#[must_use]
pub fn info_opcode(id: u16) -> [u8; 8] {
    let id = id.to_be_bytes();
    [0x07, 0x2A, id[0], id[1], 0, 0, 0, 0]
}

/// Heartbeat opcode (`conmon.md` §6).
pub const HEARTBEAT_OPCODE: [u8; 8] = [0x00, 0x08, 0x00, 0x01, 0x10, 0x00, 0x00, 0x00];

/// What the device knows about itself for the info replies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub device_id: [u8; 8],
    pub mac: [u8; 6],
    pub address: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub gateway: Ipv4Addr,
    /// The host's DNS server, unspecified when it has none (0x0011 +0x1C).
    pub dns: Ipv4Addr,
    /// Link speed of the media interface, Mb/s (0x0011 +0x06, 0x0040).
    pub link_mbps: u16,
    /// TX bytes/s, RX bytes/s, TX errors, RX errors of the media interface
    /// (`device-settings.md` §8): the heartbeat's latest 0x8000 sample.
    pub traffic: [u32; 4],
    pub board: String,
    pub manufacturer: String,
    pub model: String,
    pub version: (u8, u8, u16),
    pub rate: u32,
    pub bits: u32,
    /// Media clock correction, parts per billion.
    pub ppb: i32,
    /// Leader identity as a 6-byte MAC, when known.
    pub leader: Option<[u8; 6]>,
    /// IEEE 1588 state of the PTP port: 6 leader, 9 follower, 4 listening.
    pub port_state: u16,
    /// Board-info capability word (content 0x14).
    pub capabilities: u32,
    /// The clear-configuration mode accepted and waiting for the role to
    /// restart (0x0078 +0x04), 0 when none.
    pub clearing: u16,
    /// Clock capability word of the clock-status record (content 0x2C).
    pub clock_capabilities: u16,
    /// Sample rates the device can be set to now, the current one included
    /// (0x0080 list; the state's format options, `device-settings.md` §3.3).
    pub rates: Vec<u32>,
    /// Encodings (bits) the device can be set to now (0x0082 list).
    pub encodings: Vec<u32>,
    /// Preferred leader is set (clock priority below 64).
    pub preferred: bool,
    /// The clock role is follower only.
    pub follower_only: bool,
    /// PTPv1 stratum the device advertises now (0xFF when it cannot lead);
    /// clock-status content 0x07 (`device-settings.md` §2.3).
    pub stratum: u8,
}

fn fixed(text: &str, len: usize) -> Vec<u8> {
    let mut out = text.as_bytes()[..text.len().min(len)].to_vec();
    out.resize(len, 0);
    out
}

/// Content of the info reply answering request id `request` (`conmon.md`
/// §§3, 4, 9.2-9.3), with its response id. `None` for unknown requests.
#[must_use]
pub fn info_reply(request: u16, me: &Identity) -> Option<(u16, Vec<u8>)> {
    let (major, minor, patch) = me.version;
    let mut version = vec![major, minor];
    version.extend_from_slice(&patch.to_be_bytes());
    Some(match request {
        0x0061 => {
            let mut c = vec![0u8; 200];
            c[0..4].copy_from_slice(&version);
            c[4..8].copy_from_slice(&version);
            c[0x0C..0x14].copy_from_slice(&fixed(&me.board, 8));
            c[0x14..0x18].copy_from_slice(&me.capabilities.to_be_bytes());
            c[0x23] = 2;
            c[0x27] = 1;
            c[0x28] = 1;
            c[0x38..0x48].copy_from_slice(&fixed(&me.board, 16));
            // Non-zero, or a controller floods the device with requests (§4.1).
            c[0xBB] = 0x1F;
            (0x0060, c)
        }
        0x00C1 => {
            let mut c = vec![0u8; 336];
            c[0..8].copy_from_slice(&fixed(&me.manufacturer, 8));
            c[8..16].copy_from_slice(&fixed(&me.board, 8));
            c[0x1C..0x20].copy_from_slice(&version);
            c[0x2C..0x3C].copy_from_slice(&fixed(&me.manufacturer, 16));
            c[0xAC..0xBC].copy_from_slice(&fixed(&me.model, 16));
            (0x00C0, c)
        }
        0x0013 => {
            let mut c = vec![0u8; 64];
            c[0..2].copy_from_slice(&[0, 1]);
            // The real link speed and DNS server, not the old runtime's
            // constants (`device-settings.md` §7).
            c[6..8].copy_from_slice(&me.link_mbps.to_be_bytes());
            c[8..10].copy_from_slice(&1u16.to_be_bytes());
            c[0x0A..0x10].copy_from_slice(&me.mac);
            c[0x10..0x14].copy_from_slice(&me.address.octets());
            c[0x14..0x18].copy_from_slice(&me.netmask.octets());
            c[0x18..0x1C].copy_from_slice(&me.gateway.octets());
            c[0x1C..0x20].copy_from_slice(&me.dns.octets());
            c[0x20..0x24].copy_from_slice(&[0x00, 0x18, 0x00, 0x30]);
            (0x0011, c)
        }
        0x0021 => (0x0020, clock_status(me)),
        0x0041 if INTERFACE_STATISTICS_LAYOUT_KNOWN => (
            0x0040,
            interface_statistics(me.traffic, u32::from(me.link_mbps)),
        ),
        0x0077 => (0x0078, clear_status(me.clearing)),
        0x0081 => (0x0080, value_list(me.rate, &me.rates)),
        0x0083 => (0x0082, value_list(me.bits, &me.encodings)),
        // 0x1008, the lock state, is the access lock's (`native/access.rs`).
        _ => return None,
    })
}

/// IEEE 1588 port states used in the clock-status record.
pub mod port_state {
    pub const LISTENING: u16 = 4;
    pub const LEADER: u16 = 6;
    pub const FOLLOWER: u16 = 9;
}

fn eui(mac: [u8; 6]) -> [u8; 8] {
    [mac[0], mac[1], mac[2], 0xFF, 0xFE, mac[3], mac[4], mac[5]]
}

/// Clock capability word of the clock-status record (content 0x2C). The
/// follower value 0x0234 claims subdomain, unicast-delay and port control
/// (bits 0x0200, 0x0010, 0x0004, `device-settings.md` §12.3), none of which
/// tessera implements; 0x0080 offers preferred leader, which it does.
pub const CLOCK_CAPABILITIES: u16 = 0x00A0;

/// Clock-status record 0x0020 in the real-device layout, one PTPv1 primary
/// port (`device-settings.md` §2.1, follower template of the soundcard with
/// the live port state; words of unknown meaning keep the observed values).
/// Not preferred: preferred leader is not implemented yet (§2.3).
fn clock_status(me: &Identity) -> Vec<u8> {
    fn put16(c: &mut [u8], at: usize, v: u16) {
        c[at..at + 2].copy_from_slice(&v.to_be_bytes());
    }
    let mut c = vec![0u8; 192];
    let leader = me.leader.unwrap_or(me.mac);
    put16(&mut c, 0x00, 3);
    put16(&mut c, 0x02, 3);
    c[0x06] = u8::from(me.preferred);
    c[0x07] = me.stratum;
    c[0x08..0x0C].copy_from_slice(&me.ppb.to_be_bytes());
    c[0x0C..0x12].copy_from_slice(&me.mac);
    c[0x14..0x1A].copy_from_slice(&leader);
    c[0x1C..0x22].copy_from_slice(&leader);
    put16(&mut c, 0x24, 1);
    put16(&mut c, 0x26, 0x0034);
    put16(&mut c, 0x28, me.port_state);
    put16(&mut c, 0x2C, me.clock_capabilities);
    // Clock flags (content 0x36): 0x0004 = follower only, set only for that
    // role (`device-settings.md` §12.3); 0x0002 is in every record seen.
    let flags: u32 = 0x0002 | if me.follower_only { 0x0004 } else { 0 };
    c[0x34..0x38].copy_from_slice(&flags.to_be_bytes());
    // Maximum drift 200 000 ppb and word-clock state 1 ("none": tessera has
    // no word-clock input), as the leader device and the runtime sent; NetAudio
    // offers preferred leader only with them (`device-settings.md` §12.3).
    c[0x30..0x34].copy_from_slice(&200_000u32.to_be_bytes());
    put16(&mut c, 0x4A, 1);
    put16(&mut c, 0x4C, 0x0060);
    put16(&mut c, 0x4E, 0x0C00);
    c[0x50..0x54].copy_from_slice(&0x0Cu32.to_be_bytes());
    put16(&mut c, 0x54, 0x0078);
    put16(&mut c, 0x56, 0x0040);
    put16(&mut c, 0x58, 1);
    put16(&mut c, 0x5C, 0x0068);
    c[0x5E] = 0x10;
    // Port record: flags, number, PTP version 1, 2, primary interface.
    put16(&mut c, 0x60, 0);
    put16(&mut c, 0x62, 1);
    c[0x64] = 1;
    c[0x65] = 2;
    c[0x66] = 1;
    c[0x68..0x6C].copy_from_slice(&0x0Du32.to_be_bytes());
    put16(&mut c, 0x6C, me.port_state);
    put16(&mut c, 0x6E, 7);
    // Identity block: own, parent, grandmaster; trailer at 0xB0.
    c[0x70..0x78].copy_from_slice(&[0x00, 0x01, 0x00, 0x07, 0x00, 0xB8, 0x00, 0x10]);
    c[0x78..0x80].copy_from_slice(&eui(me.mac));
    c[0x80..0x88].copy_from_slice(&eui(leader));
    c[0x88..0x90].copy_from_slice(&eui(leader));
    c[0xB0..0xB8].copy_from_slice(&[0x00, 0x41, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00]);
    c
}

/// Device properties answered on ARC 0x1100 (`device-settings.md` §§4.2,
/// 4.4, 5; `interop-fixes.md` F8). Latencies in ns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Properties {
    /// Default receive latency (id 0x8204): the configured latency, the
    /// value the device returns to without a stored one (`device-settings.md`
    /// §4.2). Never the engine's latency floor.
    pub default_latency_ns: u32,
    /// Configured and active receive latency (ids 0x8205, 0x8301).
    pub latency_ns: u32,
    /// The smallest and largest latency a write accepts (ids 0x8306,
    /// 0x8302; `device-settings.md` §4.4 item 2).
    pub latency_range_ns: [u32; 2],
    /// A latency write can succeed: the device's configuration does not
    /// lock it (§4.4 item 1).
    pub latency_writable: bool,
}

const PROPERTY_IDS: [u16; 5] = [0x8204, 0x8205, 0x8301, 0x8302, 0x8306];

/// 0x1102: the property directory, (id, access) pairs; access 1 = read
/// only, 3 = writable (the latency setting and its active value while a
/// write can succeed, `device-settings.md` §4.4 item 1).
#[must_use]
pub fn property_directory(p: &Properties) -> Vec<u8> {
    let mut c = Content::new();
    c.u16(PROPERTY_IDS.len() as u16);
    for id in PROPERTY_IDS {
        c.u16(id);
        let writable = p.latency_writable && matches!(id, 0x8205 | 0x8301);
        c.u16(if writable { 3 } else { 1 });
    }
    c.0
}

/// Items of a 0x1101 property write (`device-settings.md` §12.1): each id
/// with its value; ids with the 0x8000 bit carry a packet offset of a u32.
/// An item whose value cannot be read is skipped.
#[must_use]
pub fn property_writes(content: &[u8]) -> Vec<(u16, u32)> {
    let count = usize::from(*content.get(1).unwrap_or(&0));
    (0..count)
        .filter_map(|n| {
            let id = be16(content, 2 + 4 * n)?;
            let raw = be16(content, 4 + 4 * n)?;
            if id & 0x8000 == 0 {
                return Some((id, u32::from(raw)));
            }
            let at = usize::from(raw).checked_sub(HEADER_LEN)?;
            Some((
                id,
                u32::from_be_bytes(content.get(at..at + 4)?.try_into().ok()?),
            ))
        })
        .collect()
}

/// 0x1100: values of the requested ids (all listed ids when the request
/// names none). Ids with the 0x8000 bit carry a packet offset of a u32;
/// an unsupported id is answered as (0, id).
#[must_use]
pub fn property_values(content: &[u8], p: &Properties) -> Vec<u8> {
    let n = usize::from(be16(content, 0).unwrap_or(0));
    let ids: Vec<u16> = (0..n).filter_map(|i| be16(content, 2 + 2 * i)).collect();
    property_list(&ids, p)
}

/// The 0x1100 value list for `ids` (all listed properties when empty); also
/// the reply to a 0x1101 write, built from the state after it.
#[must_use]
pub fn property_list(ids: &[u16], p: &Properties) -> Vec<u8> {
    let ids = if ids.is_empty() {
        &PROPERTY_IDS[..]
    } else {
        ids
    };
    let ids = &ids[..ids.len().min(255)];
    let mut c = Content::new();
    c.0.extend_from_slice(&[ids.len() as u8, ids.len() as u8]);
    let entries = c.0.len();
    c.0.resize(entries + 4 * ids.len(), 0);
    for (n, id) in ids.iter().enumerate() {
        let value = match id {
            0x8204 => Some(p.default_latency_ns),
            0x8205 | 0x8301 => Some(p.latency_ns),
            0x8302 => Some(p.latency_range_ns[1]),
            0x8306 => Some(p.latency_range_ns[0]),
            _ => None,
        };
        let at = entries + 4 * n;
        match value {
            Some(v) => {
                let offset = c.offset();
                c.u32(v);
                c.set16(at, *id);
                c.set16(at + 2, offset);
            }
            None => c.set16(at + 2, *id),
        }
    }
    c.0
}

/// Update modes of 0x0080/0x0082 (`device-settings.md` §§3.1, 3.3 item 3).
/// Tessera never sends 1 (after a reboot): it applies a change at once.
pub mod update_mode {
    /// No value but the current one is accepted now.
    pub const FIXED: u16 = 0;
    /// Another value is accepted and applied at once.
    pub const NOW: u16 = 2;
}

/// The list form of 0x0080/0x0082 (`conmon.md` §§4.6, 9.3;
/// `device-settings.md` §§3.1, 3.3 items 2-3): current value; second word
/// 0, the value after a reboot, none pending; the update mode, 2 when the
/// list holds a value other than the current one, else 0; u16 0; then the
/// values, which are exactly what a write accepts now.
fn value_list(value: u32, list: &[u32]) -> Vec<u8> {
    let mode = if list.iter().any(|v| *v != value) {
        update_mode::NOW
    } else {
        update_mode::FIXED
    };
    let mut c = vec![0x00, 0x18];
    c.extend_from_slice(&(list.len() as u16).to_be_bytes());
    c.extend_from_slice(&value.to_be_bytes());
    c.extend_from_slice(&0u32.to_be_bytes());
    c.extend_from_slice(&mode.to_be_bytes());
    c.extend_from_slice(&[0, 0]);
    for v in list {
        c.extend_from_slice(&v.to_be_bytes());
    }
    c
}

/// Bit 0 of the flags word of a 0x0081/0x0083 request: the request is a
/// write (`device-settings.md` §3.3 item 4).
pub const WRITE_FLAG: u32 = 0x0000_0001;

/// A 0x0081/0x0083 write (`device-settings.md` §§3.3 item 4, 12.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueWrite {
    /// The whole flags word, content u32 +0x00.
    pub flags: u32,
    /// The new value, content u32 +0x04; `None` when the request ends
    /// before it.
    pub value: Option<u32>,
}

/// The write a 0x0081/0x0083 request carries: one whose flags word (content
/// u32 +0x00) has [`WRITE_FLAG`] set, whatever its other bits. `None` for a
/// read. The u32 at +0x04 never decides: the vendor controller's reads
/// carry junk there (§3.3).
#[must_use]
pub fn value_write(request: &[u8]) -> Option<ValueWrite> {
    let content = request.get(32..)?;
    let flags = u32::from_be_bytes(content.get(0..4)?.try_into().ok()?);
    if flags & WRITE_FLAG == 0 {
        return None;
    }
    let value = content
        .get(4..8)
        .and_then(|v| v.try_into().ok())
        .map(u32::from_be_bytes);
    Some(ValueWrite { flags, value })
}

/// The preferred-leader change of a 0x0021 clock write: content +0x00 is the
/// change mask, bit 0x0002 = preferred leader, value byte +0x04
/// (`device-settings.md` §12.3; value position a `guess`). `None` when the
/// request does not change it.
#[must_use]
pub fn preferred_write(request: &[u8]) -> Option<bool> {
    let mask = be16(request, 32)?;
    (mask & 0x0002 != 0).then(|| request.get(32 + 4).is_some_and(|v| *v != 0))
}

/// Mode of an 0x0090 system reset (`device-settings.md` §§6, 12.5): content
/// +0x02, 0 = reboot, 1 = factory reset. `None` if too short.
#[must_use]
pub fn reset_mode(request: &[u8]) -> Option<u16> {
    be16(request, 32 + 2)
}

/// Clear configuration "everything" and "keep the network settings": the
/// modes of a 0x0077 write (`device-settings.md` §12.5a).
pub mod clear {
    pub const ALL: u16 = 1;
    pub const KEEP_NETWORK: u16 = 2;
}

/// The mode of a 0x0077 request (`device-settings.md` §§9, 12.5a): content
/// u16 0, u16 mode. `None` for a read: no content, or mode 0.
#[must_use]
pub fn clear_mode(request: &[u8]) -> Option<u16> {
    be16(request, 32 + 2).filter(|m| *m != 0)
}

/// Clear-configuration status 0x0078 (`conmon.md` §4.5,
/// `device-settings.md` §12.5a): u16 0, u16 3 (modes 1 and 2 supported, a
/// `guess`), then u32 the accepted mode until the role restarts, 0 when no
/// clear is pending. The confirmation form is a `guess`.
#[must_use]
pub fn clear_status(accepted: u16) -> Vec<u8> {
    let mut c = vec![0, 0, 0, 3];
    c.extend_from_slice(&u32::from(accepted).to_be_bytes());
    c
}

/// Whether info 0x0041 is answered with [`interface_statistics`]. Off: NetAudio
/// rejects that layout as malformed (`device network-status`, 2026-10-30), so
/// a request stays unanswered, as before, until a capture gives the byte
/// positions (open spec question in PROGRESS.md). The interface counters
/// still go out in heartbeat section 0x8000.
const INTERFACE_STATISTICS_LAYOUT_KNOWN: bool = false;

/// Interface statistics 0x0040, the answer to 0x0041 (`device-settings.md`
/// §8). The whole layout is a `guess` (the old runtime's, source
/// untraceable), and the spec names its framing words without their
/// positions: they are placed here in the order it gives them (open spec
/// question in PROGRESS.md): u16 group count 1, u16 offsets 0x10, 0x14 and
/// 0x28, u16 capability mask 3, zeros to content 0x10, then the group's one
/// 24-byte record: TX bytes/s, RX bytes/s, TX errors, RX errors (the
/// heartbeat 0x8000 counters), u32 1, link speed in Mb/s. 0x10 and 0x28 then
/// bracket the record and 0x14 is its RX word.
#[must_use]
pub fn interface_statistics(traffic: [u32; 4], link_mbps: u32) -> Vec<u8> {
    let mut c = Vec::with_capacity(0x28);
    for v in [1u16, 0x10, 0x14, 0x28, 3] {
        c.extend_from_slice(&v.to_be_bytes());
    }
    c.resize(0x10, 0);
    for v in traffic.into_iter().chain([1, link_mbps]) {
        c.extend_from_slice(&v.to_be_bytes());
    }
    c
}

/// Content of the main heartbeat datagram (`conmon.md` §6, `metering.md`
/// §2.6 item 6), in this order: the short clock section 0x8001, network
/// statistics 0x8000, then receive-flow latency 0x8003 and late packets
/// 0x8004 ([`heartbeat_latency`]), every section carrying `counter`. With
/// the header, 384 bytes at 32 flow slots, as the old runtime sent it.
#[must_use]
pub fn heartbeat(
    counter: u16,
    ppb: i32,
    traffic: [u32; 4],
    rate: u32,
    slots: &[u32],
    late: &[u32],
) -> Vec<u8> {
    let mut c = Vec::with_capacity(52 + 44 + 8 * slots.len());
    for v in [16u16, 0x8001, 4, 4, counter, 0] {
        c.extend_from_slice(&v.to_be_bytes());
    }
    c.extend_from_slice(&ppb.to_be_bytes());
    for v in [36u16, 0x8000, 4, 4, counter, 0, 0x0010, 0, 0x0001, 0x0010] {
        c.extend_from_slice(&v.to_be_bytes());
    }
    // TX bytes/s, RX bytes/s, TX errors, RX errors (`device-settings.md` §8).
    for v in traffic {
        c.extend_from_slice(&v.to_be_bytes());
    }
    c.extend(heartbeat_latency(counter, rate, slots, late));
    c
}

/// Heartbeat sections 0x8003 (per-receive-flow latency) and 0x8004 (its
/// companion) for [`heartbeat`] (`conmon.md` §§6, 6.1, 9.4): one slot per
/// receive flow slot, the largest lateness in samples since the previous
/// heartbeat; in 0x8004 each slot's cumulative late-packet count
/// (`metering.md` §§2.1-2.2).
#[must_use]
pub fn heartbeat_latency(counter: u16, rate: u32, slots: &[u32], late: &[u32]) -> Vec<u8> {
    let f = slots.len();
    let mut c = Vec::with_capacity(48 + 8 * f);
    let l3 = (24 + 4 * f) as u16;
    for v in [l3, 0x8003, 4, l3 - 12, counter, 0, f as u16, 0, 24, 0] {
        c.extend_from_slice(&v.to_be_bytes());
    }
    c.extend_from_slice(&rate.to_be_bytes());
    for s in slots {
        c.extend_from_slice(&s.to_be_bytes());
    }
    let l4 = (20 + 4 * f) as u16;
    for v in [l4, 0x8004, 4, l4 - 12, counter, 0, f as u16, 0, 20, 0] {
        c.extend_from_slice(&v.to_be_bytes());
    }
    for n in 0..f {
        c.extend_from_slice(&late.get(n).copied().unwrap_or(0).to_be_bytes());
    }
    c
}

/// Heartbeat section 0x8002, per-channel signal presence (`conmon.md` §6,
/// `metering.md` §2.5), the only section of a datagram kind of its own
/// (§2.6 item 7): TX channels then RX channels, one byte each, zero padded
/// to a multiple of 4. The length field counts the padding, so that it is a
/// multiple of 4 like every section length seen from a real device (§2.4);
/// the reference leaves the padding out (`conmon.md` §6).
#[must_use]
pub fn heartbeat_signal(counter: u16, tx: &[u8], rx: &[u8]) -> Vec<u8> {
    let len = (24 + tx.len() + rx.len()).next_multiple_of(4);
    let mut c = Vec::with_capacity(len);
    for v in [
        len as u16,
        0x8002,
        4,
        (len - 12) as u16,
        counter,
        0,
        tx.len() as u16,
        0,
        rx.len() as u16,
        0,
        24,
        0,
    ] {
        c.extend_from_slice(&v.to_be_bytes());
    }
    c.extend_from_slice(tx);
    c.extend_from_slice(rx);
    c.resize(len, 0);
    c
}

/// Signal byte of a peak (absolute left-justified 32-bit sample). The scale is
/// a `guess` (`metering.md` §2.5): 1 + 2 x (-dBFS) rounded, 0 = clipping,
/// 253 = no signal.
#[must_use]
pub fn peak_byte(peak: u64) -> u8 {
    const FULL: f64 = 2_147_483_648.0;
    if peak == 0 {
        return 253;
    }
    if peak as f64 >= FULL {
        return 0;
    }
    let db = 20.0 * (peak as f64 / FULL).log10();
    (1.0 - 2.0 * db).round().clamp(1.0, 253.0) as u8
}

/// Channel-change notification content (`conmon.md` §5): a bit mask of RX
/// channel ids that controllers should read again.
#[must_use]
pub fn channel_change(channels: &[u16]) -> Vec<u8> {
    let highest = channels.iter().copied().max().unwrap_or(0);
    let len = usize::from(highest).div_ceil(8).max(1);
    let mut mask = vec![0u8; len];
    for ch in channels.iter().filter(|c| **c > 0) {
        let i = usize::from(ch - 1);
        mask[i / 8] |= 1 << (i % 8);
    }
    let mut c = (len as u16).to_be_bytes().to_vec();
    c.extend_from_slice(&mask);
    c
}

/// Parameters of one receive-flow request (`control-flows.md` §7.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowRequest<'a> {
    pub start: u16,
    pub sequence: u16,
    pub rate: u32,
    pub bits: u32,
    /// Transmitter channel ids in media order; a repeated id is allowed.
    pub channels: &'a [u16],
    pub fpp: u16,
    pub receiver: &'a str,
    pub flow_name: &'a str,
    pub address: Ipv4Addr,
    pub port: u16,
}

/// Encode a flow request 0x0100 in the observed layout (§7.1).
#[must_use]
pub fn flow_request(r: &FlowRequest<'_>) -> Vec<u8> {
    fn put(p: &mut [u8], at: usize, v: u16) {
        p[at..at + 2].copy_from_slice(&v.to_be_bytes());
    }
    let n = r.channels.len();
    let b = 0x1A + 2 * n;
    let name_at = b + 0x1E;
    let flow_name_at = name_at + r.receiver.len() + 1;
    let strings_end = flow_name_at + r.flow_name.len() + 1;
    let socket_at = strings_end.div_ceil(8) * 8;
    let total = socket_at + 8;
    let mut p = vec![0u8; total];
    put(&mut p, 0, r.start);
    put(&mut p, 2, total as u16);
    put(&mut p, 4, r.sequence);
    put(&mut p, 6, 0x0100);
    put(&mut p, 0x0A, name_at as u16);
    p[0x0C..0x10].copy_from_slice(&r.rate.to_be_bytes());
    p[0x10..0x14].copy_from_slice(&r.bits.to_be_bytes());
    put(&mut p, 0x14, 1);
    put(&mut p, 0x16, n as u16);
    put(&mut p, 0x18, socket_at as u16);
    for (i, id) in r.channels.iter().enumerate() {
        put(&mut p, 0x1A + 2 * i, *id);
    }
    put(&mut p, b, (b + 2) as u16);
    put(&mut p, b + 2, 0x0E00);
    put(&mut p, b + 4, 0x0002);
    put(&mut p, b + 6, r.fpp);
    put(&mut p, b + 8, flow_name_at as u16);
    put(&mut p, b + 0x16, 3);
    p[name_at..name_at + r.receiver.len()].copy_from_slice(r.receiver.as_bytes());
    p[flow_name_at..flow_name_at + r.flow_name.len()].copy_from_slice(r.flow_name.as_bytes());
    put(&mut p, socket_at, 0x0802);
    put(&mut p, socket_at + 2, r.port);
    p[socket_at + 4..socket_at + 8].copy_from_slice(&r.address.octets());
    p
}

/// Encode a stop-flow request 0x0101 quoting the 6-byte handle (§3).
#[must_use]
pub fn flow_stop(start: u16, sequence: u16, handle: &[u8]) -> Vec<u8> {
    let handle = &handle[..handle.len().min(6)];
    reply(
        Header {
            start,
            sequence,
            opcode: 0x0101,
        },
        0,
        handle,
    )
}

/// Outcome of a flow-control reply matched to (sequence, opcode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowReply {
    /// Success, with the handle bytes as received.
    Accepted(Vec<u8>),
    /// Refused with this code (§4).
    Refused(u16),
}

/// Match a flow-control reply to the request it answers (§1 pairing).
#[must_use]
pub fn flow_reply(bytes: &[u8], sequence: u16, opcode: u16) -> Option<FlowReply> {
    if bytes.len() < HEADER_LEN
        || usize::from(be16(bytes, 2)?) != bytes.len()
        || be16(bytes, 4)? != sequence
        || be16(bytes, 6)? != opcode
    {
        return None;
    }
    Some(match be16(bytes, 8)? {
        0 => return None,
        1 => FlowReply::Accepted(bytes[HEADER_LEN..].to_vec()),
        code => FlowReply::Refused(code),
    })
}

/// The first `fpp` value of a channel record (`control-flows.md` §7.1).
#[must_use]
pub fn advertised_fpp(value: &str) -> Option<u16> {
    value
        .split(',')
        .next()?
        .trim()
        .parse()
        .ok()
        .filter(|v| *v > 0)
}

/// A `0x1102`-style hex TXT value (`dbcp1`).
#[must_use]
pub fn hex16(value: &str) -> Option<u16> {
    u16::from_str_radix(value.trim().trim_start_matches("0x"), 16).ok()
}

/// A flow request as a transmitter receives it (`control-flows.md` §§1, 7.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedFlowRequest {
    pub rate: u32,
    pub bits: u32,
    /// Transmitter channel ids in media order (0 = empty position).
    pub channels: Vec<u16>,
    pub fpp: u16,
    pub receiver: String,
    pub flow_name: String,
    pub address: Ipv4Addr,
    pub port: u16,
}

/// The NUL-terminated string at packet offset `offset` of a whole packet.
fn packet_string(packet: &[u8], offset: u16) -> Option<String> {
    let tail = packet.get(usize::from(offset)..)?;
    let end = tail.iter().position(|b| *b == 0)?;
    std::str::from_utf8(&tail[..end]).ok().map(str::to_owned)
}

/// Decode a 0x0100 request packet. Offsets are packet offsets (§7.1).
#[must_use]
pub fn received_flow_request(packet: &[u8]) -> Option<ReceivedFlowRequest> {
    let n = usize::from(be16(packet, 0x16)?);
    let b = 0x1A + 2 * n;
    let socket = usize::from(be16(packet, 0x18)?);
    let channels = (0..n)
        .map(|i| be16(packet, 0x1A + 2 * i))
        .collect::<Option<Vec<u16>>>()?;
    Some(ReceivedFlowRequest {
        rate: u32::from_be_bytes(packet.get(0x0C..0x10)?.try_into().ok()?),
        bits: u32::from_be_bytes(packet.get(0x10..0x14)?.try_into().ok()?),
        channels,
        fpp: be16(packet, b + 6)?,
        receiver: packet_string(packet, be16(packet, 0x0A)?).unwrap_or_default(),
        flow_name: packet_string(packet, be16(packet, b + 8)?).unwrap_or_default(),
        port: be16(packet, socket + 2)?,
        address: Ipv4Addr::from(<[u8; 4]>::try_from(packet.get(socket + 4..socket + 8)?).ok()?),
    })
}

/// Bytes of the handle a receiver quotes in 0x0101 and 0x0102: the first
/// six content bytes of the success reply (`control-flows.md` §§1-3).
pub const FLOW_HANDLE_LEN: usize = 6;

/// The handle of transmit flow `id`: the u16 flow id and a u32 that differs
/// per flow instance (`control-flows.md` §7.2, content +0x00 and +0x02).
#[must_use]
pub fn flow_handle(id: u16, instance: u32) -> [u8; FLOW_HANDLE_LEN] {
    let mut h = [0; FLOW_HANDLE_LEN];
    h[..2].copy_from_slice(&id.to_be_bytes());
    h[2..].copy_from_slice(&instance.to_be_bytes());
    h
}

/// Content of a transmitter's success reply to 0x0100: 14 bytes, the form
/// both real transmitters sent (24-byte datagrams; `control-flows.md` §7.2,
/// vectors V4 and V6; `interop-fixes.md` F1). The handle, then u16 1, u16 1
/// and four zero bytes, whose meanings are UNKNOWN.
#[must_use]
pub fn flow_accepted(handle: [u8; FLOW_HANDLE_LEN]) -> Vec<u8> {
    let mut c = handle.to_vec();
    c.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 0]);
    c
}

/// A transmit flow in an ARC 0x2200 reply (`control-arc.md` §4.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxFlowView {
    pub id: u16,
    /// A manual multicast flow: no receiver strings, flow type 0x0002.
    pub multicast: bool,
    pub local_name: String,
    pub receiver: String,
    pub receiver_flow: String,
    pub address: Ipv4Addr,
    pub port: u16,
    pub channels: Vec<u16>,
}

/// 0x2200: transmit flows (§4.9), in id order from the one-based item
/// `first`, at most 16 per page and paged like the channel lists (§6).
/// Returns the content and whether more pages follow.
#[must_use]
pub fn tx_flows(view: &DeviceView, flows: &[TxFlowView], first: usize) -> (Vec<u8>, bool) {
    let page: Vec<&TxFlowView> = flows.iter().skip(first - 1).take(FLOW_SLOTS).collect();
    let mut c = Content::new();
    c.0.extend_from_slice(&[page.len() as u8, page.len() as u8]);
    c.0.resize(2 + page.len() * 2, 0);
    let mut count = 0;
    for (n, flow) in page.iter().enumerate() {
        let local = c.string(&flow.local_name);
        let (host, remote) = if flow.multicast {
            (0, 0)
        } else {
            (c.string(&flow.receiver), c.string(&flow.receiver_flow))
        };
        c.pad(4);
        let socket = c.offset();
        c.u16(0x8002);
        c.u16(flow.port);
        c.0.extend_from_slice(&flow.address.octets());
        let names = c.offset();
        for v in [0x0A00, 1, host, remote, 0x0010, local] {
            c.u16(v);
        }
        // 8 zero bytes; in a multicast flow the first 4 are `latency_ns`.
        let latency = if flow.multicast { view.latency_ns } else { 0 };
        c.u32(latency);
        c.u32(0);
        let main = c.offset();
        c.u16(flow.id);
        c.u16(if flow.multicast { 0x0002 } else { 0x0011 });
        c.u32(view.rate);
        c.u16(0);
        c.u16(view.bits);
        c.u16(1);
        c.u16(flow.channels.len() as u16);
        c.u16(socket);
        for id in &flow.channels {
            c.u16(*id);
        }
        c.u16(names);
        c.set16(2 + n * 2, main);
        count += 1;
        if c.0.len() > SOFT_LIMIT {
            break;
        }
    }
    c.0[1] = count as u8;
    let more = first - 1 + count < flows.len();
    (c.0, more)
}
