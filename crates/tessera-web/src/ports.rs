//! Router ports and crosspoints as the portal's forms and pages carry them:
//! one form value per port (`card_in/15/hw%3ACARD%3DCQ%2CDEV%3D0`) or
//! crosspoint (`<source>><destination>`), element ids that are safe in
//! HTML and fragments, and the ports in words.
//!
//! The router itself is the daemon's (`Query::Router`, the `Set*` router
//! commands); nothing here decides what a port is or whether it exists.

use tessera_control_types::{
    Channel, ChannelSet, CrosspointKey, CrosspointOrigin, CrosspointStatus, DeviceKey, Direction,
    PortStatus, Query, Reply, RequestBody, RouterPort, RouterReport, Status,
};
use tesseractl::client::ClientError;

use crate::pages::path_segment;
use crate::{AppState, call};

/// The router as the daemon reports it.
///
/// # Errors
/// The daemon's refusal (an older daemon: `Unsupported`) or a transport
/// error.
pub async fn router_of(st: &AppState) -> Result<RouterReport, ClientError> {
    match call(st, RequestBody::Query(Query::Router)).await {
        Ok(Reply::Router(r)) => Ok(r),
        Ok(other) => Err(ClientError::Protocol(format!(
            "unexpected reply: {other:?}"
        ))),
        Err(e) => Err(e),
    }
}

/// Percent-decode what [`path_segment`] encoded.
fn unescape(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// One port as one form value: `kind/channel/key`, the key (device or
/// card) percent-encoded so it never holds `/` or `>`.
#[must_use]
pub fn encode(p: &RouterPort) -> String {
    let key = p
        .device()
        .map(|d| d.0.as_str())
        .or_else(|| p.card())
        .unwrap_or_default();
    format!("{}/{}/{}", p.kind(), p.channel().get(), path_segment(key))
}

/// The port a form value names; `None` for anything else.
#[must_use]
pub fn decode(s: &str) -> Option<RouterPort> {
    let mut it = s.trim().splitn(3, '/');
    let kind = it.next()?;
    let channel = it.next()?.parse::<u16>().ok().and_then(Channel::new)?;
    let key = unescape(it.next()?)?;
    if key.is_empty() {
        return None;
    }
    let device = || DeviceKey(key.clone());
    Some(match kind {
        "network_rx" => RouterPort::NetworkRx {
            device: device(),
            channel,
        },
        "network_tx" => RouterPort::NetworkTx {
            device: device(),
            channel,
        },
        "app_out" => RouterPort::AppOut {
            device: device(),
            channel,
        },
        "host_out" => RouterPort::HostOut {
            device: device(),
            channel,
        },
        "card_in" => RouterPort::CardIn {
            card: key.clone(),
            channel,
        },
        "card_out" => RouterPort::CardOut {
            card: key.clone(),
            channel,
        },
        _ => return None,
    })
}

/// One crosspoint as one form value: `<source>><destination>`.
#[must_use]
pub fn encode_xp(k: &CrosspointKey) -> String {
    format!("{}>{}", encode(&k.source), encode(&k.destination))
}

/// The crosspoint a form value names.
#[must_use]
pub fn decode_xp(s: &str) -> Option<CrosspointKey> {
    let (a, b) = s.split_once('>')?;
    Some(CrosspointKey::new(decode(a)?, decode(b)?))
}

/// FNV-1a, for ids that stay distinct when two keys slug alike.
fn fnv(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Letters and digits of `s`, lower case, other runs as one `-`.
fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').chars().take(24).collect()
}

/// An element id for a port (letters, digits, `-`): stable while the port
/// exists, distinct for distinct ports.
#[must_use]
pub fn port_id(p: &RouterPort) -> String {
    let v = encode(p);
    format!(
        "p-{}-{}-{}-{:06x}",
        p.kind().replace('_', ""),
        slug(p.device().map(|d| d.0.as_str()).or(p.card()).unwrap_or("")),
        p.channel().get(),
        fnv(&v) & 0x00ff_ffff
    )
}

/// An element id for a crosspoint.
#[must_use]
pub fn xp_id(k: &CrosspointKey) -> String {
    format!(
        "x-{:08x}-{:08x}",
        fnv(&encode(&k.source)),
        fnv(&encode(&k.destination))
    )
}

/// The short name of a sound card from its ALSA PCM: `hw:CARD=CQ,DEV=0`
/// → `CQ`, `hw:1,0` → `1`; anything else as written. The daemon names
/// card ports the same way (its default labels, `CQ in 15`).
#[must_use]
pub fn card_short(card: &str) -> &str {
    let Some((_, args)) = card.split_once(':') else {
        return card;
    };
    let first = args.split(',').next().unwrap_or("").trim();
    let first = first.strip_prefix("CARD=").unwrap_or(first);
    if first.is_empty() { card } else { first }
}

/// What a port's kind is, as a group heading.
#[must_use]
pub fn kind_title(p: &RouterPort) -> &'static str {
    match p {
        RouterPort::NetworkRx { .. } => "Network inputs (RX)",
        RouterPort::NetworkTx { .. } => "Network outputs (TX)",
        RouterPort::AppOut { .. } => "App playback",
        RouterPort::HostOut { .. } => "Host outputs",
        RouterPort::CardIn { .. } => "Card inputs",
        RouterPort::CardOut { .. } => "Card outputs",
        _ => "Other",
    }
}

/// Where a port is, in plain words: `Network input 2 (RX)`, `Network
/// output 1 (TX)`, `App playback 1`, `Host output 2`, `CQ input 15`,
/// `CQ output 3`. The device is not named (one per host by default); see
/// [`place_on`] where several devices are shown together.
#[must_use]
pub fn place(p: &RouterPort) -> String {
    let n = p.channel().get();
    match p {
        RouterPort::NetworkRx { .. } => format!("Network input {n} (RX)"),
        RouterPort::NetworkTx { .. } => format!("Network output {n} (TX)"),
        RouterPort::AppOut { .. } => format!("App playback {n}"),
        RouterPort::HostOut { .. } => format!("Host output {n}"),
        RouterPort::CardIn { card, .. } => format!("{} input {n}", card_short(card)),
        RouterPort::CardOut { card, .. } => format!("{} output {n}", card_short(card)),
        _ => format!("Port {n}"),
    }
}

/// [`place`] with the device's name (not its key) in front when the
/// status lists more than one device: `studio: Network input 2 (RX)`.
#[must_use]
pub fn place_on(p: &RouterPort, status: Option<&Status>) -> String {
    let words = place(p);
    match (p.device(), status) {
        (Some(d), Some(s)) if s.devices.len() > 1 => {
            let name = s
                .devices
                .iter()
                .find(|x| &x.key == d)
                .map_or(d.0.as_str(), |x| x.name.as_str());
            format!("{name}: {words}")
        }
        _ => words,
    }
}

/// Did someone name this port (a channel label, a saved port label)? The
/// daemon says so (`custom_label`); a default label is no name.
#[must_use]
pub fn named(s: &PortStatus) -> bool {
    s.custom_label && !s.label.trim().is_empty()
}

/// The port's status in the report, if listed.
#[must_use]
pub fn status<'a>(r: &'a RouterReport, p: &RouterPort) -> Option<&'a PortStatus> {
    r.ports.iter().find(|s| &s.port == p)
}

/// The name a card input carries on the network: the label of the
/// network output (TX) channel it feeds, when that channel is named.
#[must_use]
pub fn network_name(r: &RouterReport, p: &RouterPort) -> Option<String> {
    if !matches!(p, RouterPort::CardIn { .. }) {
        return None;
    }
    r.crosspoints
        .iter()
        .filter(|x| &x.crosspoint.source == p)
        .filter(|x| matches!(x.crosspoint.destination, RouterPort::NetworkTx { .. }))
        .find_map(|x| status(r, &x.crosspoint.destination).filter(|s| named(s)))
        .map(|s| s.label.clone())
}

/// A port's name: the label someone gave it; else, for a card input that
/// feeds a named network channel, `CQ input 15 → CQ Main L`; else where it
/// is ([`place`]).
#[must_use]
pub fn label(r: Option<&RouterReport>, p: &RouterPort) -> String {
    let Some(r) = r else {
        return place(p);
    };
    if let Some(s) = status(r, p).filter(|s| named(s)) {
        return s.label.clone();
    }
    match network_name(r, p) {
        Some(n) => format!("{} \u{2192} {n}", place(p)),
        None => place(p),
    }
}

/// A port's own name, without where it feeds: the label someone gave it,
/// else [`place`]. For lists of crosspoints, where the destination is
/// named next to it.
#[must_use]
pub fn short(r: Option<&RouterReport>, p: &RouterPort) -> String {
    r.and_then(|r| status(r, p))
        .filter(|s| named(s))
        .map_or_else(|| place(p), |s| s.label.clone())
}

/// A port in a list of ports: its name, and where it is when someone
/// named it (`Speaker R (Host output 2)`, `Host output 1`).
#[must_use]
pub fn described(r: &RouterReport, s: &PortStatus) -> String {
    if named(s) {
        format!("{} ({})", s.label, place(&s.port))
    } else {
        label(Some(r), &s.port)
    }
}

/// A port's name for a listed status ([`label`]).
#[must_use]
pub fn name(r: &RouterReport, s: &PortStatus) -> String {
    label(Some(r), &s.port)
}

/// The crosspoint's status in the report, if any.
#[must_use]
pub fn crosspoint<'a>(r: &'a RouterReport, k: &CrosspointKey) -> Option<&'a CrosspointStatus> {
    r.crosspoints
        .iter()
        .find(|x| x.crosspoint.source == k.source && x.crosspoint.destination == k.destination)
}

/// `source → destination`, by labels.
#[must_use]
pub fn xp_words(r: Option<&RouterReport>, k: &CrosspointKey) -> String {
    format!(
        "{} \u{2192} {}",
        short(r, &k.source),
        short(r, &k.destination)
    )
}

/// A crosspoint a network card bridge implies (read-only in the grid).
#[must_use]
pub const fn is_bridge(o: &CrosspointOrigin) -> bool {
    matches!(o, CrosspointOrigin::Bridge { .. })
}

/// The origin as a short badge and its words.
#[must_use]
pub fn origin_words(o: &CrosspointOrigin) -> (&'static str, String) {
    match o {
        CrosspointOrigin::Saved => ("saved", "saved in the grid".into()),
        CrosspointOrigin::Default => ("default", "default grid".into()),
        CrosspointOrigin::Configured => ("configured", "a [[route]] of the configuration".into()),
        CrosspointOrigin::Monitor => ("monitor", "local monitoring".into()),
        CrosspointOrigin::Bridge { name } => ("bridge", format!("card bridge `{name}`")),
        _ => ("other", "other".into()),
    }
}

/// Channel labels in a few words: one as it is; two that differ only in
/// their last word as `CQ Main L–R`; more that differ only in a short last
/// word as `CQ Ch 1/2/3`; else up to three joined, or the first and last.
fn labels_summary(labels: &[&str]) -> String {
    match labels {
        [] => String::new(),
        [one] => (*one).to_owned(),
        _ => {
            let words: Vec<Vec<&str>> = labels.iter().map(|l| l.split(' ').collect()).collect();
            let mut common = 0;
            while words
                .iter()
                .all(|w| w.len() > common + 1 && w[common] == words[0][common])
            {
                common += 1;
            }
            let prefix = words[0][..common].join(" ");
            let rests: Vec<String> = words.iter().map(|w| w[common..].join(" ")).collect();
            let short = rests
                .iter()
                .all(|r| !r.contains(' ') && r.chars().count() <= 3);
            if common > 0 && rests.len() == 2 && short {
                format!("{prefix} {}\u{2013}{}", rests[0], rests[1])
            } else if common > 0 && short {
                format!("{prefix} {}", rests.join("/"))
            } else if labels.len() <= 3 {
                labels.join(", ")
            } else {
                format!("{} \u{2026} {}", labels[0], labels[labels.len() - 1])
            }
        }
    }
}

/// The device a sentence names, in the portal's words: its name when the
/// host has several devices, `None` on a host with one (the host *is* the
/// network device there). Never its configuration key.
#[must_use]
pub fn device_name<'a>(
    devices: &'a [tessera_control_types::DeviceStatus],
    key: &DeviceKey,
) -> Option<&'a str> {
    if devices.len() > 1 {
        devices
            .iter()
            .find(|d| d.key == *key)
            .map(|d| d.name.as_str())
    } else {
        None
    }
}

/// The subject of a sentence about a device: its name on a host with
/// several devices, `This host` on a host with one.
#[must_use]
pub fn device_subject(devices: &[tessera_control_types::DeviceStatus], key: &DeviceKey) -> String {
    device_name(devices, key).map_or_else(|| "This host".to_owned(), str::to_owned)
}

/// `device_lead(Some("Speaker"), "local monitoring off.")` → `Speaker: local
/// monitoring off.`; without a name, the sentence alone, capitalised.
#[must_use]
pub fn device_lead(name: Option<&str>, sentence: &str) -> String {
    match name {
        Some(n) => format!("{n}: {sentence}"),
        None => {
            let mut c = sentence.chars();
            c.next().map_or_else(String::new, |f| {
                f.to_uppercase().chain(c).collect::<String>()
            })
        }
    }
}

/// A set of channels in the portal's words: the channels' labels with the
/// channels beside them (`CQ Main L–R (TX 1–2)`), or only the channels when
/// none is labelled (`TX 1–2`); the device's name, not its key, in front
/// when the host has several devices. A device the status does not list is
/// named by its key, as the command line does.
#[must_use]
pub fn channels_words(set: &ChannelSet, status: Option<&Status>) -> String {
    let dir = match set.direction {
        Direction::Rx => "RX",
        _ => "TX",
    };
    let numbers = format!(
        "{dir} {}",
        tesseractl::output::channel_runs(&set.channels, tesseractl::output::Style::Typographic)
    );
    let Some((s, d)) = status.and_then(|s| {
        s.devices
            .iter()
            .find(|d| d.key == set.device)
            .map(|d| (s, d))
    }) else {
        return format!("{} {numbers}", set.device.0);
    };
    let list = match set.direction {
        Direction::Rx => &d.rx_channels,
        _ => &d.tx_channels,
    };
    let labels: Vec<&str> = set
        .channels
        .iter()
        .filter_map(|n| list.iter().find(|c| c.channel == *n))
        .map(|c| c.label.as_str())
        .collect();
    let named = labels.len() == set.channels.len()
        && labels
            .iter()
            .zip(&set.channels)
            .any(|(l, n)| !l.trim().is_empty() && *l != format!("{dir} {}", n.get()));
    let words = if named {
        format!("{} ({numbers})", labels_summary(&labels))
    } else {
        numbers
    };
    if s.devices.len() > 1 {
        format!("{}: {words}", d.name)
    } else {
        words
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn ch(n: u16) -> Channel {
        Channel::new(n).unwrap()
    }

    #[test]
    fn ports_round_trip_through_one_form_value() {
        let ports = [
            RouterPort::CardIn {
                card: "hw:CARD=CQ,DEV=0".into(),
                channel: ch(15),
            },
            RouterPort::NetworkRx {
                device: DeviceKey("main".into()),
                channel: ch(1),
            },
            RouterPort::HostOut {
                device: DeviceKey("a/b>c".into()),
                channel: ch(2),
            },
        ];
        for p in &ports {
            let v = encode(p);
            assert!(!v[v.find('/').unwrap() + 1..].contains('>'), "{v}");
            assert_eq!(decode(&v).as_ref(), Some(p), "{v}");
        }
        assert_eq!(encode(&ports[0]), "card_in/15/hw%3ACARD%3DCQ%2CDEV%3D0");
        let k = CrosspointKey::new(ports[0].clone(), ports[2].clone());
        assert_eq!(decode_xp(&encode_xp(&k)), Some(k));
        for bad in [
            "",
            "x/1/a",
            "card_in/0/a",
            "card_in/1/",
            "card_in/1/%zz",
            "card_in/1",
        ] {
            assert_eq!(decode(bad), None, "{bad}");
        }
    }

    #[test]
    fn ports_are_named_in_plain_words() {
        let dev = || DeviceKey("main".into());
        let rx2 = RouterPort::NetworkRx {
            device: dev(),
            channel: ch(2),
        };
        let tx1 = RouterPort::NetworkTx {
            device: dev(),
            channel: ch(1),
        };
        let app = RouterPort::AppOut {
            device: dev(),
            channel: ch(1),
        };
        let out2 = RouterPort::HostOut {
            device: dev(),
            channel: ch(2),
        };
        let cq = |n| RouterPort::CardIn {
            card: "hw:CARD=CQ,DEV=0".into(),
            channel: ch(n),
        };
        assert_eq!(place(&rx2), "Network input 2 (RX)");
        assert_eq!(place(&tx1), "Network output 1 (TX)");
        assert_eq!(place(&app), "App playback 1");
        assert_eq!(place(&out2), "Host output 2");
        let mut r = RouterReport::default();
        r.ports = vec![
            PortStatus::new(rx2.clone(), "RX 2"),
            PortStatus::new(tx1.clone(), "CQ Ch1 Mic"),
            PortStatus::new(app.clone(), "App 1"),
            PortStatus::new(out2.clone(), "Speaker R"),
            PortStatus::new(cq(1), "CQ in 1"),
            PortStatus::new(cq(2), "CQ in 2"),
        ];
        // The daemon marks the labels someone gave.
        for p in &mut r.ports {
            p.custom_label = matches!(p.label.as_str(), "CQ Ch1 Mic" | "Speaker R");
        }
        // A label that looks like a default but was given is a name.
        r.ports.push(PortStatus::new(cq(3), "CQ in 3"));
        r.ports.last_mut().unwrap().custom_label = true;
        r.crosspoints = vec![CrosspointStatus::new(
            tessera_control_types::Crosspoint::new(cq(1), tx1.clone()),
            CrosspointOrigin::Bridge {
                name: "cq-in".into(),
            },
        )];
        // The daemon's defaults are no names: where the port is instead.
        assert_eq!(label(Some(&r), &rx2), "Network input 2 (RX)");
        assert_eq!(label(Some(&r), &app), "App playback 1");
        assert_eq!(label(Some(&r), &cq(2)), "CQ input 2");
        // A given name wins.
        assert_eq!(label(Some(&r), &out2), "Speaker R");
        assert_eq!(label(Some(&r), &tx1), "CQ Ch1 Mic");
        // A card input says which network channel it feeds; in a
        // crosspoint's words only its own name.
        assert_eq!(label(Some(&r), &cq(1)), "CQ input 1 \u{2192} CQ Ch1 Mic");
        let k = CrosspointKey::new(cq(1), out2);
        assert_eq!(xp_words(Some(&r), &k), "CQ input 1 \u{2192} Speaker R");
        // Without a report: plain words.
        assert_eq!(label(None, &cq(1)), "CQ input 1");
        assert_eq!(label(Some(&r), &cq(3)), "CQ in 3");
    }

    #[test]
    fn sentences_name_a_device_by_its_name_and_only_among_several() {
        use tessera_control_types::{DeviceStatus, Encoding, LatencyUs, SampleRate};
        let device = |key: &str, name: &str| {
            DeviceStatus::new(
                DeviceKey(key.into()),
                name,
                SampleRate(48_000),
                Encoding(24),
                LatencyUs(1_000),
            )
        };
        let main = DeviceKey("main".into());
        let one = [device("main", "Speaker")];
        assert_eq!(device_name(&one, &main), None);
        assert_eq!(device_subject(&one, &main), "This host");
        assert_eq!(
            device_lead(device_name(&one, &main), "local monitoring off."),
            "Local monitoring off."
        );
        let two = [device("main", "Speaker"), device("aux", "Studio")];
        assert_eq!(device_name(&two, &main), Some("Speaker"));
        assert_eq!(device_subject(&two, &main), "Speaker");
        assert_eq!(
            device_lead(device_name(&two, &main), "local monitoring off."),
            "Speaker: local monitoring off."
        );
    }

    #[test]
    fn channels_are_named_by_their_labels_and_the_device_name() {
        use tessera_control_types::{ChannelStatus, DeviceStatus, Encoding, LatencyUs, SampleRate};
        let device = |key: &str, name: &str, tx: &[&str]| {
            DeviceStatus::new(
                DeviceKey(key.into()),
                name,
                SampleRate(48_000),
                Encoding(24),
                LatencyUs(1_000),
            )
            .with_tx_channels(
                tx.iter()
                    .enumerate()
                    .map(|(i, l)| ChannelStatus::new(ch(u16::try_from(i + 1).unwrap()), *l))
                    .collect(),
            )
        };
        let set = |key: &str, chans: &[u16]| ChannelSet {
            device: DeviceKey(key.into()),
            direction: Direction::Tx,
            channels: chans.iter().map(|n| ch(*n)).collect(),
        };
        let one = Status::new("0").with_devices(vec![device(
            "main",
            "speaker",
            &["CQ Main L", "CQ Main R", "TX 3", "Desk", "Vox"],
        )]);
        let w = |s: &ChannelSet| channels_words(s, Some(&one));
        assert_eq!(
            w(&set("main", &[1, 2])),
            "CQ Main L\u{2013}R (TX 1\u{2013}2)"
        );
        assert_eq!(w(&set("main", &[1])), "CQ Main L (TX 1)");
        // Default labels: the channels alone, no device key.
        assert_eq!(w(&set("main", &[3])), "TX 3");
        assert_eq!(w(&set("main", &[4, 5])), "Desk, Vox (TX 4\u{2013}5)");
        // Several devices: the device's name in front.
        let two = Status::new("0").with_devices(vec![
            device("main", "speaker", &["A"]),
            device("b", "studio", &["B"]),
        ]);
        assert_eq!(
            channels_words(&set("b", &[1]), Some(&two)),
            "studio: B (TX 1)"
        );
        // A device the status does not list: as the command line says it.
        assert_eq!(channels_words(&set("gone", &[1]), Some(&one)), "gone TX 1");
        assert_eq!(channels_words(&set("main", &[1]), None), "main TX 1");
    }
    #[test]
    fn ids_are_safe_and_distinct() {
        let a = RouterPort::CardIn {
            card: "hw:CARD=CQ,DEV=0".into(),
            channel: ch(15),
        };
        let b = RouterPort::CardIn {
            card: "hw:CARD=CQ-DEV=0".into(),
            channel: ch(15),
        };
        assert_ne!(port_id(&a), port_id(&b));
        assert!(
            port_id(&a)
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-'),
            "{}",
            port_id(&a)
        );
        assert_eq!(card_short("hw:CARD=CQ,DEV=0"), "CQ");
        assert_eq!(place(&a), "CQ input 15");
        assert_eq!(card_short("hw:1,0"), "1");
    }
}
