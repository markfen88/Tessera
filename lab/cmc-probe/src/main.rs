//! Scratch protocol probe (not product code): listens on the control ports a
//! controller talks to, logs every datagram in full, and answers what the
//! drafts in `spec/protocol/` describe. Used to learn, one round at a time,
//! what Dante Controller asks next.
//!
//! cmc-probe --ip 192.0.2.10 --device-id 0011223344556677 --mac 001122334455 --name my-device

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod flowctl;

struct Flow {
    id: u16,
    channels: Vec<u16>,
}

struct State {
    tx: Vec<String>,
    rx: Vec<String>,
    flows: Vec<Flow>,
    /// Per local RX channel: the (TX channel name, TX device name) it is subscribed to.
    subs: Vec<Option<(String, String)>>,
    /// Properties written by the controller with 0x1101 (id, value).
    written: Vec<(u16, u32)>,
    /// Per local RX channel: the subscription status code shown in the RX list.
    sub_codes: Vec<u16>,
    /// TX devices this receiver asked for a flow (host, address), to release later.
    peers: Vec<(String, Ipv4Addr)>,
    /// Audio flows written to the daemon configuration.
    rx_flows: Vec<AudioFlow>,
    tx_flows: Vec<AudioFlow>,
    /// When each TX flow was last requested (receivers refresh every 10 s).
    tx_seen: std::collections::HashMap<String, std::time::Instant>,
    /// Handles given to real receivers, to the flow they name.
    tx_handles: std::collections::HashMap<[u8; 6], String>,
    /// TX flows created by real flow requests (they do not expire like the probe-to-probe ones).
    tx_real: std::collections::HashSet<String>,
    /// Last flow slot number handed out (handles never repeat).
    next_slot: u32,
    /// Real flow plans per transmitter host.
    real_plans: std::collections::HashMap<String, flowctl::RealPlan>,
}

/// A daemon `[[flow]]` entry (`docs/configuration.md`, static flows).
#[derive(Clone, PartialEq)]
struct AudioFlow {
    id: String,
    tx: bool,
    /// The device at the other end (receiver of a TX flow, sender of an RX flow).
    peer_name: String,
    /// Frames per packet requested for a TX flow (0 = the daemon's default).
    fpp: u16,
    channels: Vec<u16>,
    peer: Ipv4Addr,
    port: u16,
}

/// The daemon's clock as last read from `tesseractl --json status`.
#[derive(Clone, Copy)]
struct Live {
    ok: bool,
    state_word: u16,
    gm: [u8; 8],
    ppb: i32,
    priority: u8,
    rate: u32,
    encoding: u32,
    latency_us: u32,
}

/// Traffic and flow counters for the heartbeat, refreshed once a second.
#[derive(Clone, Default)]
struct Stats {
    tx_bps: u32,
    rx_bps: u32,
    tx_errors: u32,
    rx_errors: u32,
    /// (flow id, late packets, measured latency in samples over the last poll) per RX flow.
    rx_flows: Vec<(String, u32, u32)>,
}

struct Cfg {
    ip: Ipv4Addr,
    device_id: [u8; 8],
    mac: [u8; 6],
    info_port: u16,
    netmask: Ipv4Addr,
    gateway: Ipv4Addr,
    vendor: [u8; 8],
    name: String,
    /// Board-info feature flags, content bytes 0x14..0x18 (`conmon.md` 4.1).
    flags: [u8; 4],
    st: std::sync::Mutex<State>,
    /// Subscription status code shown in the RX list (`--sub-status`, hex).
    sub_status: u32,
    /// `--cap-value N`: answer the capability tables (0x1100, 0x1102) with N for every
    /// property instead of blocks of zeros (0 = zeros, the reference behaviour).
    cap_value: u16,
    /// `--gm HEX16`: the clock master reported in clock stats (default: this device).
    gm: [u8; 8],
    /// `--latency-ns N`: put the device latency (nanoseconds, u32) in the answer to the
    /// monitoring request 0x1008 (the 4 zero bytes the spec records for id 0x1007).
    latency_ns: u32,
    /// `--lat-ids 0310,0311`: property ids (hex) whose value in the 0x1100 answer is a pointer
    /// to the device latency (ns, u32) instead of the plain `--cap-value`.
    lat_ids: Vec<u16>,
    /// `--real-props`: answer 0x1100/0x1102 in the structure a real device used
    /// (`control-arc.md` 0.5); `--props 8301=10000000,...` overrides single values.
    real_props: bool,
    props: Vec<(u16, u32)>,
    /// `--clock-state P,S`: the two u16 words at the start of clock stats, read as the
    /// IEEE 1588 port state of the primary and secondary interface (3 = DISABLED,
    /// 6 = MASTER, 9 = SLAVE); the reference sends 3,3.
    clock_state: [u16; 2],
    /// Live clock state from the daemon (falls back to the static values above).
    live: std::sync::Mutex<Live>,
    /// `--gm-form mac`: clock ids as MAC + 00 00 instead of EUI-64 (MAC with FFFE).
    gm_mac_form: bool,
    /// Subscriptions changed: re-plan the RX flows.
    rx_dirty: AtomicBool,
    /// Flows changed: rewrite the daemon configuration and reload.
    flows_dirty: AtomicBool,
    stats: std::sync::Mutex<Stats>,
    /// Metering leases: (owner id, destination, expiry).
    meter: std::sync::Mutex<Vec<([u8; 10], SocketAddr, std::time::Instant)>>,
    /// Latest RX peak levels (metering bytes, 253 = floor).
    rx_levels: std::sync::Mutex<Vec<u8>>,
}

/// One counter for every message sent from the info socket (info and heartbeat).
static SEQ: AtomicU16 = AtomicU16::new(1);

const INFO_GROUP: (Ipv4Addr, u16) = (Ipv4Addr::new(224, 0, 0, 231), 8702);
const HB_GROUP: (Ipv4Addr, u16) = (Ipv4Addr::new(224, 0, 0, 233), 8708);

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join("")
}

fn now() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    format!("{}.{:03}", d.as_secs(), d.subsec_millis())
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

/// The 10-byte header of `spec/protocol/control-arc.md` section 1.
fn describe(b: &[u8]) -> String {
    if b.len() >= 32 && (be16(b, 0) == 0xffff || be16(b, 0) == 0xfffe) {
        // The 32-byte multicast/info header of `conmon.md` section 2.
        return format!(
            "CONMON start={:04x} len={} seq={:04x} pid={:04x} dev={} vendor={:?} opcode={} content={}",
            be16(b, 0),
            be16(b, 2),
            be16(b, 4),
            be16(b, 6),
            hex(&b[8..16]),
            String::from_utf8_lossy(&b[16..24]),
            hex(&b[24..32]),
            hex(&b[32..])
        );
    }
    if b.len() < 10 {
        return format!("short ({} bytes)", b.len());
    }
    format!(
        "start={:04x} len={} seq={:04x} op1={:04x} op2={:04x} content={}",
        be16(b, 0),
        be16(b, 2),
        be16(b, 4),
        be16(b, 6),
        be16(b, 8),
        hex(&b[10..])
    )
}

fn header(req: &[u8], op2: u16, content: &[u8]) -> Vec<u8> {
    let total = 10 + content.len();
    let mut r = Vec::with_capacity(total);
    r.extend_from_slice(&req[0..2]); // start code echoed
    r.extend_from_slice(&(total as u16).to_be_bytes());
    r.extend_from_slice(&req[4..6]); // sequence echoed
    r.extend_from_slice(&req[6..8]); // opcode1 echoed
    r.extend_from_slice(&op2.to_be_bytes());
    r.extend_from_slice(content);
    r
}

/// Answers on the CMC port (`spec/protocol/control-cmc.md`).
fn cmc_reply(req: &[u8], cfg: &Cfg) -> Option<Vec<u8>> {
    if req.len() < 10 || be16(req, 8) != 0 {
        return None;
    }
    match be16(req, 6) {
        0x1001 => {
            let mut c = Vec::with_capacity(22);
            c.extend_from_slice(&0u16.to_be_bytes()); // process id
            c.extend_from_slice(&cfg.device_id); // factory device id
            c.extend_from_slice(&1u16.to_be_bytes()); // unknown, reference: 1
            c.extend_from_slice(&0u16.to_be_bytes()); // unknown, reference: 0
            c.extend_from_slice(&cfg.ip.octets());
            c.extend_from_slice(&cfg.info_port.to_be_bytes());
            c.extend_from_slice(&0u16.to_be_bytes()); // unknown, reference: 0
            Some(header(req, 1, &c))
        }
        // Metering subscription (`controller-netaudio.md` 8): pointers from offset 20.
        0x3010 => {
            const HEAP: usize = 20;
            let at = |p: usize| be16(req, p) as usize + HEAP;
            if req.len() < 30 {
                return None;
            }
            let (client, slots_n, list) = (at(22), be16(req, 24) as usize, at(26));
            if list + 4 > req.len() || be16(req, list) != 1 {
                return None;
            }
            let record = be16(req, list + 2) as usize + HEAP;
            let slots = record + 6;
            if slots_n == 0 || slots + slots_n * 8 > req.len() || client + 2 > req.len() {
                return None;
            }
            let ip = Ipv4Addr::new(req[slots + 2], req[slots + 3], req[slots + 4], req[slots + 5]);
            let port = be16(req, slots + 6);
            let mut owner = [0u8; 10];
            owner.copy_from_slice(&req[10..20]);
            {
                let mut m = cfg.meter.lock().unwrap();
                m.retain(|(o, _, _)| *o != owner);
                if !ip.is_unspecified() && port != 0 {
                    let until = std::time::Instant::now() + Duration::from_secs(10);
                    m.push((owner, SocketAddr::from((ip, port)), until));
                }
                println!("{} cmc METERING {} ({} lease(s))", now(), if port == 0 { "stop".to_owned() } else { format!("to {ip}:{port}") }, m.len());
            }
            let mut r = req.to_vec();
            r[8..10].copy_from_slice(&1u16.to_be_bytes());
            r[10..12].copy_from_slice(&0u16.to_be_bytes());
            r[12..20].copy_from_slice(&cfg.device_id);
            r[client..client + 2].fill(0);
            for s in 1..slots_n {
                let p = slots + s * 8 + 6;
                r[p..p + 2].fill(0);
            }
            Some(r)
        }
        _ => None,
    }
}

/// Sends metering frames every 100 ms while a lease is live; RX levels come from a capture
/// of the tessera ALSA device (where its plugin is installed), TX levels are not metered.
fn meter_worker(cfg: Arc<Cfg>) {
    let sock = bind_retry("metering", SocketAddr::from((cfg.ip, 0)));
    let mut capture: Option<std::process::Child> = None;
    let mut seq: u16 = 0;
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let dests: Vec<SocketAddr> = {
            let mut m = cfg.meter.lock().unwrap();
            let t = std::time::Instant::now();
            m.retain(|(_, _, until)| *until > t);
            m.iter().map(|(_, d, _)| *d).collect()
        };
        if dests.is_empty() {
            if let Some(mut ch) = capture.take() {
                let _ = ch.kill();
                let _ = ch.wait();
                *cfg.rx_levels.lock().unwrap() = vec![253; RX_CHANNELS as usize];
            }
            continue;
        }
        if capture.is_none() {
            capture = start_level_capture(&cfg);
        }
        let mut f = vec![0u8, 0];
        f.extend_from_slice(&cfg.device_id);
        f.extend_from_slice(b"Audinate");
        f.extend_from_slice(&[2, TX_CHANNELS as u8, RX_CHANNELS as u8]);
        f.extend(std::iter::repeat_n(253u8, TX_CHANNELS as usize));
        f.extend_from_slice(&cfg.rx_levels.lock().unwrap());
        f.push(0);
        let mut m = vec![0xFF, 0xFF];
        m.extend_from_slice(&((6 + f.len()) as u16).to_be_bytes());
        m.extend_from_slice(&seq.to_be_bytes());
        m.extend_from_slice(&f);
        seq = seq.wrapping_add(1);
        for d in dests {
            let _ = sock.send_to(&m, d);
        }
    }
}

/// `arecord` from the tessera ALSA device; a reader thread turns each 100 ms block into peak
/// levels (metering byte = 1 + 2 * -dBFS, 253 = floor, 0 = clip).
fn start_level_capture(cfg: &Arc<Cfg>) -> Option<std::process::Child> {
    let ch = RX_CHANNELS as usize;
    let mut child = std::process::Command::new("arecord")
        .args(["-q", "-D", "tessera", "-f", "S32_LE", "-r", "48000", "-c", &ch.to_string(), "-t", "raw"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let c = Arc::clone(cfg);
    std::thread::spawn(move || {
        use std::io::Read;
        let frames = 4800;
        let mut buf = vec![0u8; frames * ch * 4];
        while out.read_exact(&mut buf).is_ok() {
            let mut peak = vec![0u32; ch];
            for (i, s) in buf.chunks_exact(4).enumerate() {
                let v = i32::from_le_bytes([s[0], s[1], s[2], s[3]]).unsigned_abs();
                peak[i % ch] = peak[i % ch].max(v);
            }
            let levels: Vec<u8> = peak
                .iter()
                .map(|p| {
                    if *p == 0 {
                        return 253;
                    }
                    let db = 20.0 * (f64::from(*p) / 2_147_483_648.0).log10();
                    (1.0 - 2.0 * db).round().clamp(0.0, 253.0) as u8
                })
                .collect();
            *c.rx_levels.lock().unwrap() = levels;
        }
    });
    println!("{} metering: RX level capture started", now());
    Some(child)
}

/// The 32-byte info/heartbeat header of `conmon.md` section 2 plus content.
fn conmon(cfg: &Cfg, start: u16, opcode: [u8; 8], content: &[u8]) -> Vec<u8> {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut m = Vec::with_capacity(32 + content.len());
    m.extend_from_slice(&start.to_be_bytes());
    m.extend_from_slice(&((32 + content.len()) as u16).to_be_bytes());
    m.extend_from_slice(&seq.to_be_bytes());
    m.extend_from_slice(&0u16.to_be_bytes()); // process id
    m.extend_from_slice(&cfg.device_id);
    m.extend_from_slice(&cfg.vendor); // vendor string: 8 bytes, padded with spaces
    m.extend_from_slice(&opcode);
    m.extend_from_slice(content);
    m
}

fn put(buf: &mut [u8], at: usize, bytes: &[u8]) {
    buf[at..at + bytes.len()].copy_from_slice(bytes);
}

/// `conmon.md` 4.1: board info, 200 bytes.
fn board_info(cfg: &Cfg) -> Vec<u8> {
    let mut c = vec![0u8; 200];
    put(&mut c, 0x00, &[0x04, 0x01, 0x00, 0x06]); // firmware version
    put(&mut c, 0x04, &[0x04, 0x01, 0x00, 0x03]); // hardware version
    put(&mut c, 0x0C, b"tessera\0"); // board name, short
    c[0x14..0x18].copy_from_slice(&cfg.flags);
    c[0x23] = 2;
    c[0x27] = 1;
    put(&mut c, 0x28, &[0x01, 0x00, 0x00, 0x00]); // boot version
    put(&mut c, 0x38, b"tessera\0\0\0\0\0\0\0\0\0"); // board name, long
    c[0xBB] = 0x1F; // zero makes the controller flood the device with requests
    c
}

/// `conmon.md` 4.2: product info, 336 bytes.
fn product_info() -> Vec<u8> {
    let mut c = vec![0u8; 336];
    put(&mut c, 0x00, b"tessera\0"); // manufacturer, short
    put(&mut c, 0x08, b"tessera\0"); // board name, short
    put(&mut c, 0x1C, &[0, 1, 0, 0]); // product version: major, minor, patch u16
    put(&mut c, 0x2C, b"tessera\0\0\0\0\0\0\0\0\0"); // manufacturer, long
    put(&mut c, 0xAC, b"tessera\0\0\0\0\0\0\0\0\0"); // model name
    // Product version at +0x12C (u8 major, u8 minor, u16 patch) and as dotted ASCII at +0x130: every real
    // device fills both (`interop-fixes.md` F6); this is the device's own version, not the reference's.
    put(&mut c, 0x12C, &[0, 1, 0, 0]);
    put(&mut c, 0x130, b"0.1.0\0");
    c
}

/// IEEE 1588 port-state number for a daemon clock state (the u16 words of clock stats).
fn state_word(state: &str) -> u16 {
    match state {
        "leader" => 6,       // MASTER
        "locked" => 9,       // SLAVE
        "acquiring" => 8,    // UNCALIBRATED
        "holdover" => 8,
        "listening" | "free-running" => 4, // LISTENING
        _ => 3,              // DISABLED
    }
}

/// A clock id from the daemon's 12-hex-digit grandmaster (a PTPv1 uuid = MAC).
fn clock_id(mac_hex: &str, mac_form: bool) -> Option<[u8; 8]> {
    if mac_hex.len() != 12 {
        return None;
    }
    let mut m = [0u8; 6];
    for (i, b) in m.iter_mut().enumerate() {
        *b = u8::from_str_radix(&mac_hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(if mac_form {
        [m[0], m[1], m[2], m[3], m[4], m[5], 0x00, 0x00]
    } else {
        [m[0], m[1], m[2], 0xFF, 0xFE, m[3], m[4], m[5]]
    })
}

/// Reads the daemon's clock once a second and keeps `cfg.live` current.
fn poll_daemon(cfg: Arc<Cfg>) {
    let mut last = String::new();
    let mut traffic: Traffic = None;
    let mut hists = std::collections::HashMap::new();
    loop {
        let out = std::process::Command::new("tesseractl").args(["--json", "status"]).output();
        let mut live = Live { ok: false, state_word: 3, gm: cfg.gm, ppb: 0, priority: 128, rate: 48_000, encoding: 24, latency_us: 10_000 };
        if let Ok(o) = out {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&o.stdout) {
                let c = &v["clocks"][0];
                let state = c["state"].as_str().unwrap_or("");
                live.ok = !state.is_empty();
                live.state_word = state_word(state);
                if let Some(g) = c["grandmaster"].as_str().and_then(|g| clock_id(g, cfg.gm_mac_form)) {
                    live.gm = g;
                }
                live.ppb = c["rate_ppb"].as_i64().unwrap_or(0) as i32;
                live.priority = v["clock_settings"]["priority"].as_u64().unwrap_or(128) as u8;
                let d = &v["devices"][0];
                live.rate = d["sample_rate"].as_u64().unwrap_or(48_000) as u32;
                live.encoding = d["encoding"].as_u64().unwrap_or(24) as u32;
                live.latency_us = d["latency"].as_u64().unwrap_or(10_000) as u32;
                let line = format!("state {state} -> word {} gm {} ppb {} priority {}", live.state_word, hex(&live.gm), live.ppb, live.priority);
                if line != last {
                    println!("{} daemon clock: {line}", now());
                    last = line;
                }
            }
        }
        // The daemon is down (restarting): keep the last known format instead of the 48 kHz defaults,
        // or a flow request sent in that moment would carry the wrong rate (the transmitter answers
        // 0x0301, rate mismatch) and be retried only a whole planning cycle later.
        if !live.ok {
            let l = cfg.live.lock().unwrap();
            live.rate = l.rate;
            live.encoding = l.encoding;
            live.latency_us = l.latency_us;
        }
        // Lock order everywhere: `st` before `live`; never hold `live` while taking `st`.
        let changed = {
            let mut l = cfg.live.lock().unwrap();
            let c = l.ok && live.ok && (l.rate, l.encoding) != (live.rate, live.encoding);
            let old = (l.rate, l.encoding);
            *l = live;
            c.then_some(old)
        };
        if let Some((r, e)) = changed {
            // Format change: every flow of this device is now wrong. Drop the TX flows (receivers
            // re-request and are checked) and re-plan the RX flows at once.
            println!("{} format changed {r}/{e} -> {}/{}: dropping TX flows, re-planning RX", now(), live.rate, live.encoding);
            let mut st = cfg.st.lock().unwrap();
            st.tx_flows.clear();
            st.tx_seen.clear();
            cfg.flows_dirty.store(true, Ordering::Relaxed);
            cfg.rx_dirty.store(true, Ordering::Relaxed);
        }
        poll_stats(&cfg, &mut traffic, &mut hists);
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Interface counters (sysfs) at the previous poll: (interface, tx bytes, rx bytes, time).
type Traffic = Option<(String, u64, u64, std::time::Instant)>;

fn sysfs_u64(iface: &str, name: &str) -> u64 {
    std::fs::read_to_string(format!("/sys/class/net/{iface}/statistics/{name}"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// The interface that carries `ip` (`ip -o -4 addr`).
fn iface_of(ip: Ipv4Addr) -> Option<String> {
    let o = std::process::Command::new("ip").args(["-o", "-4", "addr"]).output().ok()?;
    let needle = format!("inet {ip}/");
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .find(|l| l.contains(&needle))
        .and_then(|l| l.split_whitespace().nth(1).map(str::to_owned))
}

fn poll_stats(cfg: &Cfg, traffic: &mut Traffic, hists: &mut std::collections::HashMap<String, Vec<u64>>) {
    let mut s = Stats::default();
    if traffic.is_none() {
        if let Some(i) = iface_of(cfg.ip) {
            *traffic = Some((i.clone(), sysfs_u64(&i, "tx_bytes"), sysfs_u64(&i, "rx_bytes"), std::time::Instant::now()));
        }
    }
    if let Some((iface, tx0, rx0, t0)) = traffic.as_mut() {
        let (tx, rx, t) = (sysfs_u64(iface, "tx_bytes"), sysfs_u64(iface, "rx_bytes"), std::time::Instant::now());
        let dt = t.duration_since(*t0).as_secs_f64().max(0.001);
        s.tx_bps = (tx.saturating_sub(*tx0) as f64 / dt) as u32;
        s.rx_bps = (rx.saturating_sub(*rx0) as f64 / dt) as u32;
        s.tx_errors = sysfs_u64(iface, "tx_errors") as u32;
        s.rx_errors = sysfs_u64(iface, "rx_errors") as u32;
        (*tx0, *rx0, *t0) = (tx, rx, t);
    }
    if let Ok(o) = std::process::Command::new("tesseractl").args(["--json", "metrics"]).output() {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&o.stdout) {
            for f in v["flows"].as_array().into_iter().flatten() {
                if f["direction"] == "rx" {
                    let late = f["rx"]["late_packets"].as_u64().unwrap_or(0) as u32;
                    let id = f["id"].as_str().unwrap_or("").to_owned();
                    // Lateness (arrival minus packet time) histogram, bucket upper edges in us
                    // (tessera-media `LATENESS_EDGES_NS`); report the edge of the highest bucket
                    // that grew since the last poll, as samples.
                    const EDGES_US: [u32; 11] = [0, 125, 250, 500, 1000, 2000, 4000, 8000, 16000, 32000, 64000];
                    let h: Vec<u64> = f["rx"]["lateness_hist"].as_array().into_iter().flatten().map(|v| v.as_u64().unwrap_or(0)).collect();
                    let prev = hists.get(&id).cloned().unwrap_or_default();
                    let top = (0..h.len()).rev().find(|&b| h[b] > prev.get(b).copied().unwrap_or(0));
                    let rate = cfg.live.lock().unwrap().rate;
                    let samples = top.map_or(0, |b| (u64::from(EDGES_US[b.min(10)]) * u64::from(rate) / 1_000_000) as u32);
                    hists.insert(id.clone(), h);
                    s.rx_flows.push((id, late, samples));
                }
            }
        }
    }
    *cfg.stats.lock().unwrap() = s;
}

/// Clock values to report: live from the daemon, or the static options if unreadable.
fn clock_now(cfg: &Cfg) -> (u16, [u8; 8], i32) {
    let l = *cfg.live.lock().unwrap();
    if l.ok { (l.state_word, l.gm, l.ppb) } else { (cfg.clock_state[0], cfg.gm, 0) }
}

/// `conmon.md` 4.3: clock stats, 112 bytes. The grandmaster is claimed to be
/// this device itself (an EUI-64 made from its MAC), offset 0 ppb.
fn clock_stats(cfg: &Cfg) -> Vec<u8> {
    let mut c = vec![0u8; 112];
    let (word, gm, ppb) = clock_now(cfg);
    put(&mut c, 0x00, &word.to_be_bytes());
    put(&mut c, 0x02, &cfg.clock_state[1].to_be_bytes());
    put(&mut c, 0x04, &[0x00, 0x00, 0x00, 0x9F]);
    put(&mut c, 0x08, &ppb.to_be_bytes());
    put(&mut c, 0x0C, &cfg.mac);
    put(&mut c, 0x14, &gm);
    put(&mut c, 0x1C, &gm);
    c
}

/// A clock-status record captured from a real device (AVIO-AES3 follower, netaudio
/// fixture `tests/fixtures/clock_status/avio-aes3.hex`), from UDP offset 24: revision
/// 0x0738, id 0x0020, then the body. Used as the template for [`clock_status`].
const CLOCK_STATUS_TEMPLATE: &str = "0738002000000000000300030000009fffff774b001dc153ef370000001dc10812580000001dc1081258000000010034000900000294000000030d4000000002000000000000000000000000000000000000000100600c000000000c0098002000030000006810000000000101020100000000020009000700010002020202000000000200030003000100030202010000000002000300070003000700b80004001dc1fffe53ef37001dc1fffe081258001dc1fffe081258000100000001000000010000";

fn eui64(m: &[u8]) -> [u8; 8] {
    [m[0], m[1], m[2], 0xFF, 0xFE, m[3], m[4], m[5]]
}

/// Clock status (id 0x0020) in the real-device layout. Offsets below are relative to
/// UDP offset 24 (the 8 bytes `conmon()` calls the opcode are the first 8 here).
/// Returns (opcode, content).
fn clock_status(cfg: &Cfg) -> ([u8; 8], Vec<u8>) {
    let t = CLOCK_STATUS_TEMPLATE;
    let mut r: Vec<u8> = (0..t.len() / 2).map(|i| u8::from_str_radix(&t[i * 2..i * 2 + 2], 16).unwrap()).collect();
    let l = *cfg.live.lock().unwrap();
    let (port, gm, ppb) = clock_now(cfg);
    // Port state -> (clock state none/passive/undisciplined/disciplined, servo state
    // faulty/reset/synchronizing/synchronized/...).
    let (clock, servo) = match port {
        6 | 9 => (3u16, 3u16),
        8 => (2, 2),
        4 => (1, 1),
        _ => (0, 0),
    };
    let preferred = l.ok && l.priority < 64;
    put16(&mut r, 8, clock);
    put16(&mut r, 10, servo);
    r[14] = u8::from(preferred);
    put(&mut r, 16, &ppb.to_be_bytes());
    let own = [cfg.mac[0], cfg.mac[1], cfg.mac[2], cfg.mac[3], cfg.mac[4], cfg.mac[5], 0, 0];
    // A leader names itself as master and grandmaster.
    let master = if port == 6 { own } else { [gm[0], gm[1], gm[2], gm[3], gm[4], gm[5], 0, 0] };
    put(&mut r, 20, &own);
    put(&mut r, 28, &master);
    put(&mut r, 36, &master);
    put16(&mut r, 48, port); // base port 0
    put16(&mut r, 116, port); // port record 1: PTPv1 multicast
    put(&mut r, 160, &eui64(&own));
    put(&mut r, 168, &eui64(&master));
    put(&mut r, 176, &eui64(&master));
    let mut op = [0u8; 8];
    op.copy_from_slice(&r[..8]);
    (op, r[8..].to_vec())
}

/// A clock control request (id 0x0021). Record from UDP offset 24: +8 u16 change mask
/// (1 clock source, 2 preferred leader, 8 subdomain), +12 preferred value, +32/+34 the
/// extended change mask and values. A query has both masks zero.
fn clock_control(req: &[u8]) {
    let (Some(mask), Some(ext)) = (req.get(32..34), req.get(56..58)) else { return };
    let mask = u16::from_be_bytes([mask[0], mask[1]]);
    let ext = u16::from_be_bytes([ext[0], ext[1]]);
    if mask == 0 && ext == 0 {
        return;
    }
    println!("{} info CLOCK CONTROL mask {mask:04x} ext {ext:04x} record {}", now(), hex(&req[24..]));
    if mask & 2 != 0 {
        let on = req.get(36).copied().unwrap_or(0) != 0;
        let prio = if on { "10" } else { "128" };
        let r = std::process::Command::new("tesseractl").args(["clock", "set-priority", prio]).output();
        println!("{} info PREFERRED LEADER {on} -> tesseractl clock set-priority {prio}: {:?}", now(), r.map(|o| o.status));
    }
}

/// tessera-only ARC opcode between two probes: "send me these TX channels".
const FLOW_REQUEST: u16 = 0x7E00;
/// Audio flows reach a receiver on this port plus the last octet of the sender.
const FLOW_PORT_BASE: u16 = 14336;
const FLOWS_FILE: &str = "/etc/tessera/config.d/90-probe-flows.toml";

/// Plans the RX flows when subscriptions change, and writes the daemon
/// configuration when any flow changes.
/// Keeps the daemon's stored subscriptions (state.toml) equal to the controller's, so they
/// survive restarts of the probe and of the host.
fn persist_sub(rx: usize, sub: Option<(String, String)>) {
    let rx = rx.to_string();
    std::thread::spawn(move || {
        let r = match &sub {
            Some((tx, dev)) => std::process::Command::new("tesseractl").args(["subscribe", &rx, tx, dev]).output(),
            None => std::process::Command::new("tesseractl").args(["unsubscribe", &rx]).output(),
        };
        if !r.as_ref().is_ok_and(|o| o.status.success()) {
            println!("{} persist subscription rx {rx}: {:?}", now(), r.map(|o| String::from_utf8_lossy(&o.stderr).into_owned()));
        }
    });
}

/// Loads the subscriptions the daemon stored.
fn restore_subs(cfg: &Cfg) {
    let Ok(o) = std::process::Command::new("timeout").args(["-k", "2", "5", "tesseractl", "--json", "status"]).output() else { return };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&o.stdout) else { return };
    let mut st = cfg.st.lock().unwrap();
    for s in v["devices"][0]["subscriptions"].as_array().into_iter().flatten() {
        let rx = s["rx_channel"].as_u64().unwrap_or(0) as usize;
        if let (Some(tx), Some(dev)) = (s["tx_channel"].as_str(), s["tx_device"].as_str()) {
            if (1..=st.subs.len()).contains(&rx) {
                println!("{} restored subscription rx {rx} <- {tx:?} @ {dev:?}", now());
                st.subs[rx - 1] = Some((tx.to_owned(), dev.to_owned()));
                st.sub_codes[rx - 1] = 8;
            }
        }
    }
}

/// TX flows from the flow file this probe wrote last time, so a probe restart does not cut
/// the audio of receivers until they refresh.
fn load_tx_flows(cfg: &Cfg) {
    let Ok(text) = std::fs::read_to_string(FLOWS_FILE) else { return };
    let mut st = cfg.st.lock().unwrap();
    for block in text.split("[[flow]]").skip(1) {
        let val = |k: &str| {
            block.lines().find_map(|l| l.strip_prefix(&format!("{k} = ")).map(|v| v.trim().trim_matches('"').to_owned()))
        };
        if val("direction").as_deref() != Some("tx") {
            continue;
        }
        let (Some(id), Some(peer), Some(port), Some(ch)) = (val("id"), val("peer"), val("port"), val("channels")) else { continue };
        let (Ok(peer), Ok(port)) = (peer.parse(), port.parse()) else { continue };
        let channels: Vec<u16> = ch.trim_matches(|c| c == '[' || c == ']').split(',').filter_map(|s| s.trim().parse().ok()).collect();
        println!("{} flows: kept {id} from the flow file", now());
        st.tx_seen.insert(id.clone(), std::time::Instant::now());
        // A receiver of real flow control asks once and then only sends keepalives, so a kept real flow
        // must not expire for want of a refresh; a flow made through ARC still needs its refresh.
        if block.contains("# probe-real") {
            st.tx_real.insert(id.clone());
        }
        let fpp = val("frames_per_packet").and_then(|v| v.parse().ok()).unwrap_or(0);
        st.tx_flows.push(AudioFlow { id, tx: true, peer_name: String::new(), channels, peer, port, fpp });
    }
}

fn flow_worker(cfg: Arc<Cfg>) {
    load_tx_flows(&cfg);
    restore_subs(&cfg);
    cfg.rx_dirty.store(true, Ordering::Relaxed);
    let mut last_plan = std::time::Instant::now();
    let mut first = true;
    loop {
        // TX flows a receiver stopped refreshing expire.
        {
            let mut st = cfg.st.lock().unwrap();
            let stale: Vec<String> = st
                .tx_seen
                .iter()
                .filter(|(id, t)| t.elapsed() > Duration::from_secs(75) && !st.tx_real.contains(*id))
                .map(|(id, _)| id.clone())
                .collect();
            for id in stale {
                println!("{} flows: {id} expired (not refreshed)", now());
                st.tx_seen.remove(&id);
                st.tx_flows.retain(|f| f.id != id);
                cfg.flows_dirty.store(true, Ordering::Relaxed);
            }
        }
        // Re-request flows now and then, so a transmitter that restarted learns them again.
        if last_plan.elapsed() > Duration::from_secs(10) {
            cfg.rx_dirty.store(true, Ordering::Relaxed);
        }
        if cfg.rx_dirty.swap(false, Ordering::Relaxed) {
            plan_rx(&cfg);
            last_plan = std::time::Instant::now();
            if first {
                // Once at start: make the daemon's flow file match this state (drops leftovers).
                cfg.flows_dirty.store(true, Ordering::Relaxed);
                first = false;
            }
        }
        if cfg.flows_dirty.swap(false, Ordering::Relaxed) {
            write_flows(&cfg);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn resolve(host: &str) -> Option<Ipv4Addr> {
    let o = std::process::Command::new("timeout").args(["-k", "2", "3", "avahi-resolve-host-name", "-4", &format!("{host}.local")]).output().ok().filter(|o| o.status.success())?;
    String::from_utf8_lossy(&o.stdout).split_whitespace().nth(1)?.parse().ok()
}

/// Asks the probe at `to` for a flow of `names` to this device; returns the TX
/// channel numbers, or None if it does not answer.
/// Answer: (transmitter rate, transmitter encoding, TX channel numbers).
fn request_flow(cfg: &Cfg, to: Ipv4Addr, port: u16, names: &[String]) -> Option<(u32, u16, Vec<u16>)> {
    let mut c = cfg.ip.octets().to_vec();
    c.extend_from_slice(&port.to_be_bytes());
    c.push(names.len() as u8);
    for n in names {
        c.extend_from_slice(n.as_bytes());
        c.push(0);
    }
    // After the channel names: this device's name (shown as the flow's destination).
    c.extend_from_slice(cfg.name.as_bytes());
    c.push(0);
    // Then this device's format: u32 sample rate, u16 encoding.
    let (rate, enc) = rate_enc(cfg);
    c.extend_from_slice(&rate.to_be_bytes());
    c.extend_from_slice(&enc.to_be_bytes());
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut m = vec![0x27, 0x29];
    m.extend_from_slice(&((10 + c.len()) as u16).to_be_bytes());
    m.extend_from_slice(&seq.to_be_bytes());
    m.extend_from_slice(&FLOW_REQUEST.to_be_bytes());
    m.extend_from_slice(&0u16.to_be_bytes());
    m.extend_from_slice(&c);
    let sock = UdpSocket::bind((cfg.ip, 0)).ok()?;
    sock.set_read_timeout(Some(Duration::from_millis(700))).ok()?;
    let mut buf = [0u8; 1500];
    for _ in 0..3 {
        sock.send_to(&m, (to, 4440)).ok()?;
        if let Ok((n, _)) = sock.recv_from(&mut buf) {
            if n >= 10 && be16(&buf, 4) == seq && be16(&buf, 8) == 1 {
                if n < 16 {
                    return None;
                }
                let rate = u32::from_be_bytes([buf[10], buf[11], buf[12], buf[13]]);
                let enc = u16::from_be_bytes([buf[14], buf[15]]);
                let nums = buf[16..n].chunks_exact(2).map(|p| u16::from_be_bytes([p[0], p[1]])).collect();
                return Some((rate, enc, nums));
            }
        }
    }
    None
}

fn plan_rx(cfg: &Cfg) {
    // No requests while the daemon is down: plan again as soon as it answers.
    if !cfg.live.lock().unwrap().ok {
        cfg.rx_dirty.store(true, Ordering::Relaxed);
        return;
    }
    let (subs, old_peers) = {
        let st = cfg.st.lock().unwrap();
        (st.subs.clone(), st.peers.clone())
    };
    // TX host -> [(rx channel, tx channel name)]
    let mut by_host: std::collections::BTreeMap<String, Vec<(u16, String)>> = Default::default();
    for (i, s) in subs.iter().enumerate() {
        if let Some((tx, host)) = s {
            by_host.entry(host.clone()).or_default().push(((i + 1) as u16, tx.clone()));
        }
    }
    let mut codes: Vec<(u16, u16)> = Vec::new();
    let mut flows = Vec::new();
    let mut peers = Vec::new();
    for (host, list) in &by_host {
        if *host == cfg.name {
            // Loopback subscriptions are not implemented.
            codes.extend(list.iter().map(|(rx, _)| (*rx, 0x001c)));
            continue;
        }
        if let Some(ch) = flowctl::chan_lookup(host) {
            let mut names: Vec<String> = Vec::new();
            let mut rxs: Vec<u16> = Vec::new();
            let mut ids: Vec<u16> = Vec::new();
            for (rx, tx) in list {
                match ch.ids.get(tx) {
                    None => codes.push((*rx, 0x0005)),
                    Some(_) if names.contains(tx) => codes.push((*rx, 0x001c)),
                    Some(id) => {
                        names.push(tx.clone());
                        rxs.push(*rx);
                        ids.push(*id);
                    }
                }
            }
            let base_port = FLOW_PORT_BASE + u16::from(ch.ip.octets()[3]);
            peers.push((host.clone(), ch.ip));
            if names.is_empty() {
                continue;
            }
            // A flow that is receiving is left alone (a new request makes a new flow on the
            // transmitter); one that stopped receiving, or never worked, is asked for again.
            let flow_id = format!("probe-rx-{}", host.replace(|c: char| !c.is_ascii_alphanumeric(), "_"));
            let packets_now = flowctl::rx_packets(&flow_id);
            let cached = {
                let mut st = cfg.st.lock().unwrap();
                match st.real_plans.get_mut(host) {
                    Some(p) if p.names == names && p.result.is_ok() => {
                        // The daemon's counter went backwards: it restarted (the transmitter has probably stopped
                        // sending during the gap): ask again at once instead of after two more checks. Ignored in the
                        // first 15 s after a request: a re-request changes the port, which restarts that flow's counter.
                        let reset = p.at.elapsed() >= Duration::from_secs(15) && packets_now.is_some_and(|n| n < p.packets);
                        let alive = !reset && (p.at.elapsed() < Duration::from_secs(15) || packets_now.is_some_and(|n| n != p.packets));
                        if reset {
                            p.fails = 0;
                            p.next_try = std::time::Instant::now();
                        }
                        if let Some(n) = packets_now {
                            p.packets = n;
                        }
                        if alive {
                            p.fails = 0;
                            Some(p.clone())
                        } else if std::time::Instant::now() < p.next_try {
                            // Backing off: the transmitter accepted the last request but sends nothing
                            // (stopped, restarted, or its audio is off); asking again every 15 s would
                            // make a new flow on it each time.
                            Some(p.clone())
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            };
            let plan = match cached {
                Some(p) => p,
                None => {
                    // A new request makes a new flow on the transmitter: release the old one first.
                    let old = cfg.st.lock().unwrap().real_plans.get(host).cloned();
                    let fails = old.as_ref().map_or(0, |p| if p.names == names { p.fails + 1 } else { 0 });
                    let rot = old.as_ref().map_or(0, |p| p.rot.wrapping_add(1));
                    // Alternate between two ports (both inside the receive port ranges the 0x3300 reply advertises): a
                    // transmitter may keep its old flow to the previous port for a while.
                    let port = base_port + 256 * (rot % 2);
                    if let Some(flowctl::RealPlan { result: Ok(h), ip, port: p, .. }) = old {
                        flowctl::stop_real(cfg, ip, p, h);
                    }
                    let result = flowctl::request_real(cfg, &ch, &ids, port);
                    println!("{} flows: real flow request to {host} ({}:{}) ids {ids:?} rx port {port}: {result:02x?}", now(), ch.ip, ch.port);
                    let p = flowctl::RealPlan { names: names.clone(), at: std::time::Instant::now(), ip: ch.ip, port: ch.port, result, ids: ids.clone(), packets: 0, fails, next_try: std::time::Instant::now() + Duration::from_secs((15u64 << fails.saturating_sub(1).min(3)).min(120)), rx_port: port, rot };
                    cfg.st.lock().unwrap().real_plans.insert(host.clone(), p.clone());
                    p
                }
            };
            match plan.result {
                Ok(_) => {
                    codes.extend(rxs.iter().map(|rx| (*rx, 0x0009)));
                    let id = format!("probe-rx-{}", host.replace(|c: char| !c.is_ascii_alphanumeric(), "_"));
                    flows.push(AudioFlow { id, tx: false, peer_name: host.clone(), channels: rxs.clone(), peer: ch.ip, port: plan.rx_port, fpp: 0 });
                }
                Err(0xFFFF) => codes.extend(rxs.iter().map(|rx| (*rx, 0x000f))),
                Err(0x0301) => codes.extend(rxs.iter().map(|rx| (*rx, 0x0010))),
                Err(_) => codes.extend(rxs.iter().map(|rx| (*rx, 0x0015))),
            }
            continue;
        }
        let Some(ip) = resolve(host) else {
            println!("{} flows: {host} does not resolve", now());
            codes.extend(list.iter().map(|(rx, _)| (*rx, 0x0001)));
            continue;
        };
        // A TX channel can appear once per flow; a second RX channel on it is unsupported.
        let mut names: Vec<String> = Vec::new();
        let mut rxs: Vec<u16> = Vec::new();
        for (rx, tx) in list {
            if names.contains(tx) {
                codes.push((*rx, 0x001c));
            } else {
                names.push(tx.clone());
                rxs.push(*rx);
            }
        }
        let port = FLOW_PORT_BASE + u16::from(ip.octets()[3]);
        peers.push((host.clone(), ip));
        match request_flow(cfg, ip, port, &names) {
            None => {
                println!("{} flows: {host} ({ip}) did not answer the flow request", now());
                codes.extend(rxs.iter().map(|rx| (*rx, 0x000f)));
            }
            Some((trate, tenc, _)) if (trate, tenc) != rate_enc(cfg) => {
                println!("{} flows: {host} sends {trate} Hz / {tenc} bit, this device runs {:?}: format mismatch", now(), rate_enc(cfg));
                codes.extend(rxs.iter().map(|rx| (*rx, 0x0010)));
            }
            Some((_, _, nums)) => {
                let mut channels = Vec::new();
                for (rx, n) in rxs.iter().zip(nums.iter().chain(std::iter::repeat(&0))) {
                    if *n == 0 {
                        codes.push((*rx, 0x0005));
                    } else {
                        codes.push((*rx, 0x0009));
                        channels.push(*rx);
                    }
                }
                println!("{} flows: {host} ({ip}) port {port}: tx {nums:?} -> rx {channels:?}", now());
                if !channels.is_empty() {
                    let id = format!("probe-rx-{}", host.replace(|c: char| !c.is_ascii_alphanumeric(), "_"));
                    flows.push(AudioFlow { id, tx: false, peer_name: host.clone(), channels, peer: ip, port, fpp: 0 });
                }
            }
        }
    }
    // Release flows from devices no longer subscribed to.
    for (host, ip) in &old_peers {
        if !peers.iter().any(|(h, _)| h == host) {
            let real = cfg.st.lock().unwrap().real_plans.remove(host);
            if let Some(p) = real {
                if let Ok(h) = p.result {
                    flowctl::stop_real(cfg, p.ip, p.port, h);
                }
                continue;
            }
            let port = FLOW_PORT_BASE + u16::from(ip.octets()[3]);
            let r = request_flow(cfg, *ip, port, &[]);
            println!("{} flows: released {host} ({ip}): {}", now(), if r.is_some() { "ok" } else { "no answer" });
        }
    }
    let mut st = cfg.st.lock().unwrap();
    for (rx, code) in codes {
        // Only if the subscription is still the one planned for.
        if st.subs.get(usize::from(rx) - 1).is_some_and(|s| s.is_some()) {
            st.sub_codes[usize::from(rx) - 1] = code;
        }
    }
    st.peers = peers;
    if st.rx_flows != flows {
        st.rx_flows = flows;
        cfg.flows_dirty.store(true, Ordering::Relaxed);
    }
}

fn write_flows(cfg: &Cfg) {
    let (flows, real): (Vec<AudioFlow>, std::collections::HashSet<String>) = {
        let st = cfg.st.lock().unwrap();
        (st.rx_flows.iter().chain(st.tx_flows.iter()).cloned().collect(), st.tx_real.clone())
    };
    let mut t = String::from("# written by cmc-probe (scratch): flows from controller subscriptions\n");
    for f in &flows {
        let ch: Vec<String> = f.channels.iter().map(u16::to_string).collect();
        t += &format!(
            "\n[[flow]]\nid = \"{}\"\ndirection = \"{}\"\ndevice = \"main\"\nchannels = [{}]\npeer = \"{}\"\nport = {}\n",
            f.id,
            if f.tx { "tx" } else { "rx" },
            ch.join(", "),
            f.peer,
            f.port
        );
        if f.tx && f.fpp > 0 {
            t += &format!("frames_per_packet = {}\n", f.fpp);
        }
        if real.contains(&f.id) {
            t += "# probe-real\n";
        }
    }
    let ok = (|| {
        use std::io::Write;
        let mut p = std::process::Command::new("timeout")
            .args(["-k", "2", "5", "sudo", "-n", "tee", FLOWS_FILE])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .ok()?;
        p.stdin.take()?.write_all(t.as_bytes()).ok()?;
        p.wait().ok()?.success().then_some(())?;
        let r = std::process::Command::new("timeout").args(["-k", "2", "20", "sudo", "-n", "systemctl", "reload", "tesserad"]).status().ok()?;
        r.success().then_some(())
    })();
    println!("{} flows: wrote {} flow(s) to {FLOWS_FILE}, reload {}", now(), flows.len(), if ok.is_some() { "ok" } else { "FAILED" });
    if ok.is_none() {
        let mut st = cfg.st.lock().unwrap();
        let rx: Vec<u16> = st.rx_flows.iter().flat_map(|f| f.channels.clone()).collect();
        for c in rx {
            st.sub_codes[usize::from(c) - 1] = 0x0013;
        }
    }
}

/// `conmon.md` 4.4: network info, 64 bytes.
fn network_info(cfg: &Cfg) -> Vec<u8> {
    let mut c = vec![0u8; 64];
    put(&mut c, 0x00, &[0x00, 0x01, 0x00, 0x00, 0x00, 0x00]);
    put(&mut c, 0x06, &1000u16.to_be_bytes()); // link speed, Mb/s
    put(&mut c, 0x08, &1u16.to_be_bytes());
    put(&mut c, 0x0A, &cfg.mac);
    put(&mut c, 0x10, &cfg.ip.octets());
    put(&mut c, 0x14, &cfg.netmask.octets());
    put(&mut c, 0x18, &cfg.gateway.octets());
    put(&mut c, 0x1C, &cfg.gateway.octets()); // the reference repeats the gateway
    put(&mut c, 0x20, &[0x00, 0x18, 0x00, 0x30]);
    c
}

fn opcode(id: u16) -> [u8; 8] {
    let i = id.to_be_bytes();
    [0x07, 0x2A, i[0], i[1], 0, 0, 0, 0]
}

/// The answer to a request received on the info port (`conmon.md` section 3).
fn info_reply(req: &[u8], cfg: &Cfg) -> Option<(&'static str, Vec<u8>)> {
    if req.len() < 32 || be16(req, 0) != 0xffff {
        return None;
    }
    match be16(req, 26) {
        0x0061 => Some(("board info", conmon(cfg, 0xFFFF, opcode(0x0060), &board_info(cfg)))),
        0x00C1 => Some(("product info", conmon(cfg, 0xFFFF, opcode(0x00C0), &product_info()))),
        0x0021 if std::env::var_os("OLD_CLOCK_STATS").is_some() => {
            Some(("clock stats", conmon(cfg, 0xFFFF, opcode(0x0020), &clock_stats(cfg))))
        }
        0x0021 => {
            clock_control(req);
            let (op, c) = clock_status(cfg);
            Some(("clock status", conmon(cfg, 0xFFFF, op, &c)))
        }
        0x0013 => Some(("network info", conmon(cfg, 0xFFFF, opcode(0x0011), &network_info(cfg)))),
        // 4.6: answers recorded from a real device (guess): supported sample
        // rates, supported encodings.
        // Sample rate (0x0081 -> 0x0080) and encoding (0x0083 -> 0x0082), `controller-netaudio.md`
        // 5: a request whose u32 at 32 is 1 is a write of the u32 at 36.
        0x0081 | 0x0083 => {
            let rate = be16(req, 26) == 0x0081;
            let write = req.len() >= 40 && u32::from_be_bytes([req[32], req[33], req[34], req[35]]) == 1;
            if write {
                let value = u32::from_be_bytes([req[36], req[37], req[38], req[39]]);
                let cmd = if rate { "set-rate" } else { "set-encoding" };
                let r = std::process::Command::new("tesseractl").args([cmd, &value.to_string()]).output();
                println!("{} info WRITE {cmd} {value}: {:?}", now(), r.map(|o| o.status));
                std::thread::sleep(Duration::from_millis(1200)); // let the poller see the change
            }
            let l = *cfg.live.lock().unwrap();
            let (current, values): (u32, &[u32]) = if rate {
                (l.rate, &[44_100, 48_000, 88_200, 96_000, 176_400, 192_000])
            } else {
                (l.encoding, &[24, 16, 32])
            };
            let mut c = Vec::new();
            for v in [0x0018u16, values.len() as u16] {
                c.extend_from_slice(&v.to_be_bytes());
            }
            c.extend_from_slice(&current.to_be_bytes());
            c.extend_from_slice(&current.to_be_bytes());
            c.extend_from_slice(&0x0002_0000u32.to_be_bytes()); // update mode 2: writable
            for v in values {
                c.extend_from_slice(&v.to_be_bytes());
            }
            let id = if rate { 0x0080 } else { 0x0082 };
            Some((if rate { "sample rate" } else { "encoding" }, conmon(cfg, 0xFFFF, opcode(id), &c)))
        }
        // 0x1008 -> 0x1009 device lock / reset status (`controller-netaudio.md` 5): lock state,
        // status, no lock ids. The file /tmp/probe-locked switches the reported state to locked
        // (a display test only: tessera cannot accept vendor lock commands).
        0x1008 => {
            let locked = u16::from(std::path::Path::new("/tmp/probe-locked").exists());
            let mut c = Vec::new();
            for v in [locked, 0, 0, 8, 0, 0, 0, 0] {
                c.extend_from_slice(&v.to_be_bytes());
            }
            Some(("0x1008 -> 0x1009 lock status", conmon(cfg, 0xFFFF, opcode(0x1009), &c)))
        }
        // Interface statistics (0x0041 -> 0x0040): one group, one 24-byte record.
        0x0041 => {
            let s = cfg.stats.lock().unwrap().clone();
            // Record from offset 24: +0 revision (in the opcode), +8 group count, +10 group
            // pointer, +12 header-record pointer, header (capability mask at +0x10), group.
            let mut b = vec![0u8; 0x40];
            put16(&mut b, 0x08, 1); // one group
            put16(&mut b, 0x0A, 0x10); // group at body 0x10
            put16(&mut b, 0x0C, 0x14); // header record at body 0x14
            put16(&mut b, 0x10, 1); // group: one record
            put16(&mut b, 0x12, 0x28); // record at body 0x28
            put(&mut b, 0x14 + 0x10, &3u32.to_be_bytes()); // utilization + errors
            let mut r = Vec::new();
            for v in [s.tx_bps, s.rx_bps, s.tx_errors, s.rx_errors, 1, 1000] {
                r.extend_from_slice(&v.to_be_bytes());
            }
            put(&mut b, 0x28, &r);
            Some(("interface statistics", conmon(cfg, 0xFFFF, [0x07, 0x13, 0x00, 0x40, 0, 0, 0, 0], &b[8..])))
        }
        // Identify (record revision 0x0731, message type 0x0063): log only.
        0x0063 => {
            println!("{} info IDENTIFY requested", now());
            None
        }
        // System reset (0x0090): mode 0 = reboot -> restart the daemon; mode 1 = factory reset -> refused.
        0x0090 => {
            let mode = if req.len() >= 36 { be16(req, 34) } else { 0 };
            if mode == 0 {
                let r = std::process::Command::new("sudo").args(["-n", "systemctl", "restart", "tesserad"]).status();
                println!("{} info REBOOT -> restart tesserad: {r:?}", now());
            } else {
                println!("{} info FACTORY RESET refused (mode {mode})", now());
            }
            None
        }
        0x0077 => Some((
            "0x0078",
            conmon(cfg, 0xFFFF, opcode(0x0078), &[0, 0, 0, 3, 0, 0, 0, 0]),
        )),
        _ => None,
    }
}

/// Heartbeat (`conmon.md` section 6): a clock-offset section (0x8001) of 0 ppb.
fn heartbeat(cfg: &Cfg) -> Vec<u8> {
    let ctr = SEQ.load(Ordering::Relaxed);
    let mut s = Vec::with_capacity(16);
    s.extend_from_slice(&16u16.to_be_bytes());
    s.extend_from_slice(&0x8001u16.to_be_bytes());
    s.extend_from_slice(&4u16.to_be_bytes());
    s.extend_from_slice(&4u16.to_be_bytes());
    s.extend_from_slice(&ctr.to_be_bytes());
    s.extend_from_slice(&0u16.to_be_bytes());
    s.extend_from_slice(&clock_now(cfg).2.to_be_bytes());
    let w = |s: &mut Vec<u8>, v: u16| s.extend_from_slice(&v.to_be_bytes());
    let st = cfg.stats.lock().unwrap().clone();
    let live = *cfg.live.lock().unwrap();
    // 0x8000 interface traffic (`controller-netaudio.md` 6): one interface, 16-byte entry.
    for v in [36u16, 0x8000, 4, 4, ctr, 0, 0x0010, 0, 1, 16] {
        w(&mut s, v);
    }
    for v in [st.tx_bps, st.rx_bps, st.tx_errors, st.rx_errors] {
        s.extend_from_slice(&v.to_be_bytes());
    }
    // 0x8003 per-flow latency and 0x8004 late packets: every real device sends both, with F
    // slots = the max-flow words of ARC 0x1000 (`interop-fixes.md` F5; the three real devices seen sent both,
    // whether a device without RX flows does is unknown); only active flows are non-zero.
    const SLOTS: u16 = 32;
    for (kind, len, vals) in [
        (0x8003u16, 24 + 4 * SLOTS, st.rx_flows.iter().map(|f| f.2).collect::<Vec<u32>>()),
        (0x8004u16, 20 + 4 * SLOTS, st.rx_flows.iter().map(|f| f.1).collect::<Vec<u32>>()),
    ] {
        let off = if kind == 0x8003 { 24 } else { 20 };
        for v in [len, kind, 4, len - 12, ctr, 0, SLOTS, 0, off, 0] {
            w(&mut s, v);
        }
        if kind == 0x8003 {
            s.extend_from_slice(&live.rate.to_be_bytes());
        }
        for i in 0..usize::from(SLOTS) {
            s.extend_from_slice(&vals.get(i).copied().unwrap_or(0).to_be_bytes());
        }
    }
    conmon(cfg, 0xFFFE, [0x00, 0x08, 0x00, 0x01, 0x10, 0x00, 0x00, 0x00], &s)
}

/// Send multicast out of the wired interface only (this machine also has Wi-Fi).
fn pin_multicast_to(sock: &UdpSocket, ip: Ipv4Addr) {
    let addr = libc::in_addr {
        s_addr: u32::from_ne_bytes(ip.octets()),
    };
    // SAFETY: `setsockopt` reads `size_of::<in_addr>()` bytes from `addr`,
    // which outlives the call; the fd is owned by `sock`.
    let rc = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_IF,
            (&raw const addr).cast(),
            size_of::<libc::in_addr>() as u32,
        )
    };
    if rc != 0 {
        println!("{} IP_MULTICAST_IF failed: {}", now(), std::io::Error::last_os_error());
    }
}

const TX_CHANNELS: u16 = 2;
const RX_CHANNELS: u16 = 2;

fn put16(buf: &mut [u8], at: usize, v: u16) {
    buf[at..at + 2].copy_from_slice(&v.to_be_bytes());
}

/// The supported-property table a real device sent for 0x1102 (`control-arc.md` 0.5):
/// (id, access mask; 3 = writable, 1 = read only).
const PROP_TABLE: [(u16, u16); 23] = [
    (0x8020, 1), (0x8021, 3), (0x0022, 3), (0x0023, 3), (0x0024, 1), (0x0201, 3),
    (0x8204, 3), (0x8205, 3), (0x020A, 1), (0x020B, 1), (0x0210, 3), (0x0211, 3),
    (0x0212, 3), (0x0213, 1), (0x0214, 1), (0x8301, 3), (0x8306, 1), (0x8302, 1),
    (0x0310, 1), (0x0311, 1), (0x0303, 3), (0x83F0, 1), (0x0601, 1),
];

/// A property's value: `--props` override, else what the real device answered.
fn prop_value(cfg: &Cfg, written: &[(u16, u32)], id: u16) -> u32 {
    // Configured and active latency come from the daemon.
    if matches!(id, 0x8205 | 0x8301) {
        return cfg.live.lock().unwrap().latency_us.saturating_mul(1000);
    }
    if let Some(&(_, v)) = written.iter().find(|(i, _)| *i == id) {
        return v;
    }
    if let Some(&(_, v)) = cfg.props.iter().find(|(i, _)| *i == id) {
        return v;
    }
    match id {
        0x0201 | 0x0022 => 1,
        0x8204 | 0x8205 | 0x8301 | 0x8306 => 1_000_000,
        0x8302 => 20_312_500,
        0x0210 | 0x0211 | 0x0310 | 0x0311 => 16,
        0x0303 => 2,
        _ => 0,
    }
}

/// `control-arc.md` 0.5: a property list in the real-device structure for `ids`.
fn props_list(cfg: &Cfg, written: &[(u16, u32)], ids: &[u16]) -> Vec<u8> {
    let data_start = 10 + 2 + 4 * ids.len();
    let (mut items, mut data) = (Vec::new(), Vec::new());
    for id in ids {
        if !PROP_TABLE.iter().any(|(i, _)| i == id) {
            items.extend_from_slice(&[0, 0]);
            items.extend_from_slice(&id.to_be_bytes());
        } else if id & 0x8000 != 0 {
            items.extend_from_slice(&id.to_be_bytes());
            items.extend_from_slice(&((data_start + data.len()) as u16).to_be_bytes());
            data.extend_from_slice(&prop_value(cfg, written, *id).to_be_bytes());
        } else {
            items.extend_from_slice(&id.to_be_bytes());
            items.extend_from_slice(&(prop_value(cfg, written, *id) as u16).to_be_bytes());
        }
    }
    let mut r = vec![ids.len() as u8, ids.len() as u8];
    r.extend_from_slice(&items);
    r.extend_from_slice(&data);
    r
}

/// The NUL-terminated string at a packet offset of the request.
fn cstr_at(req: &[u8], packet_off: usize) -> Option<String> {
    let b = req.get(packet_off..)?;
    let end = b.iter().position(|&x| x == 0)?;
    Some(String::from_utf8_lossy(&b[..end]).into_owned())
}

/// `control-arc.md` 4.9: the TX flows list (multicast flows only).
/// 0x3200 RX flow list in the layout of a real device's answer (`controller-netaudio.md` 3.3):
/// per flow an endpoint (where we listen), one RX-channel bitmap per flow slot and a 16-byte
/// status block (subscription status, interface state, flags, latency).
fn rx_flows_content(cfg: &Cfg, flows: &[AudioFlow], codes: &[u16]) -> Vec<u8> {
    const SLOTS_ALLOC: usize = 16;
    let l = *cfg.live.lock().unwrap();
    let n = flows.len().min(SLOTS_ALLOC);
    let mut c = vec![0u8; 2 + SLOTS_ALLOC * 2];
    c[0] = SLOTS_ALLOC as u8;
    c[1] = n as u8;
    for (i, f) in flows.iter().take(n).enumerate() {
        while (10 + c.len()) % 4 != 0 {
            c.push(0);
        }
        let rec = c.len();
        let slots = f.channels.len();
        let mut r = Vec::new();
        for v in [(i + 1) as u16, 0x0001] {
            r.extend_from_slice(&v.to_be_bytes());
        }
        r.extend_from_slice(&l.rate.to_be_bytes());
        r.extend_from_slice(&l.encoding.to_be_bytes());
        for v in [1u16, slots as u16, 8] {
            r.extend_from_slice(&v.to_be_bytes());
        }
        // Pointer table: endpoint, one per slot, status; filled below.
        let ptrs = r.len();
        r.resize(ptrs + 2 * (slots + 2), 0);
        let abs = |off: usize| (10 + rec + off) as u16;
        let mut slot_ptrs = Vec::new();
        for ch in &f.channels {
            slot_ptrs.push(abs(r.len()));
            let mut bm = [0u8; 16];
            let k = usize::from(ch.saturating_sub(1));
            if k < 128 {
                let word = (k / 16) * 2;
                let bit = 1u16 << (k % 16);
                let v = u16::from_be_bytes([bm[word], bm[word + 1]]) | bit;
                bm[word..word + 2].copy_from_slice(&v.to_be_bytes());
            }
            r.extend_from_slice(&bm);
        }
        while (10 + rec + r.len()) % 4 != 0 {
            r.push(0);
        }
        let ep = abs(r.len());
        r.extend_from_slice(&[0x08, 0x02]);
        r.extend_from_slice(&f.port.to_be_bytes());
        r.extend_from_slice(&cfg.ip.octets());
        let status = abs(r.len());
        let code = f.channels.first().and_then(|ch| codes.get(usize::from(*ch) - 1)).copied().unwrap_or(9);
        for v in [code, 0x0001, 0x0800, 0] {
            r.extend_from_slice(&v.to_be_bytes());
        }
        r.extend_from_slice(&(l.latency_us * 1000).to_be_bytes());
        r.extend_from_slice(&[0u8; 4]);
        put16(&mut r, ptrs, ep);
        for (k, p) in slot_ptrs.iter().enumerate() {
            put16(&mut r, ptrs + 2 + 2 * k, *p);
        }
        put16(&mut r, ptrs + 2 + 2 * slots, status);
        put16(&mut c, 2 + i * 2, (10 + rec) as u16);
        c.extend_from_slice(&r);
    }
    c
}

/// The daemon's current sample rate and encoding (one lock, released on return).
fn rate_enc(cfg: &Cfg) -> (u32, u16) {
    let l = *cfg.live.lock().unwrap();
    (l.rate, l.encoding as u16)
}

fn tx_flows_content(cfg: &Cfg, flows: &[Flow], unicast: &[AudioFlow]) -> Vec<u8> {
    let (live_rate, live_enc) = {
        let l = *cfg.live.lock().unwrap();
        (l.rate, l.encoding as u16)
    };
    let nm = flows.len().min(16);
    let nu = unicast.len().min(16);
    let n = nm + nu;
    let mut c = vec![0u8; 2 + n * 2];
    c[0] = n as u8;
    c[1] = n as u8;
    let w16 = |c: &mut Vec<u8>, v: u16| c.extend_from_slice(&v.to_be_bytes());
    for (i, f) in flows.iter().take(nm).enumerate() {
        let name_off = 10 + c.len();
        c.extend_from_slice(format!("{}_0", f.id).as_bytes());
        c.push(0);
        while (10 + c.len()) % 4 != 0 {
            c.push(0);
        }
        let sock_off = 10 + c.len();
        w16(&mut c, 0x8002);
        w16(&mut c, 4321);
        c.extend_from_slice(&[239, 255, cfg.ip.octets()[3], f.id as u8]);
        let names_off = 10 + c.len();
        w16(&mut c, 0x0A00);
        w16(&mut c, 1);
        w16(&mut c, 0); // remote hostname: none for multicast
        w16(&mut c, 0); // remote flow name: none
        w16(&mut c, 0x0010);
        w16(&mut c, name_off as u16);
        c.extend_from_slice(&[0u8; 8]);
        let main_off = 10 + c.len();
        w16(&mut c, f.id);
        w16(&mut c, 0x0002); // multicast
        c.extend_from_slice(&live_rate.to_be_bytes());
        w16(&mut c, 0);
        w16(&mut c, live_enc);
        w16(&mut c, 1);
        w16(&mut c, f.channels.len() as u16);
        w16(&mut c, sock_off as u16);
        for ch in &f.channels {
            w16(&mut c, *ch);
        }
        w16(&mut c, names_off as u16);
        put16(&mut c, 2 + i * 2, main_off as u16);
    }
    // Unicast flows set up by receivers (`control-arc.md` 4.9: type 0x0011, remote host and
    // remote flow name), numbered from 17 so they never collide with controller-made flows.
    for (j, f) in unicast.iter().take(nu).enumerate() {
        let flow_no = 17 + j as u16;
        let name_off = 10 + c.len();
        c.extend_from_slice(format!("{flow_no}_0").as_bytes());
        c.push(0);
        let host_off = 10 + c.len();
        c.extend_from_slice(f.peer_name.as_bytes());
        c.push(0);
        let rname_off = 10 + c.len();
        c.extend_from_slice(format!("{}_0", f.port).as_bytes());
        c.push(0);
        while (10 + c.len()) % 4 != 0 {
            c.push(0);
        }
        let sock_off = 10 + c.len();
        w16(&mut c, 0x8002);
        w16(&mut c, f.port);
        c.extend_from_slice(&f.peer.octets());
        let names_off = 10 + c.len();
        w16(&mut c, 0x0A00);
        w16(&mut c, 1);
        w16(&mut c, host_off as u16);
        w16(&mut c, rname_off as u16);
        w16(&mut c, 0x0010);
        w16(&mut c, name_off as u16);
        c.extend_from_slice(&[0u8; 8]);
        let main_off = 10 + c.len();
        w16(&mut c, flow_no);
        w16(&mut c, 0x0011); // unicast
        c.extend_from_slice(&live_rate.to_be_bytes());
        w16(&mut c, 0);
        w16(&mut c, live_enc);
        w16(&mut c, 1);
        w16(&mut c, f.channels.len() as u16);
        w16(&mut c, sock_off as u16);
        for ch in &f.channels {
            w16(&mut c, *ch);
        }
        w16(&mut c, names_off as u16);
        put16(&mut c, 2 + (nm + j) * 2, main_off as u16);
    }
    c
}

#[derive(Clone, Copy)]
enum ListKind {
    TxFactory, // 0x2000, 8-byte items
    TxFriendly, // 0x2010, 6-byte items
    Rx,        // 0x3000, 20-byte items
}

/// `control-arc.md` 4.4 to 4.6 and 6: an item list for the first page (all
/// channels fit), with the strings and the common descriptor after the items.
fn list_content(
    kind: ListKind,
    names: &[String],
    subs: &[Option<(String, String)>],
    codes: &[u16],
    rate: u32,
    enc: u16,
) -> Vec<u8> {
    let n = names.len();
    let slots = n.min(32);
    let item = match kind {
        ListKind::TxFactory => 8,
        ListKind::TxFriendly => 6,
        ListKind::Rx => 20,
    };
    let items_end = 2 + slots * item;
    let pre: usize = if matches!(kind, ListKind::TxFriendly) { 4 } else { 16 };
    let desc_off = 10 + items_end; // packet offsets count the 10-byte header
    let strings = items_end + pre;
    let mut c = vec![0u8; strings];
    c[0] = slots as u8;
    c[1] = n as u8;
    if pre == 16 {
        let d = items_end;
        c[d..d + 4].copy_from_slice(&rate.to_be_bytes());
        c[d + 4] = 1;
        c[d + 5] = 1;
        put16(&mut c, d + 6, enc);
        put16(&mut c, d + 8, 0x0400);
        put16(&mut c, d + 10, enc);
        put16(&mut c, d + 12, enc);
        put16(&mut c, d + 14, 0x000E);
    }
    let mut at = strings;
    for (i, name) in names.iter().enumerate() {
        let off = (10 + at) as u16;
        let id = (i + 1) as u16;
        let b = 2 + i * item;
        match kind {
            ListKind::TxFactory => {
                put16(&mut c, b, id);
                put16(&mut c, b + 2, 7);
                put16(&mut c, b + 4, desc_off as u16);
                put16(&mut c, b + 6, off);
            }
            ListKind::TxFriendly => {
                put16(&mut c, b, id);
                put16(&mut c, b + 2, id);
                put16(&mut c, b + 4, off);
            }
            ListKind::Rx => {
                put16(&mut c, b, id);
                put16(&mut c, b + 2, 6);
                put16(&mut c, b + 4, desc_off as u16);
                // b+6, b+8: subscribed TX name and device, 0 = not subscribed
                put16(&mut c, b + 10, off);
                // b+12: status 0 = no subscription; b+16: zero
            }
        }
        c.extend_from_slice(name.as_bytes());
        c.push(0);
        at += name.len() + 1;
        if let (ListKind::Rx, Some(Some((tn, th)))) = (kind, subs.get(i)) {
            let o1 = (10 + at) as u16;
            c.extend_from_slice(tn.as_bytes());
            c.push(0);
            at += tn.len() + 1;
            let o2 = (10 + at) as u16;
            c.extend_from_slice(th.as_bytes());
            c.push(0);
            at += th.len() + 1;
            put16(&mut c, b + 6, o1);
            put16(&mut c, b + 8, o2);
            // b+12: receiver status, b+14: subscription status. Receiving is 0x0101 + 0x0009
            // (`interop-fixes.md` F4: a real device reports 0x01010009 while it receives a flow).
            let code = codes.get(i).copied().unwrap_or(8);
            if code == 0x0009 {
                put16(&mut c, b + 12, 0x0101);
            }
            put16(&mut c, b + 14, code);
        }
    }
    c
}

/// Answers on the ARC port (`spec/protocol/control-arc.md`), the first three
/// opcodes only; everything else is logged and left unanswered so the next
/// request the controller makes shows up in the log.
fn arc_reply(req: &[u8], cfg: &Cfg) -> Option<(&'static str, Vec<u8>)> {
    if req.len() < 10 || be16(req, 8) != 0 {
        return None;
    }
    let mut st = cfg.st.lock().unwrap();
    match be16(req, 6) {
        // 4.1: counts and capabilities, 34 bytes.
        0x1000 => {
            let mut c = vec![0u8; 34];
            c[1] = 0x30; // TX channels renamable, multicast TX flows creatable
            put16(&mut c, 0x02, TX_CHANNELS);
            put16(&mut c, 0x04, RX_CHANNELS);
            put16(&mut c, 0x06, 4);
            put16(&mut c, 0x08, TX_CHANNELS.min(8));
            put16(&mut c, 0x0A, 8);
            put16(&mut c, 0x0C, 32);
            put16(&mut c, 0x0E, 32);
            put16(&mut c, 0x10, TX_CHANNELS + RX_CHANNELS);
            put16(&mut c, 0x12, 1);
            put16(&mut c, 0x14, 1);
            Some(("0x1000 counts", header(req, 1, &c)))
        }
        // 4.2: friendly device name, NUL-terminated.
        0x1002 => {
            let mut c = cfg.name.clone().into_bytes();
            c.push(0);
            Some(("0x1002 name", header(req, 1, &c)))
        }
        // 4.3: names block: 38 bytes of header, then the strings.
        0x1003 => {
            let (friendly, factory, board, rev) = (cfg.name.as_str(), cfg.name.as_str(), "tessera", ":705");
            let first = 10 + 38; // packet offset of the first string
            let o_friendly = first;
            let o_factory = o_friendly + friendly.len() + 1;
            let o_board = o_factory + factory.len() + 1;
            let o_rev = o_board + board.len() + 1;
            let mut c = vec![0u8; 38];
            put16(&mut c, 0x06, o_board as u16);
            put16(&mut c, 0x08, o_rev as u16);
            put16(&mut c, 0x0C, o_friendly as u16);
            put16(&mut c, 0x0E, o_factory as u16);
            put16(&mut c, 0x10, o_friendly as u16);
            put16(&mut c, 0x1E, 0x2729);
            put16(&mut c, 0x22, 0x1102);
            for s in [friendly, factory, board, rev] {
                c.extend_from_slice(s.as_bytes());
                c.push(0);
            }
            Some(("0x1003 names", header(req, 1, &c)))
        }
        // 5.1: set RX subscriptions (add or remove).
        // tessera-only flow request from a receiving probe (not a vendor message): content
        // u32 receiver address, u16 port, u8 count, then `count` NUL-terminated TX channel
        // names. Answer: one u16 TX channel number per name (0 = no such channel). Count 0
        // releases the flow to that address and port.
        FLOW_REQUEST => {
            let c = &req[10..];
            if c.len() < 7 {
                return Some(("flow request (short)", header(req, 0xFFFF, &[])));
            }
            let peer = Ipv4Addr::new(c[0], c[1], c[2], c[3]);
            let port = be16(c, 4);
            let mut names = Vec::new();
            let mut at = 7;
            for _ in 0..c[6] {
                let end = c[at..].iter().position(|b| *b == 0).map_or(c.len(), |e| at + e);
                names.push(String::from_utf8_lossy(&c[at..end]).into_owned());
                at = (end + 1).min(c.len());
            }
            let peer_name = c.get(at..).and_then(|t| t.split(|b| *b == 0).next()).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
            // After the name: the receiver's u32 rate and u16 encoding; a mismatch gets no flow.
            let fmt_at = at + peer_name.len() + 1;
            let rx_fmt = (c.len() >= fmt_at + 6).then(|| (u32::from_be_bytes([c[fmt_at], c[fmt_at + 1], c[fmt_at + 2], c[fmt_at + 3]]), be16(c, fmt_at + 4)));
            let mismatch = rx_fmt.is_some_and(|f| f != rate_enc(cfg));
            let nums: Vec<u16> = if mismatch {
                println!("{} arc FLOW REQUEST from {peer}:{port} refused: format {:?} vs this device {:?}", now(), rx_fmt, rate_enc(cfg));
                vec![0; names.len()]
            } else {
                names.iter().map(|n| st.tx.iter().position(|t| t == n).map_or(0, |i| (i + 1) as u16)).collect()
            };
            let id = format!("probe-tx-{}-{port}", peer.to_string().replace('.', "_"));
            let id_for_seen = id.clone();
            let before = st.tx_flows.clone();
            st.tx_flows.retain(|f| f.id != id);
            let channels: Vec<u16> = nums.iter().copied().filter(|n| *n != 0).collect();
            let has_channels = !channels.is_empty();
            if !channels.is_empty() {
                st.tx_flows.push(AudioFlow { id, tx: true, peer_name, channels, peer, port, fpp: 0 });
            }
            st.tx_flows.sort_by(|a, b| a.id.cmp(&b.id));
            if !has_channels {
                st.tx_seen.remove(&id_for_seen);
            } else {
                st.tx_seen.insert(id_for_seen, std::time::Instant::now());
            }
            if st.tx_flows != before {
                println!("{} arc FLOW REQUEST from {peer}:{port} names {names:?} -> {nums:?}", now());
                cfg.flows_dirty.store(true, Ordering::Relaxed);
            }
            let mut r = Vec::new();
            let (rate, enc) = rate_enc(cfg);
            r.extend_from_slice(&rate.to_be_bytes());
            r.extend_from_slice(&enc.to_be_bytes());
            for n in nums {
                r.extend_from_slice(&n.to_be_bytes());
            }
            Some(("flow request", header(req, 1, &r)))
        }
        0x3010 => {
            let c = &req[10..];
            if c.len() >= 2 {
                for i in 0..usize::from(c[1]) {
                    let at = 2 + i * 6;
                    if at + 6 > c.len() {
                        break;
                    }
                    let id = usize::from(be16(c, at));
                    if id == 0 || id > st.rx.len() {
                        continue;
                    }
                    let (no, ho) = (usize::from(be16(c, at + 2)), usize::from(be16(c, at + 4)));
                    let name = if no >= 10 { cstr_at(req, no) } else { None };
                    let host = if ho >= 10 { cstr_at(req, ho) } else { None };
                    match (name, host) {
                        (Some(n), Some(h)) => {
                            println!("{} arc SUBSCRIBE rx {id} <- {n:?} @ {h:?}", now());
                            st.subs[id - 1] = Some((n, h));
                            st.sub_codes[id - 1] = 8;
                            persist_sub(id, st.subs[id - 1].clone());
                        }
                        _ => {
                            println!("{} arc UNSUBSCRIBE rx {id}", now());
                            st.subs[id - 1] = None;
                            st.sub_codes[id - 1] = 0;
                            persist_sub(id, None);
                        }
                    }
                }
            }
            cfg.rx_dirty.store(true, Ordering::Relaxed);
            Some(("0x3010 subscribe", header(req, 1, &[])))
        }
        // 5.3: remove a subscription (alternative form): the channel id is the u16 at content +4.
        0x3014 => {
            let c = &req[10..];
            if c.len() >= 6 {
                let id = usize::from(be16(c, 4));
                if (1..=st.subs.len()).contains(&id) {
                    println!("{} arc UNSUBSCRIBE rx {id} (0x3014)", now());
                    st.subs[id - 1] = None;
                    st.sub_codes[id - 1] = 0;
                    cfg.rx_dirty.store(true, Ordering::Relaxed);
                    persist_sub(id, None);
                }
            }
            Some(("0x3014 unsubscribe", header(req, 1, &[])))
        }
        // 4.7: rename TX channels.
        0x2013 => {
            let c = &req[10..];
            let mut applied = 0;
            if c.len() >= 2 {
                for i in 0..usize::from(c[1]) {
                    let at = 2 + i * 6;
                    if at + 6 > c.len() {
                        break;
                    }
                    let id = usize::from(be16(c, at + 2));
                    if let Some(name) = cstr_at(req, usize::from(be16(c, at + 4))) {
                        if (1..=st.tx.len()).contains(&id) && !name.is_empty() {
                            println!("{} arc RENAME tx {id} -> {name:?}", now());
                            st.tx[id - 1] = name;
                            applied += 1;
                        }
                    }
                }
            }
            if applied > 0 {
                Some(("0x2013 rename tx", header(req, 1, &[0, 0])))
            } else {
                Some(("0x2013 rename tx (nothing)", header(req, 0xFFFF, &[])))
            }
        }
        // 4.8: rename RX channels.
        0x3001 => {
            let c = &req[10..];
            let mut applied = 0;
            if c.len() >= 2 {
                for i in 0..usize::from(c[1]) {
                    let at = 2 + i * 4;
                    if at + 4 > c.len() {
                        break;
                    }
                    let id = usize::from(be16(c, at));
                    if let Some(name) = cstr_at(req, usize::from(be16(c, at + 2))) {
                        if (1..=st.rx.len()).contains(&id) && !name.is_empty() {
                            println!("{} arc RENAME rx {id} -> {name:?}", now());
                            st.rx[id - 1] = name;
                            applied += 1;
                        }
                    }
                }
            }
            if applied > 0 {
                Some(("0x3001 rename rx", header(req, 1, &[])))
            } else {
                Some(("0x3001 rename rx (nothing)", header(req, 0xFFFF, &[])))
            }
        }
        // 4.10: create multicast TX flow.
        0x2201 => {
            let c = &req[10..];
            let mut created = Vec::new();
            if c.len() >= 2 {
                for i in 0..usize::from(c[1]) {
                    let at = 2 + i * 2;
                    if at + 2 > c.len() {
                        break;
                    }
                    let Some(d) = usize::from(be16(c, at)).checked_sub(10) else {
                        continue;
                    };
                    if d + 16 > c.len() {
                        continue;
                    }
                    let (id, kind, n) = (be16(c, d), be16(c, d + 2), usize::from(be16(c, d + 14)));
                    if kind != 2 || d + 16 + n * 2 > c.len() || st.flows.iter().any(|f| f.id == id) {
                        continue;
                    }
                    let channels: Vec<u16> = (0..n).map(|k| be16(c, d + 16 + k * 2)).collect();
                    println!("{} arc CREATE multicast flow {id} channels {channels:?}", now());
                    st.flows.push(Flow { id, channels });
                    created.push(id);
                }
            }
            if created.is_empty() {
                Some(("0x2201 create flow (refused)", header(req, 0xFFFF, &[])))
            } else {
                let mut r = Vec::new();
                r.extend_from_slice(&(created.len() as u16).to_be_bytes());
                r.extend_from_slice(&0u16.to_be_bytes());
                for id in created {
                    r.extend_from_slice(&id.to_be_bytes());
                }
                Some(("0x2201 create flow", header(req, 1, &r)))
            }
        }
        // 4.11: delete multicast TX flow.
        0x2202 => {
            let c = &req[10..];
            let before = st.flows.len();
            if c.len() >= 4 {
                for k in 0..usize::from(be16(c, 0)) {
                    if 4 + k * 2 + 2 > c.len() {
                        break;
                    }
                    let id = be16(c, 4 + k * 2);
                    println!("{} arc DELETE flow {id}", now());
                    st.flows.retain(|f| f.id != id);
                }
            }
            let ok = st.flows.len() < before;
            Some(("0x2202 delete flow", header(req, if ok { 1 } else { 0xFFFF }, &[])))
        }
        // 4.4, 4.5, 4.6: channel lists (first page only: every list fits).
        0x2000 => {
            let names = st.tx.clone();
            Some(("0x2000 tx list", header(req, 1, &list_content(ListKind::TxFactory, &names, &[], &[], rate_enc(cfg).0, rate_enc(cfg).1))))
        }
        0x2010 => {
            let names = st.tx.clone();
            Some(("0x2010 tx names", header(req, 1, &list_content(ListKind::TxFriendly, &names, &[], &[], rate_enc(cfg).0, rate_enc(cfg).1))))
        }
        0x3000 => {
            let names = st.rx.clone();
            Some(("0x3000 rx list", header(req, 1, &list_content(ListKind::Rx, &names, &st.subs, &st.sub_codes, rate_enc(cfg).0, rate_enc(cfg).1))))
        }
        // 4.9, 4.13: flow queries; no flows yet, so empty lists (0 slots, 0 items).
        0x2200 => Some(("0x2200 tx flows", header(req, 1, &tx_flows_content(cfg, &st.flows, &st.tx_flows)))),
        0x3200 => Some(("0x3200 rx flows", header(req, 1, &rx_flows_content(cfg, &st.rx_flows, &st.sub_codes)))),
        // NOT in the spec: the controller asks it right after 0x2200; same request shape.
        0x2204 => Some(("0x2204 (unknown, empty list)", header(req, 1, &[0, 0]))),
        // 4.12: stubbed opcodes, answered as the reference does.
        // NOT in the spec: appears once the capability tables claim support; try success + empty.
        0x3320 => Some(("0x3320 (unknown, empty list)", header(req, 1, &[0, 0]))),
        // Observed 2026-10-16 (not in the spec): the controller WRITES properties with
        // 0x1101, in the same list structure as the 0x1100 answer: (slots, count),
        // 4-byte items (id, inline u16 or packet offset to a u32), then the u32 values.
        // Store them and answer with the 0x1100-style list of the new values.
        0x1101 => {
            let c = &req[10..];
            let mut ids = Vec::new();
            if c.len() >= 2 {
                for k in 0..usize::from(c[1]) {
                    let at = 2 + k * 4;
                    if at + 4 > c.len() {
                        break;
                    }
                    let (id, v) = (be16(c, at), be16(c, at + 2));
                    if id == 0 {
                        continue;
                    }
                    let value = if id & 0x8000 != 0 {
                        let off = usize::from(v);
                        match req.get(off..off + 4) {
                            Some(b) => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
                            None => continue,
                        }
                    } else {
                        u32::from(v)
                    };
                    println!("{} arc WRITE property {id:04x} = {value}", now());
                    if id == 0x8205 {
                        // Device latency (ns) -> the daemon (microseconds).
                        let us = (value / 1000).to_string();
                        let r = std::process::Command::new("tesseractl").args(["set-latency", &us]).output();
                        if r.as_ref().is_ok_and(|o| o.status.success()) {
                            cfg.live.lock().unwrap().latency_us = value / 1000;
                        }
                        println!("{} arc LATENCY -> tesseractl set-latency {us}: {:?}", now(), r.map(|o| (o.status, String::from_utf8_lossy(&o.stderr).trim().to_owned())));
                    }
                    if id == 0x0022 || id == 0x8021 {
                        // Candidate "preferred leader" properties (writable in 0x1102):
                        // preferred = priority below 64 (PTPv1 preferred flag).
                        let prio = if value != 0 { "10" } else { "128" };
                        let r = std::process::Command::new("tesseractl").args(["clock", "set-priority", prio]).output();
                        println!("{} arc PREFERRED? property {id:04x} = {value} -> tesseractl clock set-priority {prio}: {:?}", now(), r.map(|o| o.status));
                    }
                    st.written.retain(|(i, _)| *i != id);
                    st.written.push((id, value));
                    ids.push(id);
                }
            }
            let r = props_list(cfg, &st.written, &ids);
            Some(("0x1101 write properties", header(req, 1, &r)))
        }
        // `control-arc.md` 0.5: the real-device structure. Items in request order;
        // supported ids carry an inline u16, or (bit 0x8000) a packet offset to a u32
        // after the items; unsupported ids are answered as (0x0000, id).
        0x1100 if cfg.real_props => {
            let c = &req[10..];
            let n = if c.len() >= 2 { usize::from(be16(c, 0)) } else { 0 };
            let ids: Vec<u16> = (0..n)
                .filter_map(|k| c.get(2 + k * 2..4 + k * 2).map(|b| u16::from_be_bytes([b[0], b[1]])))
                .collect();
            let data_start = 10 + 2 + 4 * ids.len();
            let (mut items, mut data) = (Vec::new(), Vec::new());
            for id in &ids {
                if !PROP_TABLE.iter().any(|(i, _)| i == id) {
                    items.extend_from_slice(&[0, 0]);
                    items.extend_from_slice(&id.to_be_bytes());
                } else if id & 0x8000 != 0 {
                    items.extend_from_slice(&id.to_be_bytes());
                    items.extend_from_slice(&((data_start + data.len()) as u16).to_be_bytes());
                    data.extend_from_slice(&prop_value(cfg, &st.written, *id).to_be_bytes());
                } else {
                    items.extend_from_slice(&id.to_be_bytes());
                    items.extend_from_slice(&(prop_value(cfg, &st.written, *id) as u16).to_be_bytes());
                }
            }
            let mut r = vec![ids.len() as u8, ids.len() as u8];
            r.extend_from_slice(&items);
            r.extend_from_slice(&data);
            Some(("0x1100 properties (real structure)", header(req, 1, &r)))
        }
        0x1102 if cfg.real_props => {
            let mut r = (PROP_TABLE.len() as u16).to_be_bytes().to_vec();
            for (id, mask) in PROP_TABLE {
                r.extend_from_slice(&id.to_be_bytes());
                r.extend_from_slice(&mask.to_be_bytes());
            }
            Some(("0x1102 supported properties (real table)", header(req, 1, &r)))
        }
        // Experiment (`control-arc.md` 4.12): the list layout (slots, count, 4-byte items
        // of (id, value-or-pointer)) followed by u32 values, as a real device answers.
        0x1100 if !cfg.lat_ids.is_empty() => {
            let c = &req[10..];
            let n = if c.len() >= 2 { usize::from(be16(c, 0)) } else { 0 };
            let ids: Vec<u16> = (0..n)
                .filter_map(|k| c.get(2 + k * 2..4 + k * 2).map(|b| u16::from_be_bytes([b[0], b[1]])))
                .collect();
            let area = (10 + 2 + 4 * ids.len()) as u16; // packet offset of the u32 values
            let mut r = vec![ids.len() as u8, ids.len() as u8];
            for id in &ids {
                r.extend_from_slice(&id.to_be_bytes());
                let v = if cfg.lat_ids.contains(id) { area } else { cfg.cap_value };
                r.extend_from_slice(&v.to_be_bytes());
            }
            r.extend_from_slice(&cfg.latency_ns.to_be_bytes());
            r.extend_from_slice(&cfg.latency_ns.to_be_bytes());
            r.extend_from_slice(&0x0135_F1B4u32.to_be_bytes());
            Some(("0x1100 properties (latency pointers)", header(req, 1, &r)))
        }
        0x1100 if cfg.cap_value != 0 => {
            // Experiment: echo every property id the controller asks about with a value.
            let c = &req[10..];
            let n = if c.len() >= 2 { usize::from(be16(c, 0)) } else { 0 };
            let mut r = Vec::new();
            r.extend_from_slice(&(n as u16).to_be_bytes());
            for k in 0..n {
                if 2 + k * 2 + 2 > c.len() {
                    break;
                }
                r.extend_from_slice(&c[2 + k * 2..4 + k * 2]);
                r.extend_from_slice(&cfg.cap_value.to_be_bytes());
            }
            Some(("0x1100 properties", header(req, 1, &r)))
        }
        0x1102 if cfg.cap_value != 0 => {
            // Experiment: the 23 property ids a real device reported (`control-arc.md` 4.12).
            const IDS: [u16; 23] = [
                0x8020, 0x8021, 0x0022, 0x0023, 0x0024, 0x0201, 0x8204, 0x8205, 0x020A, 0x020B,
                0x0210, 0x0211, 0x0212, 0x0213, 0x0214, 0x8301, 0x8306, 0x8302, 0x0310, 0x0311,
                0x0303, 0x83F0, 0x0601,
            ];
            let mut r = Vec::new();
            r.extend_from_slice(&(IDS.len() as u16).to_be_bytes());
            for id in IDS {
                r.extend_from_slice(&id.to_be_bytes());
                r.extend_from_slice(&cfg.cap_value.to_be_bytes());
            }
            Some(("0x1102 properties", header(req, 1, &r)))
        }
        0x1100 => Some(("0x1100 stub", header(req, 1, &[0u8; 110]))),
        0x1102 => Some(("0x1102 stub", header(req, 1, &[0u8; 94]))),
        0x2320 => Some(("0x2320 stub", header(req, 0x0030, &[]))),
        0x3300 => Some((
            "0x3300 rx port ranges",
            // The ranges a real device reported (`controller-netaudio.md` 3.4); tessera flows use
            // FLOW_PORT_BASE + sender octet, inside the first range.
            header(req, 1, &[0x38, 0x00, 0x39, 0x7F, 0x39, 0x80, 0x39, 0xFF]),
        )),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Cmc,
    Arc,
    Info,
    Flow,
    LogOnly,
}

fn bind_retry(name: &str, bind: SocketAddr) -> UdpSocket {
    let mut logged = false;
    loop {
        match UdpSocket::bind(bind) {
            Ok(s) => return s,
            Err(e) => {
                if !logged {
                    println!("{} {name} cannot bind {bind} yet ({e}); retrying", now());
                    logged = true;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

fn serve(name: &'static str, bind: SocketAddr, role: Role, cfg: Arc<Cfg>) {
    // At boot the address may not exist yet: retry until it does.
    let sock = bind_retry(name, bind);
    println!("{} {name} listening on {bind}", now());
    if role == Role::Info {
        pin_multicast_to(&sock, cfg.ip);
        let tx = sock.try_clone().unwrap();
        let c = Arc::clone(&cfg);
        std::thread::spawn(move || {
            // The reference sends board and product info once at start, then a
            // heartbeat every second.
            for (what, m) in [
                ("board info", conmon(&c, 0xFFFF, opcode(0x0060), &board_info(&c))),
                ("product info", conmon(&c, 0xFFFF, opcode(0x00C0), &product_info())),
            ] {
                let _ = tx.send_to(&m, INFO_GROUP);
                println!("{} info TX startup {what} -> {}:{} {}B", now(), INFO_GROUP.0, INFO_GROUP.1, m.len());
            }
            let mut n = 0u64;
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let hb = heartbeat(&c);
                let _ = tx.send_to(&hb, HB_GROUP);
                n += 1;
                if n <= 3 || n % 60 == 0 {
                    println!("{} info TX heartbeat #{n} -> {}:{} {}", now(), HB_GROUP.0, HB_GROUP.1, describe(&hb));
                }
            }
        });
    }
    let mut buf = [0u8; 2048];
    loop {
        let Ok((n, from)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let req = &buf[..n];
        println!("{} {name} RX {from} -> :{} {n}B {}", now(), bind.port(), describe(req));
        match role {
            Role::Cmc => match cmc_reply(req, &cfg) {
                Some(r) => {
                    let _ = sock.send_to(&r, from);
                    println!("{} {name} TX {from} {}B {}", now(), r.len(), describe(&r));
                }
                None => println!("{} {name} UNHANDLED (no reply)", now()),
            },
            Role::Arc => match arc_reply(req, &cfg) {
                Some((what, r)) => {
                    let _ = sock.send_to(&r, from);
                    println!("{} {name} TX {what} {from} {}B {}", now(), r.len(), describe(&r));
                }
                None => println!("{} {name} UNHANDLED (no reply)", now()),
            },
            Role::Flow => match flowctl::serve_request(req, from, &cfg) {
                Some(r) => {
                    let _ = sock.send_to(&r, from);
                    println!("{} {name} TX {from} {}B {}", now(), r.len(), hex(&r));
                }
                None => println!("{} {name} UNHANDLED (no reply)", now()),
            },
            Role::Info => match info_reply(req, &cfg) {
                Some((what, r)) => {
                    let _ = sock.send_to(&r, INFO_GROUP);
                    // Experiment: also answer the requester directly.
                    let _ = sock.send_to(&r, from);
                    println!(
                        "{} {name} TX {what} -> {}:{} {}B {}",
                        now(),
                        INFO_GROUP.0,
                        INFO_GROUP.1,
                        r.len(),
                        describe(&r)
                    );
                }
                None => println!("{} {name} UNHANDLED (no reply)", now()),
            },
            Role::LogOnly => {}
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let ip: Ipv4Addr = get("--ip").expect("--ip").parse().expect("ip");
    let id = get("--device-id").expect("--device-id");
    let mac_s = get("--mac").expect("--mac");
    let mut device_id = [0u8; 8];
    for (i, b) in device_id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&id[i * 2..i * 2 + 2], 16).expect("hex device id");
    }
    // `--vendor` text, first 8 bytes, padded with spaces (default "tessera ").
    let mut vendor = *b"tessera ";
    if let Some(v) = get("--vendor") {
        vendor = [b' '; 8];
        for (d, s) in vendor.iter_mut().zip(v.bytes()) {
            *d = s;
        }
    }
    println!("{} vendor field = {:?}", now(), String::from_utf8_lossy(&vendor));
    // `--flags` 8 hex digits for content bytes 0x14..0x17 (default 00001000).
    let flag_s = get("--flags").unwrap_or_else(|| "00001000".into());
    let mut flags = [0u8; 4];
    for (i, b) in flags.iter_mut().enumerate() {
        *b = u8::from_str_radix(&flag_s[i * 2..i * 2 + 2], 16).expect("hex flags");
    }
    println!("{} board-info flags = {}", now(), flag_s);
    let sub_status = u32::from_str_radix(&get("--sub-status").unwrap_or_else(|| "00000001".into()), 16).expect("hex sub-status");
    println!("{} subscription status = {sub_status:08x}", now());
    let latency_ns: u32 = get("--latency-ns").map_or(0, |v| v.parse().expect("--latency-ns N"));
    println!("{} latency reported in 0x1007 = {latency_ns} ns", now());
    let lat_ids: Vec<u16> = get("--lat-ids").map_or_else(Vec::new, |v| {
        v.split(',').map(|s| u16::from_str_radix(s, 16).expect("hex id")).collect()
    });
    println!("{} latency property ids = {lat_ids:04x?}", now());
    let real_props = args.iter().any(|a| a == "--real-props");
    let props: Vec<(u16, u32)> = get("--props").map_or_else(Vec::new, |v| {
        v.split(',')
            .map(|kv| {
                let (k, val) = kv.split_once('=').expect("--props ID=VALUE");
                (u16::from_str_radix(k, 16).expect("hex id"), val.parse().expect("decimal value"))
            })
            .collect()
    });
    println!("{} real property structure = {real_props}, overrides = {props:04x?}", now());
    let clock_state: [u16; 2] = get("--clock-state").map_or([3, 3], |v| {
        let mut it = v.split(',').map(|s| s.parse::<u16>().expect("--clock-state P,S"));
        [it.next().unwrap_or(3), it.next().unwrap_or(3)]
    });
    println!("{} clock state words = {clock_state:?}", now());
    let cap_value: u16 = get("--cap-value").map_or(0, |v| v.parse().expect("--cap-value N"));
    println!("{} capability value = {cap_value}", now());
    let mut mac = [0u8; 6];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = u8::from_str_radix(&mac_s[i * 2..i * 2 + 2], 16).expect("hex mac");
    }
    // Clock master reported in clock stats: this device (EUI-64 from its MAC) unless `--gm`.
    let mut gm = [mac[0], mac[1], mac[2], 0xFF, 0xFE, mac[3], mac[4], mac[5]];
    if let Some(g) = get("--gm") {
        for (i, b) in gm.iter_mut().enumerate() {
            *b = u8::from_str_radix(&g[i * 2..i * 2 + 2], 16).expect("hex gm");
        }
    }
    println!("{} clock master = {}", now(), hex(&gm));
    let cfg = Arc::new(Cfg {
        ip,
        device_id,
        mac,
        info_port: 8700,
        netmask: Ipv4Addr::new(255, 255, 255, 0),
        gateway: Ipv4Addr::new(ip.octets()[0], ip.octets()[1], ip.octets()[2], 1),
        vendor,
        name: get("--name").unwrap_or_else(|| "tessera-probe".into()),
        flags,
        st: std::sync::Mutex::new(State {
            tx: (1..=TX_CHANNELS).map(|i| format!("TX {i}")).collect(),
            rx: (1..=RX_CHANNELS).map(|i| format!("RX {i}")).collect(),
            flows: Vec::new(),
            subs: vec![None; RX_CHANNELS as usize],
            written: Vec::new(),
            sub_codes: vec![0; RX_CHANNELS as usize],
            peers: Vec::new(),
            rx_flows: Vec::new(),
            tx_flows: Vec::new(),
            tx_seen: std::collections::HashMap::new(),
            tx_handles: std::collections::HashMap::new(),
            tx_real: std::collections::HashSet::new(),
            next_slot: 0,
            real_plans: std::collections::HashMap::new(),
        }),
        sub_status,
        cap_value,
        gm,
        latency_ns,
        lat_ids,
        real_props,
        props,
        clock_state,
        live: std::sync::Mutex::new(Live { ok: false, state_word: 3, gm, ppb: 0, priority: 128, rate: 48_000, encoding: 24, latency_us: 10_000 }),
        stats: std::sync::Mutex::new(Stats::default()),
        meter: std::sync::Mutex::new(Vec::new()),
        rx_levels: std::sync::Mutex::new(vec![253; RX_CHANNELS as usize]),
        gm_mac_form: get("--gm-form").is_some_and(|f| f == "mac"),
        rx_dirty: AtomicBool::new(false),
        flows_dirty: AtomicBool::new(false),
    });
    {
        let c = Arc::clone(&cfg);
        std::thread::spawn(move || poll_daemon(c));
    }
    {
        let c = Arc::clone(&cfg);
        std::thread::spawn(move || flow_worker(c));
    }
    {
        let c = Arc::clone(&cfg);
        std::thread::spawn(move || meter_worker(c));
    }
    let any = Ipv4Addr::UNSPECIFIED;
    let jobs: [(&'static str, SocketAddr, Role); 5] = [
        ("cmc", SocketAddr::from((ip, 8800)), Role::Cmc),
        ("arc", SocketAddr::from((ip, 4440)), Role::Arc),
        ("flow", SocketAddr::from((ip, 4455)), Role::Flow),
        ("info", SocketAddr::from((ip, 8700)), Role::Info),
        ("bcast", SocketAddr::from((any, 10670)), Role::LogOnly),
    ];
    let handles: Vec<_> = jobs
        .into_iter()
        .map(|(n, a, r)| {
            let c = Arc::clone(&cfg);
            std::thread::spawn(move || serve(n, a, r, c))
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }
}
