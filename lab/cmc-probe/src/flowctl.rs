//! Real flow control (`spec/protocol/control-flows.md`): a server on the flow-control port for
//! receivers that ask this device for a unicast flow, and a client for real transmitters.
//! Scratch code, like the rest of the probe.

use super::*;
use std::collections::HashMap;

/// Start code of every flow-control message (`control-flows.md`: 0x1102 in all reference messages
/// and in the request a hardware receiver sent to this device).
const FC_START: u16 = 0x1102;
/// The 8 bytes between the parameter block and the strings in a hardware receiver's request.
const FC_TAIL: [u8; 8] = [0, 3, 0, 0, 0, 0, 0, 0];

/// A transmitter's `_netaudio-chan` service data.
pub struct Chan {
    pub ip: Ipv4Addr,
    pub port: u16,
    pub start: u16,
    /// Channel name -> channel id (the `id=` TXT key of the instance `name@host`).
    pub ids: HashMap<String, u16>,
    pub fpp: u16,
}

/// A planned flow to one transmitter, kept between plans.
#[derive(Clone)]
pub struct RealPlan {
    pub names: Vec<String>,
    pub at: std::time::Instant,
    pub ip: Ipv4Addr,
    pub port: u16,
    pub result: Result<[u8; 6], u16>,
    pub ids: Vec<u16>,
    /// Packets the daemon had received on the flow at the last check.
    pub packets: u64,
    /// Consecutive requests that did not make the flow receive (drives the back-off).
    pub fails: u32,
    /// Not before this time is the flow asked for again after a stall.
    pub next_try: std::time::Instant,
    /// The local receive port this plan asked for, and how many times it was rotated (a transmitter
    /// keeps its old flow to a port for several seconds after our daemon vanished and then refuses a
    /// new flow to the same port, so every re-request after a stall asks for the next port).
    pub rx_port: u16,
    pub rot: u16,
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(u8::is_ascii_digit) {
            let v: u16 = std::str::from_utf8(&b[i + 1..i + 4])
                .unwrap()
                .parse()
                .unwrap_or(63);
            out.push(v as u8);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn txt_value<'a>(txt: &'a [String], key: &str) -> Option<&'a str> {
    txt.iter()
        .find_map(|t| t.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

/// Looks the transmitter up in mDNS (`avahi-browse`): its address, its flow-control port, the
/// start code (TXT `dbcp1`), channel name -> id (TXT `id` of the instance `name@host`).
pub fn chan_lookup(host: &str) -> Option<Chan> {
    let out = std::process::Command::new("timeout")
        .args(["-k", "2", "5", "avahi-browse", "-rtp", "_netaudio-chan._udp"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let want = format!("{host}.local");
    let mut found: Option<Chan> = None;
    for line in text.lines().filter(|l| l.starts_with("=;")) {
        let f: Vec<&str> = line.splitn(10, ';').collect();
        if f.len() < 10 || f[2] != "IPv4" || unescape(f[6]) != want {
            continue;
        }
        let (Ok(ip), Ok(port)) = (f[7].parse::<Ipv4Addr>(), f[8].parse::<u16>()) else {
            continue;
        };
        let txt: Vec<String> = f[9]
            .split("\" \"")
            .map(|t| t.trim_matches('"').to_owned())
            .collect();
        let instance = unescape(f[3]);
        let name = instance.split('@').next().unwrap_or("").to_owned();
        let id = txt_value(&txt, "id")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let c = found.get_or_insert_with(|| Chan {
            ip,
            port,
            start: FC_START,
            ids: HashMap::new(),
            fpp: txt_value(&txt, "fpp")
                .and_then(|v| v.split(',').next())
                .and_then(|v| v.parse().ok())
                .unwrap_or(16),
        });
        if id != 0 {
            c.ids.insert(name, id);
        }
    }
    found
}

fn fc_header(start: u16, seq: u16, op1: u16, op2: u16, content: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(10 + content.len());
    m.extend_from_slice(&start.to_be_bytes());
    m.extend_from_slice(&((10 + content.len()) as u16).to_be_bytes());
    m.extend_from_slice(&seq.to_be_bytes());
    m.extend_from_slice(&op1.to_be_bytes());
    m.extend_from_slice(&op2.to_be_bytes());
    m.extend_from_slice(content);
    m
}

/// The request for a flow of `ids` to this device, in the layout a hardware receiver used.
fn build_request(
    cfg: &Cfg,
    ids: &[u16],
    rate: u32,
    bits: u32,
    fpp: u16,
    rx_port: u16,
    seq: u16,
) -> Vec<u8> {
    let n = ids.len();
    let mut p = vec![0u8; 10];
    let put16v = |p: &mut Vec<u8>, v: u16| p.extend_from_slice(&v.to_be_bytes());
    let host_off = 56 + 2 * n;
    let flow_name = "1";
    let name_off = host_off + cfg.name.len() + 1;
    let mut desc_off = name_off + flow_name.len() + 1;
    desc_off = desc_off.div_ceil(8) * 8;
    put16v(&mut p, host_off as u16);
    p.extend_from_slice(&rate.to_be_bytes());
    p.extend_from_slice(&bits.to_be_bytes());
    put16v(&mut p, 1);
    put16v(&mut p, n as u16);
    put16v(&mut p, desc_off as u16);
    for id in ids {
        put16v(&mut p, *id);
    }
    put16v(&mut p, (28 + 2 * n) as u16);
    put16v(&mut p, 0x0e00);
    put16v(&mut p, 2);
    put16v(&mut p, fpp);
    put16v(&mut p, name_off as u16);
    p.extend_from_slice(&[0u8; 12]);
    p.extend_from_slice(&FC_TAIL);
    p.extend_from_slice(cfg.name.as_bytes());
    p.push(0);
    p.extend_from_slice(flow_name.as_bytes());
    p.push(0);
    while p.len() < desc_off {
        p.push(0);
    }
    put16v(&mut p, 0x0802);
    put16v(&mut p, rx_port);
    p.extend_from_slice(&cfg.ip.octets());
    let h = fc_header(FC_START, seq, 0x0100, 0, &[]);
    p[..10].copy_from_slice(&h);
    let len = p.len() as u16;
    p[2..4].copy_from_slice(&len.to_be_bytes());
    p
}

/// Sends a flow-control request and waits for the matching reply. Ok(content of a success
/// reply) or Err(opcode2); Err(0xFFFF) = no answer.
fn exchange(
    cfg: &Cfg,
    to: Ipv4Addr,
    port: u16,
    msg: &[u8],
    seq: u16,
    op1: u16,
) -> Result<Vec<u8>, u16> {
    let sock = UdpSocket::bind((cfg.ip, 0)).map_err(|_| 0xFFFFu16)?;
    sock.set_read_timeout(Some(Duration::from_millis(1500)))
        .map_err(|_| 0xFFFFu16)?;
    let mut buf = [0u8; 1500];
    for attempt in 0..2 {
        sock.send_to(msg, (to, port)).map_err(|_| 0xFFFFu16)?;
        println!(
            "{} flowctl TX {to}:{port} attempt {attempt} {}B {}",
            now(),
            msg.len(),
            hex(msg)
        );
        while let Ok((n, _)) = sock.recv_from(&mut buf) {
            println!("{} flowctl RX {to}:{port} {n}B {}", now(), hex(&buf[..n]));
            if n >= 10 && be16(&buf, 4) == seq && be16(&buf, 6) == op1 {
                let op2 = be16(&buf, 8);
                return if op2 == 1 {
                    Ok(buf[10..n].to_vec())
                } else {
                    Err(op2)
                };
            }
        }
    }
    Err(0xFFFF)
}

/// Asks the real transmitter for a flow of `ids` to this device (`rx_port` here).
pub fn request_real(cfg: &Cfg, ch: &Chan, ids: &[u16], rx_port: u16) -> Result<[u8; 6], u16> {
    let (rate, bits) = rate_enc(cfg);
    let fpp = ch
        .fpp
        .min((1400 / (ids.len().max(1) * (bits as usize / 8).max(1))) as u16)
        .max(2);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let msg = build_request(cfg, ids, rate, u32::from(bits), fpp, rx_port, seq);
    let c = exchange(cfg, ch.ip, ch.port, &msg, seq, 0x0100)?;
    let mut h = [0u8; 6];
    for (d, s) in h.iter_mut().zip(c.iter()) {
        *d = *s;
    }
    Ok(h)
}

/// Stops a flow that was created with `request_real`.
pub fn stop_real(cfg: &Cfg, ip: Ipv4Addr, port: u16, handle: [u8; 6]) {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let msg = fc_header(FC_START, seq, 0x0101, 0, &handle);
    let r = exchange(cfg, ip, port, &msg, seq, 0x0101);
    println!(
        "{} flowctl stop {ip}: {}",
        now(),
        if r.is_ok() {
            "ok".to_owned()
        } else {
            format!("{r:?}")
        }
    );
}

/// The server: answers flow requests (0x0100), updates (0x0102) and stops (0x0101).
pub fn serve_request(req: &[u8], from: SocketAddr, cfg: &Cfg) -> Option<Vec<u8>> {
    if req.len() < 10 || be16(req, 8) != 0 {
        return None;
    }
    let (start, seq, op1) = (be16(req, 0), be16(req, 4), be16(req, 6));
    let fail = |code: u16| Some(fc_header(start, seq, op1, code, &[]));
    let mut st = cfg.st.lock().unwrap();
    match op1 {
        0x0100 => {
            if req.len() < 28 {
                return fail(0x0302);
            }
            let rate = u32::from_be_bytes([req[12], req[13], req[14], req[15]]);
            let n = usize::from(be16(req, 22));
            let desc_off = usize::from(be16(req, 24));
            let (Some(ids_raw), Some(desc)) =
                (req.get(26..26 + 2 * n), req.get(desc_off..desc_off + 8))
            else {
                return fail(0x0302);
            };
            let ids: Vec<u16> = ids_raw
                .chunks_exact(2)
                .map(|p| u16::from_be_bytes([p[0], p[1]]))
                .collect();
            let fpp = req
                .get(32 + 2 * n..34 + 2 * n)
                .map_or(0, |b| u16::from_be_bytes([b[0], b[1]]));
            let rx_port = be16(desc, 2);
            let rx_ip = Ipv4Addr::new(desc[4], desc[5], desc[6], desc[7]);
            let host = cstr_at(req, usize::from(be16(req, 10))).unwrap_or_default();
            let (my_rate, _) = rate_enc(cfg);
            println!(
                "{} flowctl REQUEST from {from} ({host}): {n} ch {ids:?} rate {rate} -> {rx_ip}:{rx_port}",
                now()
            );
            if rate != my_rate {
                println!("{} flowctl refused: rate {rate} vs {my_rate}", now());
                return fail(0x0301);
            }
            if ids.iter().any(|i| usize::from(*i) > st.tx.len()) {
                return fail(0x0302);
            }
            let id = format!("probe-tx-{}-{rx_port}", rx_ip.to_string().replace('.', "_"));
            let channels: Vec<u16> = ids.iter().copied().filter(|i| *i != 0).collect();
            st.next_slot += 1;
            let slot = st.next_slot;
            let mut handle = [0u8; 6];
            handle[..4].copy_from_slice(&slot.to_be_bytes());
            handle[4..].copy_from_slice(&(rx_port ^ 0x5a5a).to_be_bytes());
            // A receiver that asks again for the same channels (it lost the flow, for instance when this device
            // restarted) gets a new flow; the old one to the same address is stale and would keep sending.
            let stale: Vec<String> = st
                .tx_flows
                .iter()
                .filter(|f| f.id != id && f.peer == rx_ip && f.channels == channels)
                .map(|f| f.id.clone())
                .collect();
            if !stale.is_empty() {
                cfg.flows_dirty.store(true, Ordering::Relaxed);
            }
            for old in &stale {
                println!(
                    "{} flowctl replaces {old} (same receiver and channels)",
                    now()
                );
                st.tx_handles.retain(|_, v| v != old);
                st.tx_real.remove(old);
                st.tx_seen.remove(old);
            }
            st.tx_flows.retain(|f| !stale.contains(&f.id));
            st.tx_handles.retain(|_, v| *v != id);
            st.tx_handles.insert(handle, id.clone());
            let before = st.tx_flows.clone();
            st.tx_flows.retain(|f| f.id != id);
            st.tx_flows.push(AudioFlow {
                id: id.clone(),
                tx: true,
                peer_name: host,
                channels,
                peer: rx_ip,
                port: rx_port,
                fpp,
            });
            st.tx_flows.sort_by(|a, b| a.id.cmp(&b.id));
            st.tx_real.insert(id);
            if st.tx_flows != before {
                cfg.flows_dirty.store(true, Ordering::Relaxed);
            }
            Some(fc_header(start, seq, op1, 1, &handle))
        }
        0x0102 => {
            let Some(h) = req.get(10..16).and_then(|h| <[u8; 6]>::try_from(h).ok()) else {
                return fail(0x0103);
            };
            let Some(id) = st.tx_handles.get(&h).cloned() else {
                return fail(0x0103);
            };
            let Some(count) = req.get(16..18) else {
                return fail(0x0103);
            };
            let n = usize::from(u16::from_be_bytes([count[0], count[1]]));
            let ids: Vec<u16> = req
                .get(18..18 + 2 * n)
                .unwrap_or(&[])
                .chunks_exact(2)
                .map(|p| u16::from_be_bytes([p[0], p[1]]))
                .collect();
            let channels: Vec<u16> = ids
                .iter()
                .copied()
                .filter(|i| *i != 0 && usize::from(*i) <= st.tx.len())
                .collect();
            println!("{} flowctl UPDATE {id}: {ids:?}", now());
            for f in st.tx_flows.iter_mut().filter(|f| f.id == id) {
                f.channels = channels.clone();
            }
            cfg.flows_dirty.store(true, Ordering::Relaxed);
            Some(fc_header(start, seq, op1, 1, &[]))
        }
        0x0101 => {
            let Some(h) = req.get(10..16).and_then(|h| <[u8; 6]>::try_from(h).ok()) else {
                return fail(0x0103);
            };
            let Some(id) = st.tx_handles.remove(&h) else {
                return fail(0x0103);
            };
            println!("{} flowctl STOP {id}", now());
            st.tx_flows.retain(|f| f.id != id);
            st.tx_real.remove(&id);
            cfg.flows_dirty.store(true, Ordering::Relaxed);
            Some(fc_header(start, seq, op1, 1, &[]))
        }
        _ => {
            println!("{} flowctl unknown opcode {op1:04x}", now());
            fail(0xFFFF)
        }
    }
}

/// Packets the daemon has received on the RX flow `id` so far (`tesseractl --json metrics`).
pub fn rx_packets(id: &str) -> Option<u64> {
    let o = std::process::Command::new("timeout")
        .args(["-k", "2", "5", "tesseractl", "--json", "metrics"])
        .output()
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).ok()?;
    v["flows"]
        .as_array()?
        .iter()
        .find(|f| f["id"] == id && f["direction"] == "rx")?["packets"]
        .as_u64()
}
