//! Human-readable rendering of status, events and clock information.

use std::fmt::Write as _;

use crate::sanitize::cleaned;

use tessera_control_types::{
    BindingAction, BridgeDirection, BridgeState, BridgeStatus, ButtonMode, CardBridgeSpec, Channel,
    ChannelSet, ClockRole, ClockSettings, ClockState, ClockStatus, ControlKind, ControlValue,
    CrosspointKey, DetectedKind, DeviceStatus, Direction, Event, Gain, LearnedControl, Metrics,
    MidiAddress, MidiKind, NetworkStatus, Origin, PrimarySource, RouterPort, Status,
    SubscriptionState, SubscriptionStatus, SurfaceBinding, SurfaceControl, SurfaceSource,
    SurfaceStatus, SurfacesReport, Taper, Timestamping,
};

/// Lay out `rows` (the first is the header) in aligned columns.
#[must_use]
pub fn table(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut width = vec![0usize; cols];
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            if let Some(w) = width.get_mut(i) {
                *w = (*w).max(c.chars().count());
            }
        }
    }
    let mut out = String::new();
    for r in rows {
        let mut line = String::new();
        for (i, c) in r.iter().enumerate() {
            let w = width.get(i).copied().unwrap_or(0);
            if i + 1 == r.len() {
                line.push_str(c);
            } else {
                let _ = write!(line, "{c:<w$}  ");
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn dash_if_empty(items: &[String]) -> String {
    if items.is_empty() {
        "-".into()
    } else {
        items.join(",")
    }
}

/// Characters for the words of gains, ranges and bindings: ASCII for the
/// terminal, typographic (real minus, en dash, ellipsis) for the portal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// `-6.0 dB`, `1-2`, `-60..0 dB`.
    Ascii,
    /// `−6.0 dB`, `1–2`, `−60…0 dB`.
    Typographic,
}

impl Style {
    fn minus(self) -> &'static str {
        match self {
            Self::Ascii => "-",
            Self::Typographic => "\u{2212}",
        }
    }

    fn dash(self) -> &'static str {
        match self {
            Self::Ascii => "-",
            Self::Typographic => "\u{2013}",
        }
    }

    fn ellipsis(self) -> &'static str {
        match self {
            Self::Ascii => "..",
            Self::Typographic => "\u{2026}",
        }
    }
}

/// Milli-dB as a dB number with up to one decimal: `-6`, `-6.5`, `+3`
/// (`0` unsigned).
#[must_use]
pub fn db_number(mdb: i32, style: Style) -> String {
    let abs = mdb.unsigned_abs();
    // Tenths, rounded half away from zero.
    let tenths = (abs + 50) / 100;
    let body = if tenths.is_multiple_of(10) {
        format!("{}", tenths / 10)
    } else {
        format!("{}.{}", tenths / 10, tenths % 10)
    };
    if tenths == 0 {
        "0".into()
    } else if mdb < 0 {
        format!("{}{body}", style.minus())
    } else {
        format!("+{body}")
    }
}

/// A gain in words: `off`, `0 dB`, `-6.5 dB`.
#[must_use]
pub fn gain_words(g: Gain, style: Style) -> String {
    match g {
        Gain::Off => "off".into(),
        Gain::MilliDb(m) => format!("{} dB", db_number(m, style)),
        _ => "?".into(),
    }
}

/// Channels as runs: `1-4,7` (ASCII) or `1–4, 7`.
#[must_use]
pub fn channel_runs(channels: &[Channel], style: Style) -> String {
    let mut runs: Vec<(u16, u16)> = Vec::new();
    for c in channels {
        let n = c.get();
        match runs.last_mut() {
            Some((_, end)) if end.checked_add(1) == Some(n) => *end = n,
            _ => runs.push((n, n)),
        }
    }
    let sep = match style {
        Style::Ascii => ",",
        Style::Typographic => ", ",
    };
    runs.iter()
        .map(|(a, b)| {
            if a == b {
                a.to_string()
            } else {
                format!("{a}{}{b}", style.dash())
            }
        })
        .collect::<Vec<_>>()
        .join(sep)
}

/// A channel set in words: `main TX 1-2`.
#[must_use]
pub fn channel_set_words(t: &ChannelSet, style: Style) -> String {
    format!(
        "{} {} {}",
        t.device.0,
        match t.direction {
            Direction::Rx => "RX",
            Direction::Tx => "TX",
        },
        channel_runs(&t.channels, style)
    )
}

/// What a binding does, in words: `Mute: main TX 1-2, toggle` or
/// `Gain: main RX 1-2, -60..0 dB, owns gain`. `control` is the bound
/// control (to tell a motorized fader and an encoder), `owning` whether
/// the surface is connected and enabled, so that its non-motorized faders
/// set the gain of their channels right now.
#[must_use]
pub fn binding_words(
    b: &SurfaceBinding,
    control: Option<&SurfaceControl>,
    owning: bool,
    style: Style,
) -> String {
    let target = channel_set_words(&b.target, style);
    match b.action {
        BindingAction::Mute | BindingAction::OutputMute => {
            let mode = match b.mode {
                ButtonMode::Toggle => "toggle",
                ButtonMode::Momentary => "hold to mute",
                ButtonMode::PushToTalk => "push to talk",
                _ => "?",
            };
            let what = if b.action == BindingAction::OutputMute {
                "Output mute"
            } else {
                "Mute"
            };
            format!("{what}: {target}, {mode}")
        }
        BindingAction::Gain | BindingAction::OutputGain => {
            let output = b.action == BindingAction::OutputGain;
            let what = if output { "Output" } else { "Gain" };
            let kind = control.map(|c| c.kind);
            if kind == Some(ControlKind::Encoder) {
                return format!(
                    "{what}: {target}, {} dB per step",
                    db_number(b.step_mdb, style)
                );
            }
            let range = match (b.curve.first(), b.curve.last()) {
                (Some(lo), Some(hi)) => format!(
                    "{}{}{} dB (the desk's fader law)",
                    db_number(lo.mdb, style),
                    style.ellipsis(),
                    db_number(hi.mdb, style)
                ),
                _ => format!(
                    "{}{}{} dB",
                    db_number(b.min_mdb, style),
                    style.ellipsis(),
                    db_number(b.max_mdb, style)
                ),
            };
            let taper = match b.taper {
                _ if !b.curve.is_empty() => "",
                Taper::Db => "",
                Taper::Linear => ", linear taper",
                _ => ", custom taper",
            };
            if output {
                // The output level is never owned.
                return format!("{what}: {target}, {range}{taper}");
            }
            let owner = if control.is_some_and(|c| c.motorized) {
                "motorized"
            } else if owning {
                "owns gain"
            } else {
                "owns gain while connected"
            };
            format!("{what}: {target}, {range}{taper}, {owner}")
        }
        BindingAction::HostVolume => "Host volume".into(),
        BindingAction::HostMute => "Host mute".into(),
        BindingAction::PortGain | BindingAction::PortMute => {
            let ports: Vec<String> = b.ports.iter().map(crate::cli::port_words).collect();
            let what = if b.action == BindingAction::PortGain {
                "Port level"
            } else {
                "Port mute"
            };
            format!("{what}: {}", ports.join(", "))
        }
        BindingAction::CrosspointGain | BindingAction::CrosspointMute => {
            let xs: Vec<String> = b
                .crosspoints
                .iter()
                .map(|x| {
                    format!(
                        "{} -> {}",
                        crate::cli::port_words(&x.source),
                        crate::cli::port_words(&x.destination)
                    )
                })
                .collect();
            let what = if b.action == BindingAction::CrosspointGain {
                "Crosspoint gain"
            } else {
                "Crosspoint mute"
            };
            format!("{what}: {}", xs.join(", "))
        }
        BindingAction::Follow => b.follow.as_ref().map_or_else(
            || "Follow: -".into(),
            |f| format!("Follows {}/{}", f.surface, f.control),
        ),
        _ => target,
    }
}

fn hex4(v: Option<u16>) -> String {
    v.map_or_else(|| "-".into(), |v| format!("{v:04x}"))
}

/// Where a MIDI control lives, in words: `CC 7, ch 1`, `Note 60, ch 1`, or
/// for NRPN its MSB/LSB in hex as desk manuals write them and the parameter
/// number: `NRPN 40/00 (8192), ch 1`.
#[must_use]
pub fn midi_words(m: &MidiAddress) -> String {
    match m.kind {
        MidiKind::ControlChange => format!("CC {}, ch {}", m.number, m.channel),
        MidiKind::Nrpn => format!(
            "NRPN {:02X}/{:02X} ({}), ch {}",
            m.number >> 7,
            m.number & 0x7f,
            m.number,
            m.channel
        ),
        MidiKind::Note => format!("Note {}, ch {}", m.number, m.channel),
        _ => format!("MIDI {}, ch {}", m.number, m.channel),
    }
}

/// A surface's source in words: `USB 3142:0c33, interface 3` or
/// `MIDI card CQ, port 0`.
#[must_use]
pub fn source_words(s: &SurfaceSource) -> String {
    match s {
        SurfaceSource::Hidraw {
            vendor_id,
            product_id,
            interface,
        } => format!(
            "USB {vendor_id:04x}:{product_id:04x}{}",
            interface.map_or_else(String::new, |i| format!(", interface {i}"))
        ),
        SurfaceSource::Midi { card, device } => format!("MIDI card {card}, port {device}"),
        SurfaceSource::TcpMidi { host, port } => format!("MIDI over TCP to {host}:{port}"),
        _ => "an unknown source".into(),
    }
}

/// Sample rates as `44.1/48 kHz`.
#[must_use]
pub fn rates_words(rates: &[u32]) -> String {
    if rates.is_empty() {
        return "-".into();
    }
    let list: Vec<String> = rates
        .iter()
        .map(|r| {
            if r % 1000 == 0 {
                (r / 1000).to_string()
            } else {
                format!("{:.1}", f64::from(*r) / 1000.0)
            }
        })
        .collect();
    format!("{} kHz", list.join("/"))
}

/// What a detected device offers beyond its kind: a sound card's channels,
/// rates and PCM; a MIDI port's card.
fn detected_details(d: &tessera_control_types::DetectedDevice) -> String {
    let mut parts: Vec<String> = Vec::new();
    if d.kind == DetectedKind::SoundCard {
        if let Some(i) = d.capture_channels {
            parts.push(format!("in {i}"));
        }
        if let Some(o) = d.playback_channels {
            parts.push(format!("out {o}"));
        }
        if !d.rates.is_empty() {
            parts.push(rates_words(&d.rates));
        }
        if let Some(p) = &d.pcm {
            parts.push(p.clone());
        }
    } else if let Some(SurfaceSource::Midi { card, device }) = &d.source {
        parts.push(format!("card {card} port {device}"));
    }
    if parts.is_empty() {
        "-".into()
    } else {
        parts.join(", ")
    }
}

/// A card bridge's channel mapping in words: `hw:CARD=SC3,DEV=0 in 1-2 ->
/// main TX 1-2` or `main RX 3-4 -> hw:... out 1-2`.
#[must_use]
pub fn bridge_mapping_words(
    card: &str,
    device: &str,
    direction: BridgeDirection,
    first: u16,
    channels: u16,
    style: Style,
) -> String {
    bridge_slice_words(card, device, direction, first, channels, 1, style)
}

/// As [`bridge_mapping_words`], for a bridge that carries card channels
/// from `card_first` on: `hw:CARD=CQ,DEV=0 in 15-16 -> main TX 1-2`.
#[must_use]
pub fn bridge_slice_words(
    card: &str,
    device: &str,
    direction: BridgeDirection,
    first: u16,
    channels: u16,
    card_first: u16,
    style: Style,
) -> String {
    let span = |a: u32, n: u32| {
        if n <= 1 {
            a.to_string()
        } else {
            format!("{a}{}{}", style.dash(), a + n - 1)
        }
    };
    let n = u32::from(channels);
    let dev = span(u32::from(first), n);
    let crd = span(u32::from(card_first.max(1)), n);
    let arrow = match style {
        Style::Ascii => "->",
        Style::Typographic => "\u{2192}",
    };
    match direction {
        BridgeDirection::Capture => format!("{card} in {crd} {arrow} {device} TX {dev}"),
        _ => format!("{device} RX {dev} {arrow} {card} out {crd}"),
    }
}

/// A saved card bridge in words (for confirmations). A `local_only` bridge
/// feeds no device channel: `hw:CARD=CQ,DEV=0 in 15-16 -> host router only`.
#[must_use]
pub fn card_bridge_words(b: &CardBridgeSpec, style: Style) -> String {
    if b.local_only {
        let first = u32::from(b.card_first_channel.max(1));
        let n = u32::from(b.channels);
        let span = if n <= 1 {
            first.to_string()
        } else {
            format!("{first}{}{}", style.dash(), first + n - 1)
        };
        let arrow = match style {
            Style::Ascii => "->",
            Style::Typographic => "\u{2192}",
        };
        return match b.direction {
            BridgeDirection::Capture => format!("{} in {span} {arrow} host router only", b.card),
            _ => format!("host router only {arrow} {} out {span}", b.card),
        };
    }
    bridge_slice_words(
        &b.card,
        &b.device.0,
        b.direction,
        b.first_channel,
        b.channels,
        b.card_first_channel,
        style,
    )
}

/// What kind of detected device, in words.
#[must_use]
pub fn detected_kind(k: DetectedKind) -> &'static str {
    match k {
        DetectedKind::Hid => "HID",
        DetectedKind::SoundCard => "sound card",
        DetectedKind::Midi => "MIDI",
        _ => "other",
    }
}

/// A control's live value: `128/255`, `pressed`, `released`, `-` (not
/// reported yet).
#[must_use]
pub fn control_value(v: Option<&ControlValue>) -> String {
    match v {
        None => "-".into(),
        Some(ControlValue::Button { pressed: true }) => "pressed".into(),
        Some(ControlValue::Button { pressed: false }) => "released".into(),
        Some(ControlValue::Fader { value, max }) => format!("{value}/{max}"),
        Some(_) => "?".into(),
    }
}

/// The connection state of a surface in words.
#[must_use]
pub fn surface_state(s: &SurfaceStatus) -> String {
    if !s.spec.enabled {
        "disabled".into()
    } else if s.connected {
        match &s.device {
            Some(d) => format!("connected ({d})"),
            None => "connected".into(),
        }
    } else {
        match &s.problem {
            Some(p) => format!("not connected: {p}"),
            None => "not found".into(),
        }
    }
}

/// `surfaces`: the detected devices, then each surface with its controls
/// and bindings.
#[must_use]
pub fn render_surfaces(r: &SurfacesReport) -> String {
    let r = &cleaned(r);
    let mut out = String::new();
    if r.detected.is_empty() {
        out.push_str("No devices detected.\n");
    } else {
        out.push_str("Detected devices:\n");
        let mut rows = vec![vec![
            "ID".into(),
            "KIND".into(),
            "NAME".into(),
            "USB".into(),
            "IFACE".into(),
            "NODE".into(),
            "DETAILS".into(),
            "USED BY".into(),
        ]];
        for d in &r.detected {
            rows.push(vec![
                d.id.clone(),
                detected_kind(d.kind).into(),
                d.name.clone(),
                if d.vendor_id.is_some() || d.product_id.is_some() {
                    format!("{}:{}", hex4(d.vendor_id), hex4(d.product_id))
                } else {
                    "-".into()
                },
                d.interface.map_or_else(|| "-".into(), |i| i.to_string()),
                d.node.clone(),
                detected_details(d),
                d.used_by.clone().unwrap_or_else(|| "-".into()),
            ]);
        }
        for l in table(&rows).lines() {
            let _ = writeln!(out, "  {l}");
        }
    }
    if r.surfaces.is_empty() {
        out.push_str("No control surfaces configured.\n");
    }
    for s in &r.surfaces {
        let owning = s.spec.enabled && s.connected;
        let _ = writeln!(out, "Surface {}: {}", s.spec.name, surface_state(s));
        let _ = writeln!(out, "  source: {}", source_words(&s.spec.source));
        if s.spec.controls.is_empty() {
            out.push_str("  no controls yet\n");
        }
        for c in &s.spec.controls {
            let value = s.controls.iter().find(|v| v.id == c.id);
            let kind = match c.kind {
                ControlKind::Button => "button",
                ControlKind::Fader => "fader",
                ControlKind::Encoder => "encoder",
                _ => "control",
            };
            let label = c
                .label
                .as_ref()
                .map_or_else(String::new, |l| format!(" \"{l}\""));
            let midi = c
                .midi
                .as_ref()
                .map_or_else(String::new, |m| format!("  [{}]", midi_words(m)));
            let motor = if c.motorized { " (motorized)" } else { "" };
            let _ = writeln!(
                out,
                "  {}{label}  {kind}{motor}{midi}  {}",
                c.id,
                control_value(value.and_then(|v| v.value.as_ref()))
            );
            for b in s.spec.bindings.iter().filter(|b| b.control == c.id) {
                let _ = writeln!(
                    out,
                    "    {}",
                    binding_words(b, Some(c), owning, Style::Ascii)
                );
            }
        }
        for b in s
            .spec
            .bindings
            .iter()
            .filter(|b| !s.spec.controls.iter().any(|c| c.id == b.control))
        {
            let _ = writeln!(
                out,
                "  (unknown control {}) {}",
                b.control,
                binding_words(b, None, owning, Style::Ascii)
            );
        }
    }
    out
}

/// `surface learn`: the suggested layout of the control that moved.
#[must_use]
pub fn render_learned(l: &LearnedControl) -> String {
    let kind = match l.kind {
        ControlKind::Button => "button",
        ControlKind::Fader => "fader",
        ControlKind::Encoder => "encoder",
        _ => "control",
    };
    let mut out = format!("A {kind} moved.\n");
    let mut rows = vec![vec!["kind".to_owned(), kind.to_owned()]];
    if let Some(m) = &l.midi {
        rows.push(vec!["midi".into(), midi_words(m)]);
        if let Some(max) = l.max {
            rows.push(vec!["max".into(), max.to_string()]);
        }
        for line in table(&rows).lines() {
            let _ = writeln!(out, "  {line}");
        }
        return out;
    }
    rows.push(vec![
        "report_id".into(),
        l.report_id
            .map_or_else(|| "none (unnumbered)".into(), |r| r.to_string()),
    ]);
    rows.push(vec!["offset".into(), format!("{} (bits)", l.offset)]);
    rows.push(vec!["bits".into(), l.bits.to_string()]);
    if let Some(m) = l.max {
        rows.push(vec!["max".into(), m.to_string()]);
    }
    match l.kind {
        ControlKind::Button => rows.push(vec!["active_low".into(), l.active_low.to_string()]),
        _ => rows.push(vec!["inverted".into(), l.inverted.to_string()]),
    }
    for line in table(&rows).lines() {
        let _ = writeln!(out, "  {line}");
    }
    out
}

/// Words for a subscription state.
#[must_use]
pub fn sub_state(s: &SubscriptionState) -> String {
    match s {
        SubscriptionState::Receiving => "receiving".into(),
        SubscriptionState::InProgress => "in progress".into(),
        SubscriptionState::Unresolved => "unresolved (transmitter not found)".into(),
        SubscriptionState::FlowRefused { code } => format!("flow refused (code {code:#06x})"),
        SubscriptionState::NoMedia => "no media".into(),
        SubscriptionState::RateMismatch { ours, theirs } => {
            format!("rate mismatch (ours {} Hz, theirs {} Hz)", ours.0, theirs.0)
        }
        SubscriptionState::EncodingMismatch { ours, theirs } => {
            format!(
                "encoding mismatch (ours {} bit, theirs {} bit)",
                ours.0, theirs.0
            )
        }
        other => format!("{other:?}"),
    }
}

fn clock_state(s: ClockState) -> &'static str {
    match s {
        ClockState::FreeRunning => "free-running",
        ClockState::Listening => "listening",
        ClockState::Acquiring => "acquiring",
        ClockState::Locked => "locked",
        ClockState::Holdover => "holdover",
        ClockState::Leader => "leader",
        _ => "unknown",
    }
}

fn timestamping(t: Timestamping) -> &'static str {
    match t {
        Timestamping::Hardware => "hardware",
        Timestamping::Software => "software",
        _ => "unknown",
    }
}

/// The clock table (empty string if there are no clocks).
#[must_use]
pub fn render_clocks(clocks: &[ClockStatus]) -> String {
    let clocks = &cleaned(&clocks.to_vec());
    if clocks.is_empty() {
        return String::new();
    }
    let mut rows = vec![vec![
        "INTERFACE".to_owned(),
        "STATE".into(),
        "TIMESTAMPS".into(),
        "OFFSET".into(),
        "RATE".into(),
        "GRANDMASTER".into(),
        "EPOCH".into(),
    ]];
    for c in clocks {
        rows.push(vec![
            c.interface.clone(),
            clock_state(c.state).into(),
            timestamping(c.timestamping).into(),
            c.offset_ns.map_or("-".into(), |o| format!("{o} ns")),
            format!("{} ppb", c.rate_ppb),
            c.grandmaster.clone().unwrap_or_else(|| "-".into()),
            c.epoch.to_string(),
        ]);
    }
    table(&rows)
}

fn subscription_rows(devices: &[DeviceStatus]) -> Vec<Vec<String>> {
    let mut rows = vec![vec![
        "DEVICE".to_owned(),
        "RX".into(),
        "FROM".into(),
        "STATE".into(),
        "LATENCY".into(),
    ]];
    for d in devices {
        for s in &d.subscriptions {
            rows.push(vec![
                d.key.0.clone(),
                s.rx_channel.to_string(),
                format!("{} @ {}", s.tx_channel, s.tx_device),
                sub_state(&s.state),
                s.effective_latency
                    .map_or("-".into(), |l| format!("{} us", l.0)),
            ]);
        }
    }
    rows
}

/// One line for the effective clock role and priority.
#[must_use]
pub fn render_clock_settings(c: &ClockSettings) -> String {
    let role = match c.role {
        ClockRole::Follower => "follower",
        ClockRole::Leader => "leader",
        ClockRole::Auto => "auto",
        _ => "unknown",
    };
    let locked = if c.locked.is_empty() {
        String::new()
    } else {
        format!(" (locked: {})", c.locked.join(", "))
    };
    cleaned(&format!("role {role}, priority {}{locked}", c.priority))
}

/// The per-flow counters as a table.
#[must_use]
pub fn render_flows(flows: &[tessera_control_types::FlowMetrics]) -> String {
    let flows = &cleaned(&flows.to_vec());
    if flows.is_empty() {
        return String::new();
    }
    let mut rows = vec![vec![
        "FLOW".to_owned(),
        "DEVICE".into(),
        "DIR".into(),
        "PACKETS".into(),
        "FRAMES".into(),
        "LATE".into(),
        "GAPS".into(),
        "HOLES".into(),
        "UNDERRUN".into(),
        "SKIPS".into(),
        "REFUSED".into(),
        "MAX LATENESS".into(),
    ]];
    for f in flows {
        let (late, gaps, holes, max_late) = f.rx.as_ref().map_or_else(
            || {
                (
                    "-".to_owned(),
                    "-".to_owned(),
                    "-".to_owned(),
                    "-".to_owned(),
                )
            },
            |r| {
                (
                    r.late_packets.to_string(),
                    r.gap_events.to_string(),
                    r.hole_samples.to_string(),
                    format!("{} us", r.max_lateness_us),
                )
            },
        );
        let (under, skips, refused) = f.tx.as_ref().map_or_else(
            || ("-".to_owned(), "-".to_owned(), "-".to_owned()),
            |t| {
                (
                    t.underrun_samples.to_string(),
                    t.late_skips.to_string(),
                    t.send_failures.to_string(),
                )
            },
        );
        rows.push(vec![
            f.id.clone(),
            f.device.0.clone(),
            match f.direction {
                Direction::Rx => "rx".to_owned(),
                _ => "tx".to_owned(),
            },
            f.packets.to_string(),
            f.frames.to_string(),
            late,
            gaps,
            holes,
            under,
            skips,
            refused,
            max_late,
        ]);
    }
    table(&rows)
}

fn ppm(ppb: i64) -> String {
    format!("{:+.1}", ppb as f64 / 1000.0)
}

/// The hardware-card bridges: one row each, and the last error of those
/// that have one.
#[must_use]
pub fn render_bridges(bridges: &[BridgeStatus]) -> String {
    let bridges = &cleaned(&bridges.to_vec());
    if bridges.is_empty() {
        return String::new();
    }
    let mut rows = vec![vec![
        "BRIDGE".to_owned(),
        "DEVICE".into(),
        "DIR".into(),
        "CHANNELS".into(),
        "CARD".into(),
        "STATE".into(),
        "FILL".into(),
        "POS ERR".into(),
        "CORR PPM".into(),
        "TREND PPM".into(),
        "RATIO".into(),
        "XRUNS".into(),
        "SLIPS".into(),
        "SHORT".into(),
        "REOPENS".into(),
    ]];
    for b in bridges {
        let state = match b.state {
            BridgeState::Starting => "starting",
            BridgeState::Running => "running",
            BridgeState::Reopening => "reopening",
            BridgeState::Failed => "failed",
            BridgeState::Disabled => "disabled",
            _ => "unknown",
        };
        let dir = match b.direction {
            BridgeDirection::Playback => "playback",
            BridgeDirection::Capture => "capture",
            _ => "?",
        };
        let channels = if b.channels == 0 {
            "-".to_owned()
        } else if b.local_only {
            // Router ports only: no device channel.
            "router".to_owned()
        } else {
            let last = u32::from(b.first_channel) + u32::from(b.channels) - 1;
            format!("{}..{last}", b.first_channel)
        };
        // The card's own channels when they do not start at 1.
        let card = if b.card_first_channel > 1 && b.channels > 0 {
            let last = u32::from(b.card_first_channel) + u32::from(b.channels) - 1;
            format!("{} ch {}..{last}", b.card, b.card_first_channel)
        } else {
            b.card.clone()
        };
        rows.push(vec![
            b.name.clone(),
            b.device.0.clone(),
            dir.to_owned(),
            channels,
            card,
            format!("{state}{}", if b.rt_degraded { " (no rt)" } else { "" }),
            b.fill_frames.to_string(),
            b.position_error_frames.to_string(),
            ppm(b.correction_ppb),
            ppm(b.trend_ppb),
            format!("{:.9}", b.ratio),
            b.xruns.to_string(),
            b.slips.to_string(),
            b.short_frames.to_string(),
            b.reopens.to_string(),
        ]);
    }
    let mut out = table(&rows);
    for b in bridges {
        if let Some(e) = &b.last_error {
            let _ = writeln!(out, "  {}: {e}", b.name);
        }
    }
    out
}

/// The `metrics` output.
#[must_use]
pub fn render_metrics(m: &Metrics) -> String {
    let d = &m.daemon;
    let a = &m.audio;
    let mut rows = vec![vec!["COUNTER".to_owned(), "VALUE".into()]];
    let mut add = |k: &str, v: u64| rows.push(vec![k.to_owned(), v.to_string()]);
    add("daemon.uptime_ms", d.uptime_ms);
    add("daemon.control_connections", d.control_connections);
    add(
        "daemon.control_connections_total",
        d.control_connections_total,
    );
    add(
        "daemon.control_connections_refused",
        d.control_connections_refused,
    );
    add("daemon.requests_total", d.requests_total);
    add("daemon.requests_rate_limited", d.requests_rate_limited);
    add("daemon.requests_denied", d.requests_denied);
    add("daemon.requests_invalid", d.requests_invalid);
    add("daemon.subscribers", d.subscribers);
    add("daemon.subscriber_lags", d.subscriber_lags);
    add("daemon.task_restarts", d.task_restarts);
    add("audio.sessions", a.sessions);
    add("audio.sessions_total", a.sessions_total);
    add("audio.rejects_total", a.rejects_total);
    add("audio.ring_bytes", a.ring_bytes);
    add("audio.connections_refused", a.connections_refused);
    let e = &m.engine;
    add("engine.running", e.running);
    add("engine.ticks", e.ticks);
    add("engine.tick_skips", e.tick_skips);
    add("engine.max_wake_lateness_us", e.max_wake_lateness_us);
    add("engine.max_tick_work_us", e.max_tick_work_us);
    add("engine.rx_datagrams", e.rx_datagrams);
    add("engine.rx_batches", e.rx_batches);
    add("engine.rx_filtered", e.rx_filtered);
    add("engine.rx_truncated", e.rx_truncated);
    add("engine.rx_batch_limit_hits", e.rx_batch_limit_hits);
    add("engine.tx_datagrams", e.tx_datagrams);
    add("engine.tx_batches", e.tx_batches);
    add("engine.tx_send_failures", e.tx_send_failures);
    add("engine.tx_suppressed_ticks", e.tx_suppressed_ticks);
    add("engine.wake_signals", e.wake_signals);
    add("engine.sessions", e.sessions);
    add("engine.flows", e.flows);
    add("engine.rt_degraded", e.rt_degraded);
    add("engine.mlock_failed", e.mlock_failed);
    add("engine.events_dropped", e.events_dropped);
    add("engine.rx_errors", e.rx_errors);
    add("engine.retire_leaks", e.retire_leaks);
    add("engine.rx_transit_max_us", e.rx_transit_max_us);
    add("engine.socket_buffer_min_bytes", e.socket_buffer_min_bytes);
    add("engine.socket_buffers_capped", e.socket_buffers_capped);
    // Wake-up lateness: how many wake-ups fell in each band.
    let bands = [
        "<25", "<50", "<100", "<250", "<500", "<1000", "<2000", "<4000", ">=4000",
    ];
    for (band, n) in bands.iter().zip(&e.wake_lateness_hist) {
        add(&format!("engine.wake_lateness_us{{{band}}}"), *n);
    }
    let mut out = table(&rows);
    let flows = render_flows(&m.flows);
    if !flows.is_empty() {
        out.push_str("\nFlows (provisional static flows)\n");
        out.push_str(&flows);
    }
    let bridges = render_bridges(&m.bridges);
    if !bridges.is_empty() {
        out.push_str("\nBridges\n");
        out.push_str(&bridges);
    }
    let clocks = render_clocks(&m.clocks);
    if !clocks.is_empty() {
        out.push_str("\nClocks\n");
        out.push_str(&clocks);
    }
    out
}

fn source(s: PrimarySource) -> &'static str {
    match s {
        PrimarySource::Locked => "locked by configuration",
        PrimarySource::Selected => "selected",
        PrimarySource::Configured => "configured",
        PrimarySource::Automatic => "automatic",
        PrimarySource::Fallback => "fallback",
        _ => "unknown",
    }
}

/// One line: the primary interface and how it was chosen (ADR 0012).
#[must_use]
pub fn render_primary(n: &NetworkStatus) -> String {
    let primary = n.primary.as_deref().unwrap_or("none");
    let mut line = format!("{primary} ({})", source(n.source));
    if n.source == PrimarySource::Fallback {
        if let Some(sel) = &n.selected {
            let why = n
                .interfaces
                .iter()
                .find(|i| i.name == *sel)
                .map_or("not present", |i| {
                    i.reason.as_deref().unwrap_or("unavailable")
                });
            let _ = write!(line, "; selected `{sel}` is not usable: {why}");
        }
    }
    line
}

/// The `network show` output.
#[must_use]
pub fn render_network(n: &NetworkStatus) -> String {
    let n = &cleaned(n);
    let mut out = format!("Primary interface: {}\n", render_primary(n));
    if let Some(sel) = &n.selected {
        let _ = writeln!(out, "Selected: {sel}");
    }
    if n.locked {
        out.push_str("Selection is locked by configuration.\n");
    }
    let mut rows = vec![vec![
        "INTERFACE".to_owned(),
        "MAC".into(),
        "IPV4".into(),
        "LINK".into(),
        "TYPE".into(),
        "USABLE".into(),
    ]];
    for i in &n.interfaces {
        let kind = if i.wireless {
            "wi-fi"
        } else if i.is_virtual {
            "virtual"
        } else {
            "wired"
        };
        let mark = if n.primary.as_deref() == Some(i.name.as_str()) {
            " *"
        } else {
            ""
        };
        rows.push(vec![
            format!("{}{mark}", i.name),
            i.mac.clone().unwrap_or_else(|| "-".into()),
            i.ipv4.clone().unwrap_or_else(|| "-".into()),
            if i.carrier { "up" } else { "down" }.into(),
            kind.into(),
            if i.usable {
                "yes".to_owned()
            } else {
                format!("no: {}", i.reason.as_deref().unwrap_or("unknown"))
            },
        ]);
    }
    out.push('\n');
    out.push_str(&table(&rows));
    out
}

/// The `status` output.
#[must_use]
pub fn render_status(s: &Status) -> String {
    let s = &cleaned(s);
    let mut out = format!("tessera {}\n", s.version);
    if s.devices.is_empty() {
        out.push_str("\nNo devices.\n");
    } else {
        let mut rows = vec![vec![
            "DEVICE".to_owned(),
            "NAME".into(),
            "RATE".into(),
            "BITS".into(),
            "LATENCY".into(),
            "RX".into(),
            "TX".into(),
            "LOCKED".into(),
            "ACCESS".into(),
        ]];
        for d in &s.devices {
            rows.push(vec![
                d.key.0.clone(),
                d.name.clone(),
                format!("{} Hz", d.sample_rate.0),
                d.encoding.0.to_string(),
                format!("{} us", d.latency.0),
                d.rx_channels.len().to_string(),
                d.tx_channels.len().to_string(),
                dash_if_empty(&d.locked),
                access(d.access_locked).into(),
            ]);
        }
        out.push('\n');
        out.push_str(&table(&rows));
        for d in &s.devices {
            for line in format_pins(d) {
                let _ = writeln!(out, "{}: {line}", d.key.0);
            }
        }
    }
    let subs = subscription_rows(&s.devices);
    if subs.len() > 1 {
        out.push_str("\nSubscriptions\n");
        out.push_str(&table(&subs));
    }
    if let Some(n) = &s.network {
        let _ = writeln!(out, "\nPrimary interface: {}", render_primary(n));
    }
    if let Some(c) = &s.clock_settings {
        let _ = writeln!(out, "\nClock settings: {}", render_clock_settings(c));
    }
    let clocks = render_clocks(&s.clocks);
    if !clocks.is_empty() {
        out.push_str("\nClocks\n");
        out.push_str(&clocks);
    }
    let bridges = render_bridges(&s.bridges);
    if !bridges.is_empty() {
        out.push_str("\nBridges\n");
        out.push_str(&bridges);
    }
    if !s.warnings.is_empty() {
        out.push_str("\nWarnings\n");
        for w in &s.warnings {
            let _ = writeln!(out, "  - {w}");
        }
    }
    out
}

/// One device in detail (channels and labels).
#[must_use]
pub fn render_device(d: &DeviceStatus) -> String {
    let d = &cleaned(d);
    let mut out = format!(
        "{} ({})\n  rate {} Hz, {} bit, latency {} us, locked: {}, access: {}\n",
        d.key.0,
        d.name,
        d.sample_rate.0,
        d.encoding.0,
        d.latency.0,
        dash_if_empty(&d.locked),
        access(d.access_locked)
    );
    if let Some(f) = &d.formats {
        let join = |v: Vec<String>| v.join(", ");
        let _ = writeln!(
            out,
            "  can be set to: {} Hz; {} bit",
            join(f.sample_rates.iter().map(ToString::to_string).collect()),
            join(f.encodings.iter().map(ToString::to_string).collect())
        );
    }
    for line in format_pins(d) {
        let _ = writeln!(out, "  {line}");
    }
    for (title, list) in [
        ("RX channels", &d.rx_channels),
        ("TX channels", &d.tx_channels),
    ] {
        let _ = writeln!(out, "  {title}:");
        for c in list {
            let _ = writeln!(out, "    {:>3}  {}{}", c.channel, c.label, channel_level(c));
        }
    }
    let subs = subscription_rows(std::slice::from_ref(d));
    if subs.len() > 1 {
        out.push_str("  Subscriptions:\n");
        for l in table(&subs).lines() {
            let _ = writeln!(out, "    {l}");
        }
    }
    out
}

/// A channel's gain and mute after its label, when not the defaults:
/// `  [muted, -6 dB, gain set by the sc3 fader]` (and the same for the RX
/// output level); empty at unity, unmuted.
fn channel_level(c: &tessera_control_types::ChannelStatus) -> String {
    let mut parts = Vec::new();
    if c.muted {
        parts.push("muted".to_owned());
    }
    if c.gain != Gain::default() || c.gain_owner.is_some() {
        parts.push(gain_words(c.gain, Style::Ascii));
    }
    if let Some(s) = &c.gain_owner {
        parts.push(format!("gain set by the {s} fader"));
    }
    if c.output_muted {
        parts.push("output muted".to_owned());
    }
    if c.output_gain != Gain::default() || c.output_gain_owner.is_some() {
        parts.push(format!(
            "output {}",
            gain_words(c.output_gain, Style::Ascii)
        ));
    }
    if let Some(s) = &c.output_gain_owner {
        parts.push(format!("output level set by the {s} fader"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("  [{}]", parts.join(", "))
    }
}

/// Why the device's sample rate or encoding cannot change now, one line
/// each (`device-settings.md` §3.3 item 10): the bridge that pins it, or the
/// configuration lock.
fn format_pins(d: &DeviceStatus) -> Vec<String> {
    let Some(f) = &d.formats else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    if let Some(pin) = &f.sample_rate_pin {
        lines.push(format!(
            "sample rate fixed at {} Hz: {pin}",
            d.sample_rate.0
        ));
    }
    if let Some(pin) = &f.encoding_pin {
        lines.push(format!("encoding fixed at {} bit: {pin}", d.encoding.0));
    }
    lines
}

/// The device access lock in a status: `locked` or `open` (not the
/// configuration's `LOCKED` keys).
fn access(locked: bool) -> &'static str {
    if locked { "locked" } else { "open" }
}

fn origin(o: &Origin) -> String {
    match o {
        Origin::Protocol { peer } => format!("network peer {peer}"),
        Origin::Cli { uid } => format!("cli uid {uid}"),
        Origin::Config => "configuration".into(),
        Origin::Api { principal } => format!("api {principal}"),
        Origin::Internal => "daemon".into(),
        Origin::Surface { name } => format!("surface {name}"),
        other => format!("{other:?}"),
    }
}

fn dir(d: Direction) -> &'static str {
    match d {
        Direction::Rx => "rx",
        Direction::Tx => "tx",
    }
}

fn sub_line(s: &SubscriptionStatus) -> String {
    format!(
        "rx {} <- {} @ {}: {}",
        s.rx_channel,
        s.tx_channel,
        s.tx_device,
        sub_state(&s.state)
    )
}

/// One line per event, for `watch`.
#[must_use]
pub fn render_event(e: &Event) -> String {
    let e = &cleaned(e);
    match e {
        Event::LockedValueRefused {
            field,
            origin: o,
            attempted,
            ..
        } => format!(
            "{field}: locked change refused{} ({})",
            attempted
                .as_ref()
                .map_or_else(String::new, |v| format!("; attempted {v}")),
            origin(o)
        ),
        Event::RuntimeValueChanged {
            field, origin: o, ..
        } => format!(
            "{field}: runtime value changed; queued for state.toml ({})",
            origin(o)
        ),
        Event::ProtocolFlowsChanged {
            flows, origin: o, ..
        } => format!(
            "controller flows: {} ({})",
            if flows.is_empty() {
                "none".into()
            } else {
                flows.join(", ")
            },
            origin(o)
        ),
        Event::SubscriptionChangedWithOrigin {
            device,
            status,
            origin: o,
            ..
        } => format!(
            "{}: subscription {} ({})",
            device.0,
            sub_line(status),
            origin(o)
        ),
        Event::DeviceRenamed {
            device,
            name,
            origin: o,
        } => {
            format!("{}: renamed to \"{name}\" ({})", device.0, origin(o))
        }
        Event::ChannelLabelChanged {
            device,
            direction,
            channel,
            label,
            origin: o,
        } => format!(
            "{}: {} channel {channel} label is now \"{label}\" ({})",
            device.0,
            dir(*direction),
            origin(o)
        ),
        Event::SubscriptionChanged { device, status } => {
            format!("{}: subscription {}", device.0, sub_line(status))
        }
        Event::SubscriptionRemoved {
            device,
            rx_channel,
            origin: o,
        } => {
            format!(
                "{}: subscription on rx {rx_channel} removed ({})",
                device.0,
                origin(o)
            )
        }
        Event::DeviceSettingsChanged {
            device,
            sample_rate,
            encoding,
            latency,
            origin: o,
        } => format!(
            "{}: settings now {} Hz, {} bit, {} us ({})",
            device.0,
            sample_rate.0,
            encoding.0,
            latency.0,
            origin(o)
        ),
        Event::ClockStateChanged { status } => format!(
            "clock {}: {}, offset {}, rate {} ppb",
            status.interface,
            clock_state(status.state),
            status.offset_ns.map_or("-".into(), |o| format!("{o} ns")),
            status.rate_ppb
        ),
        Event::ClockSettingsChanged {
            role,
            priority,
            origin: o,
        } => {
            format!(
                "clock settings: role {role:?}, priority {priority} ({})",
                origin(o)
            )
        }
        Event::NetworkSettingsChanged {
            selected,
            origin: o,
        } => format!(
            "primary interface selection: {} ({})",
            selected.as_deref().unwrap_or("none"),
            origin(o)
        ),
        Event::ConfigReloadedWithOrigin { origin: o } => {
            format!("configuration reloaded ({})", origin(o))
        }
        Event::ConfigReloaded => "configuration reloaded".into(),
        Event::DeviceChannelsChanged {
            device,
            rx_channels,
            tx_channels,
            origin: o,
        } => format!(
            "{}: channel counts now rx {rx_channels}, tx {tx_channels} ({})",
            device.0,
            origin(o)
        ),
        Event::AccessLockChanged {
            device,
            locked,
            origin: o,
        } => format!(
            "{}: access lock {} ({})",
            device.0,
            if *locked { "set" } else { "removed" },
            origin(o)
        ),
        Event::ChannelMuteChanged {
            target,
            muted,
            origin: o,
        } => format!(
            "{}: {} ({})",
            channel_set_words(target, Style::Ascii),
            if *muted { "muted" } else { "unmuted" },
            origin(o)
        ),
        Event::ChannelGainChanged {
            target,
            gain,
            origin: o,
        } => format!(
            "{}: gain {} ({})",
            channel_set_words(target, Style::Ascii),
            gain_words(*gain, Style::Ascii),
            origin(o)
        ),
        Event::SurfacesChanged { origin: o } => format!("control surfaces changed ({})", origin(o)),
        Event::OutputGainChanged {
            target,
            gain,
            origin: o,
        } => format!(
            "{}: output level {} ({})",
            channel_set_words(target, Style::Ascii),
            gain_words(*gain, Style::Ascii),
            origin(o)
        ),
        Event::OutputMuteChanged {
            target,
            muted,
            origin: o,
        } => format!(
            "{}: output {} ({})",
            channel_set_words(target, Style::Ascii),
            if *muted { "muted" } else { "unmuted" },
            origin(o)
        ),
        Event::RouterChanged { origin: o } => format!("host router changed ({})", origin(o)),
        Event::CrosspointGainChanged {
            crosspoints,
            gain,
            origin: o,
        } => format!(
            "{}: gain {} ({})",
            crosspoint_words(crosspoints),
            gain_words(*gain, Style::Ascii),
            origin(o)
        ),
        Event::CrosspointMuteChanged {
            crosspoints,
            muted,
            origin: o,
        } => format!(
            "{}: {} ({})",
            crosspoint_words(crosspoints),
            if *muted { "muted" } else { "unmuted" },
            origin(o)
        ),
        Event::PortGainChanged {
            ports,
            gain,
            origin: o,
        } => format!(
            "{}: level {} ({})",
            port_list_words(ports),
            gain_words(*gain, Style::Ascii),
            origin(o)
        ),
        Event::PortMuteChanged {
            ports,
            muted,
            origin: o,
        } => format!(
            "{}: {} ({})",
            port_list_words(ports),
            if *muted { "muted" } else { "unmuted" },
            origin(o)
        ),
        Event::HostVolumeChanged {
            gain,
            muted,
            origin: o,
        } => format!(
            "host volume: {}{} ({})",
            gain.map_or_else(
                || "not saved (left alone)".into(),
                |g| gain_words(g, Style::Ascii)
            ),
            if *muted { ", muted" } else { ", unmuted" },
            origin(o)
        ),
        Event::MixerChanged { id, origin: o } => {
            format!("mixer panel {id} changed ({})", origin(o))
        }
        other => format!("{other:?}"),
    }
}

/// Router ports in a `watch` line: `router port in:CQ:15` or `router ports
/// in:CQ:15, in:CQ:16`.
fn port_list_words(ports: &[RouterPort]) -> String {
    let names: Vec<String> = ports.iter().map(crate::cli::port_words).collect();
    format!(
        "router port{} {}",
        if names.len() == 1 { "" } else { "s" },
        names.join(", ")
    )
}

/// Crosspoints in a `watch` line: `crosspoint in:CQ:15 -> host:main:1`, or
/// `crosspoints ...` with several separated by commas.
fn crosspoint_words(crosspoints: &[CrosspointKey]) -> String {
    let names: Vec<String> = crosspoints
        .iter()
        .map(|x| {
            format!(
                "{} -> {}",
                crate::cli::port_words(&x.source),
                crate::cli::port_words(&x.destination)
            )
        })
        .collect();
    format!(
        "crosspoint{} {}",
        if names.len() == 1 { "" } else { "s" },
        names.join(", ")
    )
}

/// The `router show` output: the ports with their levels, then the grid.
#[must_use]
pub fn render_router(r: &tessera_control_types::RouterReport) -> String {
    use tessera_control_types::CrosspointOrigin as O;
    let r = &cleaned(r);
    let style = Style::Ascii;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Grid: {}{}",
        if r.saved { "saved" } else { "default" },
        if r.read_only {
            " (read only: this tesserad cannot change it)"
        } else {
            ""
        }
    );
    let mut rows = vec![vec![
        "PORT".to_owned(),
        "LABEL".into(),
        "LEVEL".into(),
        "MUTE".into(),
        "NOTE".into(),
    ]];
    for p in &r.ports {
        let mut note = Vec::new();
        if !p.available {
            note.push("not available".to_owned());
        }
        if let Some(o) = &p.gain_owner {
            note.push(format!("set by surface {o}"));
        }
        rows.push(vec![
            crate::cli::port_words(&p.port),
            p.label.clone(),
            gain_words(p.gain, style),
            if p.muted { "muted" } else { "" }.into(),
            note.join(", "),
        ]);
    }
    out.push_str(&table(&rows));
    if r.crosspoints.is_empty() {
        out.push_str("No crosspoints.\n");
        return out;
    }
    let mut rows = vec![vec![
        "SOURCE".to_owned(),
        "DESTINATION".into(),
        "GAIN".into(),
        "MUTE".into(),
        "ORIGIN".into(),
        "NOTE".into(),
    ]];
    for x in &r.crosspoints {
        let origin = match &x.origin {
            O::Saved => "saved".to_owned(),
            O::Default => "default".into(),
            O::Configured => "configured".into(),
            O::Monitor => "monitor".into(),
            O::Bridge { name } => format!("bridge {name}"),
            _ => "?".into(),
        };
        let mut note = Vec::new();
        if let Some(p) = &x.problem {
            note.push(format!("inactive: {p}"));
        }
        if let Some(o) = &x.gain_owner {
            note.push(format!("gain set by surface {o}"));
        }
        rows.push(vec![
            crate::cli::port_words(&x.crosspoint.source),
            crate::cli::port_words(&x.crosspoint.destination),
            gain_words(x.crosspoint.gain, style),
            if x.crosspoint.muted { "muted" } else { "" }.into(),
            origin,
            note.join(", "),
        ]);
    }
    out.push_str(&table(&rows));
    out
}

/// The `volume show` output.
#[must_use]
pub fn render_host_volume(v: &tessera_control_types::HostVolumeStatus) -> String {
    let v = &cleaned(v);
    let style = Style::Ascii;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Host volume: card {}, control {}, range {}{}{} dB",
        v.card,
        v.control,
        db_number(v.min_mdb, style),
        style.ellipsis(),
        db_number(v.max_mdb, style)
    );
    let _ = writeln!(
        out,
        "Saved: {}{}",
        v.gain
            .map_or_else(|| "never set (left alone)".into(), |g| gain_words(g, style)),
        if v.muted { ", muted" } else { "" }
    );
    match &v.reading {
        Some(r) => {
            let _ = writeln!(
                out,
                "Reads: {}{}",
                gain_words(r.gain, style),
                if r.muted { ", muted" } else { "" }
            );
        }
        None => out.push_str("Reads: -\n"),
    }
    if let Some(o) = &v.gain_owner {
        let _ = writeln!(out, "Set by the {o} fader");
    }
    if let Some(p) = &v.problem {
        let _ = writeln!(out, "Problem: {p}");
    }
    out
}

/// The `mixer list` output.
#[must_use]
pub fn render_mixers(m: &[tessera_control_types::MixerSpec]) -> String {
    let m = &cleaned(&m.to_vec());
    let mut rows = vec![vec!["ID".to_owned(), "TITLE".into(), "STRIPS".into()]];
    for p in m {
        rows.push(vec![
            p.id.clone(),
            p.title.clone(),
            p.strips.len().to_string(),
        ]);
    }
    table(&rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_control_types::{
        Channel, ChannelStatus, DeviceKey, Encoding, LatencyUs, SampleRate,
    };

    fn ch(n: u16) -> Channel {
        Channel::new(n).unwrap()
    }

    fn status() -> Status {
        let dev = DeviceStatus::new(
            DeviceKey("main".into()),
            "studio",
            SampleRate(48_000),
            Encoding(24),
            LatencyUs(4_000),
        )
        .with_rx_channels(vec![ChannelStatus::new(ch(1), "RX 1")])
        .with_tx_channels(vec![ChannelStatus::new(ch(1), "Vox \"A\"")])
        .with_subscriptions(vec![SubscriptionStatus::new(
            ch(1),
            "Out 1",
            "stagebox",
            SubscriptionState::NoMedia,
        )])
        .with_locked(vec!["name".into()]);
        Status::new("0.0.0")
            .with_devices(vec![dev])
            .with_clocks(vec![ClockStatus::new(
                "lo",
                ClockState::FreeRunning,
                Timestamping::Software,
            )])
            .with_warnings(vec!["careful".into()])
    }

    #[test]
    fn bridges_show_in_status_and_metrics() {
        let mut b = BridgeStatus::new(
            "speaker",
            DeviceKey("main".into()),
            "hw:Loopback,0,0",
            BridgeDirection::Playback,
            BridgeState::Reopening,
        );
        b.channels = 2;
        b.fill_frames = 1440;
        b.correction_ppb = -12_345;
        b.trend_ppb = -12_000;
        b.ratio = 0.999_987_655;
        b.xruns = 3;
        b.reopens = 2;
        b.rt_degraded = true;
        b.last_error = Some("cannot open the card: open: busy\u{1b}[2J".into());
        let t = render_status(&status().with_bridges(vec![b.clone()]));
        for needle in [
            "Bridges",
            "speaker",
            "playback",
            "1..2",
            "hw:Loopback,0,0",
            "reopening (no rt)",
            "1440",
            "-12.3",
            "-12.0",
            "0.999987655",
            "speaker: cannot open the card: open: busy",
        ] {
            assert!(t.contains(needle), "missing {needle} in\n{t}");
        }
        assert!(!t.contains('\u{1b}'), "escape sequences are neutralised");
        let m = Metrics::default().with_bridges(vec![b]);
        assert!(render_metrics(&m).contains("speaker"));
        assert!(!render_status(&status()).contains("Bridges"));
        assert!(t.contains("POS ERR") && t.contains("SLIPS") && t.contains("SHORT"));
        // An unknown channel range is a dash, not a made-up range.
        let mut u = BridgeStatus::new(
            "x",
            DeviceKey("gone".into()),
            "hw:0",
            BridgeDirection::Capture,
            BridgeState::Failed,
        );
        u.channels = 0;
        let t = render_bridges(&[u]);
        assert!(!t.contains("1..1"), "{t}");
    }

    #[test]
    fn table_aligns_and_trims() {
        let t = table(&[
            vec!["A".into(), "BB".into()],
            vec!["long".into(), "x".into()],
        ]);
        assert_eq!(t, "A     BB\nlong  x\n");
    }

    #[test]
    fn status_mentions_everything() {
        let t = render_status(&status());
        for needle in [
            "main",
            "studio",
            "48000 Hz",
            "4000 us",
            "name",
            "Out 1 @ stagebox",
            "no media",
            "free-running",
            "careful",
        ] {
            assert!(t.contains(needle), "missing {needle} in\n{t}");
        }
    }

    #[test]
    fn metrics_show_the_engine_and_the_flows() {
        use tessera_control_types::{
            AudioMetrics, DaemonMetrics, EngineMetrics, FlowMetrics, RxFlowMetrics, TxFlowMetrics,
        };
        let mut e = EngineMetrics::new();
        e.ticks = 1234;
        e.rt_degraded = 1;
        let mut rx = FlowMetrics::new("from-a", DeviceKey("main".into()), Direction::Rx);
        rx.packets = 77;
        let mut r = RxFlowMetrics::new();
        r.gap_events = 3;
        r.max_lateness_us = 912;
        rx.rx = Some(r);
        let mut tx = FlowMetrics::new("to-b", DeviceKey("main".into()), Direction::Tx);
        let mut t = TxFlowMetrics::new();
        t.late_skips = 2;
        tx.tx = Some(t);
        let m = Metrics::new(DaemonMetrics::new(), AudioMetrics::new(), Vec::new())
            .with_engine(e)
            .with_flows(vec![rx, tx]);
        let text = render_metrics(&m);
        for needle in [
            "engine.ticks",
            "1234",
            "engine.rt_degraded",
            "from-a",
            "to-b",
            "912 us",
            "Flows",
        ] {
            assert!(text.contains(needle), "missing {needle} in\n{text}");
        }
        // No flows: no flows section.
        let none = Metrics::new(DaemonMetrics::new(), AudioMetrics::new(), Vec::new());
        assert!(!render_metrics(&none).contains("Flows"));
    }

    #[test]
    fn empty_status_says_so() {
        assert!(render_status(&Status::new("0")).contains("No devices."));
    }

    #[test]
    fn events_render_one_line() {
        let e = Event::DeviceRenamed {
            device: DeviceKey("main".into()),
            name: "x".into(),
            origin: Origin::Cli { uid: 1000 },
        };
        assert_eq!(render_event(&e), "main: renamed to \"x\" (cli uid 1000)");
        assert_eq!(
            render_event(&Event::ConfigReloaded),
            "configuration reloaded"
        );
        assert_eq!(
            render_event(&Event::DeviceChannelsChanged {
                device: DeviceKey("main".into()),
                rx_channels: 8,
                tx_channels: 2,
                origin: Origin::Cli { uid: 1000 },
            }),
            "main: channel counts now rx 8, tx 2 (cli uid 1000)"
        );
        // Names come from the network: no escape sequence reaches a terminal.
        let line = render_event(&Event::DeviceChannelsChanged {
            device: DeviceKey("m\u{1b}[2J\nfake".into()),
            rx_channels: 1,
            tx_channels: 1,
            origin: Origin::Api {
                principal: "p\u{202e}".into(),
            },
        });
        assert!(!line.contains('\u{1b}') && !line.contains('\n') && !line.contains('\u{202e}'));
        assert!(line.contains("\\u{1b}"), "{line}");
    }
    #[test]
    fn host_audit_events_are_readable_and_sanitized() {
        let refused = Event::LockedValueRefused {
            field: "flows".into(),
            origin: Origin::Protocol {
                peer: "controller".into(),
            },
            attempted: None,
        };
        let line = render_event(&refused);
        assert!(
            line.contains("flows")
                && line.contains("locked change refused")
                && line.contains("controller")
        );
        let changed = Event::RuntimeValueChanged {
            field: "clock.priority".into(),
            origin: Origin::Cli { uid: 1000 },
        };
        assert!(render_event(&changed).contains("clock.priority: runtime value changed"));
        let escaped = Event::LockedValueRefused {
            field: "flows".into(),
            origin: Origin::Internal,
            attempted: Some("bad\n\u{1b}[2J".into()),
        };
        let line = render_event(&escaped);
        assert!(!line.contains('\n') && !line.contains('\u{1b}'));
    }

    #[test]
    fn a_fixed_rate_or_encoding_says_why() {
        use tessera_control_types::{FormatOptions, SettingPin};
        // `device-settings.md` §3.3 item 10.
        let mut s = status();
        s.devices[0].formats = Some(Box::new(
            FormatOptions::new(vec![48_000], vec![16, 24, 32]).with_sample_rate_pin(Some(
                SettingPin::Bridge {
                    name: "speaker\u{1b}[2J".into(),
                    sample_rate: 48_000,
                },
            )),
        ));
        let t = render_status(&s);
        assert!(
            t.contains("main: sample rate fixed at 48000 Hz: bridge `speaker"),
            "{t}"
        );
        assert!(!t.contains('\u{1b}'), "escape sequences are neutralised");
        assert!(!t.contains("encoding fixed"), "{t}");
        let d = render_device(&s.devices[0]);
        assert!(
            d.contains("  can be set to: 48000 Hz; 16, 24, 32 bit\n"),
            "{d}"
        );
        assert!(d.contains("  sample rate fixed at 48000 Hz: bridge"), "{d}");
        s.devices[0].formats = Some(Box::new(
            FormatOptions::new(vec![44_100, 48_000], vec![24])
                .with_encoding_pin(Some(SettingPin::Configuration)),
        ));
        let d = render_device(&s.devices[0]);
        assert!(
            d.contains("  encoding fixed at 24 bit: locked by the configuration"),
            "{d}"
        );
        // A daemon that does not report the options: nothing extra.
        s.devices[0].formats = None;
        assert!(!render_device(&s.devices[0]).contains("can be set to"));
    }
}

#[cfg(test)]
mod surface_tests {
    use super::*;
    use tessera_control_types::{
        Channel, ChannelStatus, ControlState, DetectedDevice, DeviceKey, SurfaceSource, SurfaceSpec,
    };

    fn set(dir: Direction, ch: &[u16]) -> ChannelSet {
        ChannelSet {
            device: DeviceKey("main".into()),
            direction: dir,
            channels: ch.iter().map(|n| Channel::new(*n).unwrap()).collect(),
        }
    }

    fn control(id: &str, kind: ControlKind, motorized: bool) -> SurfaceControl {
        SurfaceControl {
            id: id.into(),
            label: None,
            kind,
            report_id: Some(1),
            offset: Some(0),
            bits: None,
            max: None,
            inverted: false,
            active_low: false,
            motorized,
            code: None,
            midi: None,
        }
    }

    #[test]
    fn midi_addresses_and_bridges_read_naturally() {
        let m = MidiAddress::new(MidiKind::Nrpn, 1, (0x40 << 7) | 3);
        assert_eq!(midi_words(&m), "NRPN 40/03 (8195), ch 1");
        let cc = MidiAddress::new(MidiKind::ControlChange, 2, 7);
        assert_eq!(midi_words(&cc), "CC 7, ch 2");
        assert_eq!(rates_words(&[44_100, 48_000]), "44.1/48 kHz");
        assert_eq!(
            bridge_mapping_words("SC3", "main", BridgeDirection::Capture, 3, 2, Style::Ascii),
            "SC3 in 1-2 -> main TX 3-4"
        );
        assert_eq!(
            bridge_mapping_words("SC3", "main", BridgeDirection::Playback, 1, 1, Style::Ascii),
            "main RX 1 -> SC3 out 1"
        );
        assert_eq!(
            bridge_slice_words(
                "CQ",
                "main",
                BridgeDirection::Capture,
                1,
                2,
                15,
                Style::Ascii
            ),
            "CQ in 15-16 -> main TX 1-2"
        );
        let mut c = control("ip1", ControlKind::Fader, true);
        c.midi = Some(m);
        let mut b = SurfaceBinding::new(
            "ip1",
            BindingAction::Gain,
            ChannelSet {
                device: tessera_control_types::DeviceKey("main".into()),
                direction: Direction::Tx,
                channels: vec![Channel::new(1).unwrap()],
            },
        );
        b.curve = vec![tessera_control_types::CurvePoint {
            raw: 12_544,
            mdb: 0,
        }];
        assert_eq!(
            binding_words(&b, Some(&c), true, Style::Ascii),
            "Gain: main TX 1, 0..0 dB (the desk's fader law), motorized"
        );
        let mut out = SurfaceBinding::new(
            "main",
            BindingAction::OutputGain,
            ChannelSet {
                device: tessera_control_types::DeviceKey("speaker".into()),
                direction: Direction::Rx,
                channels: vec![Channel::new(1).unwrap(), Channel::new(2).unwrap()],
            },
        );
        out.max_mdb = 10_000;
        assert_eq!(
            binding_words(&out, Some(&c), true, Style::Typographic),
            "Output: speaker RX 1\u{2013}2, \u{2212}60\u{2026}+10 dB"
        );
    }

    #[test]
    fn gains_and_channel_runs_read_naturally() {
        assert_eq!(db_number(-6000, Style::Ascii), "-6");
        assert_eq!(db_number(-6500, Style::Typographic), "\u{2212}6.5");
        assert_eq!(db_number(3000, Style::Ascii), "+3");
        assert_eq!(db_number(0, Style::Ascii), "0");
        assert_eq!(db_number(-20, Style::Ascii), "0");
        assert_eq!(db_number(-250, Style::Ascii), "-0.3");
        assert_eq!(gain_words(Gain::Off, Style::Ascii), "off");
        assert_eq!(gain_words(Gain::MilliDb(-60_000), Style::Ascii), "-60 dB");
        let s = set(Direction::Rx, &[1, 2, 3, 5, 7, 8]);
        assert_eq!(channel_runs(&s.channels, Style::Ascii), "1-3,5,7-8");
        assert_eq!(
            channel_runs(&s.channels, Style::Typographic),
            "1\u{2013}3, 5, 7\u{2013}8"
        );
        assert_eq!(channel_set_words(&s, Style::Ascii), "main RX 1-3,5,7-8");
    }

    #[test]
    fn bindings_are_described_in_words() {
        let mut mute = SurfaceBinding::new("m", BindingAction::Mute, set(Direction::Tx, &[1, 2]));
        assert_eq!(
            binding_words(&mute, None, true, Style::Typographic),
            "Mute: main TX 1\u{2013}2, toggle"
        );
        mute.mode = ButtonMode::Momentary;
        assert_eq!(
            binding_words(&mute, None, true, Style::Ascii),
            "Mute: main TX 1-2, hold to mute"
        );
        mute.mode = ButtonMode::PushToTalk;
        assert!(binding_words(&mute, None, true, Style::Ascii).ends_with("push to talk"));
        let gain = SurfaceBinding::new("f", BindingAction::Gain, set(Direction::Rx, &[1, 2]));
        let fader = control("f", ControlKind::Fader, false);
        assert_eq!(
            binding_words(&gain, Some(&fader), true, Style::Typographic),
            "Gain: main RX 1\u{2013}2, \u{2212}60\u{2026}0 dB, owns gain"
        );
        assert_eq!(
            binding_words(&gain, Some(&fader), false, Style::Ascii),
            "Gain: main RX 1-2, -60..0 dB, owns gain while connected"
        );
        let motor = control("f", ControlKind::Fader, true);
        assert!(binding_words(&gain, Some(&motor), true, Style::Ascii).ends_with(", motorized"));
        let enc = control("f", ControlKind::Encoder, false);
        assert_eq!(
            binding_words(&gain, Some(&enc), true, Style::Ascii),
            "Gain: main RX 1-2, +1 dB per step"
        );
    }

    fn report() -> SurfacesReport {
        let source = SurfaceSource::Hidraw {
            vendor_id: 0x3142,
            product_id: 0x0c33,
            interface: Some(3),
        };
        let spec = SurfaceSpec::new(
            "sc3",
            source.clone(),
            vec![
                control("mic-fader", ControlKind::Fader, false),
                control("mic-mute", ControlKind::Button, false),
            ],
            vec![SurfaceBinding::new(
                "mic-fader",
                BindingAction::Gain,
                set(Direction::Tx, &[1, 2]),
            )],
        );
        let mut device = DetectedDevice::new(
            "hid:3142:0c33:3",
            DetectedKind::Hid,
            "fifine SC3",
            "/dev/hidraw3",
        );
        device.vendor_id = Some(0x3142);
        device.product_id = Some(0x0c33);
        device.interface = Some(3);
        device.used_by = Some("sc3".into());
        device.source = Some(source);
        let mut status = SurfaceStatus::new(spec);
        status.connected = true;
        status.device = Some("hid:3142:0c33:3".into());
        status.controls = vec![
            ControlState::new(
                "mic-fader",
                Some(ControlValue::Fader {
                    value: 128,
                    max: 255,
                }),
            ),
            ControlState::new("mic-mute", Some(ControlValue::Button { pressed: true })),
        ];
        let mut report = SurfacesReport::default();
        report.detected = vec![device];
        report.surfaces = vec![status];
        report
    }

    #[test]
    fn the_surfaces_report_lists_devices_controls_and_bindings() {
        let text = render_surfaces(&report());
        assert!(text.contains("hid:3142:0c33:3"), "{text}");
        assert!(text.contains("3142:0c33"), "{text}");
        assert!(text.contains("/dev/hidraw3"), "{text}");
        assert!(
            text.contains("Surface sc3: connected (hid:3142:0c33:3)"),
            "{text}"
        );
        assert!(text.contains("mic-fader  fader  128/255"), "{text}");
        assert!(text.contains("mic-mute  button  pressed"), "{text}");
        assert!(
            text.contains("Gain: main TX 1-2, -60..0 dB, owns gain"),
            "{text}"
        );
        let mut r = report();
        r.surfaces[0].connected = false;
        r.surfaces[0].problem = Some("permission denied".into());
        let text = render_surfaces(&r);
        assert!(text.contains("not connected: permission denied"), "{text}");
        assert!(text.contains("owns gain while connected"), "{text}");
        let text = render_surfaces(&SurfacesReport::default());
        assert!(text.contains("No devices detected."));
        assert!(text.contains("No control surfaces configured."));
    }

    #[test]
    fn channel_lines_show_gain_mute_and_the_owner_only_when_set() {
        let c = ChannelStatus::new(Channel::new(1).unwrap(), "Mic");
        assert_eq!(channel_level(&c), "");
        let mut c = c;
        c.muted = true;
        c.gain = Gain::MilliDb(-6000);
        c.gain_owner = Some("sc3".into());
        assert_eq!(
            channel_level(&c),
            "  [muted, -6 dB, gain set by the sc3 fader]"
        );
        // An RX output level owned by a fader shows its owner too, even at unity.
        let mut c = ChannelStatus::new(Channel::new(1).unwrap(), "Speaker L");
        c.output_gain_owner = Some("cq".into());
        assert_eq!(
            channel_level(&c),
            "  [output 0 dB, output level set by the cq fader]"
        );
    }

    #[test]
    fn router_mixer_and_host_volume_events_read_as_sentences() {
        let cq = |n: u16| RouterPort::CardIn {
            card: "hw:CARD=CQ,DEV=0".into(),
            channel: Channel::new(n).unwrap(),
        };
        let main = |n: u16| RouterPort::HostOut {
            device: DeviceKey("main".into()),
            channel: Channel::new(n).unwrap(),
        };
        let surface = Origin::Surface { name: "cq".into() };
        let cli = Origin::Cli { uid: 1000 };
        let one = vec![CrosspointKey::new(cq(15), main(1))];
        let two = vec![one[0].clone(), CrosspointKey::new(cq(16), main(2))];
        for (event, line) in [
            (
                Event::RouterChanged {
                    origin: cli.clone(),
                },
                "host router changed (cli uid 1000)",
            ),
            (
                Event::CrosspointGainChanged {
                    crosspoints: one,
                    gain: Gain::MilliDb(-6000),
                    origin: surface.clone(),
                },
                "crosspoint in:hw:CARD=CQ,DEV=0:15 -> host:main:1: gain -6 dB (surface cq)",
            ),
            (
                Event::CrosspointMuteChanged {
                    crosspoints: two,
                    muted: true,
                    origin: cli.clone(),
                },
                "crosspoints in:hw:CARD=CQ,DEV=0:15 -> host:main:1, in:hw:CARD=CQ,DEV=0:16 -> host:main:2: muted (cli uid 1000)",
            ),
            (
                Event::PortGainChanged {
                    ports: vec![cq(15), cq(16)],
                    gain: Gain::MilliDb(-3000),
                    origin: surface.clone(),
                },
                "router ports in:hw:CARD=CQ,DEV=0:15, in:hw:CARD=CQ,DEV=0:16: level -3 dB (surface cq)",
            ),
            (
                Event::PortMuteChanged {
                    ports: vec![cq(15)],
                    muted: false,
                    origin: cli.clone(),
                },
                "router port in:hw:CARD=CQ,DEV=0:15: unmuted (cli uid 1000)",
            ),
            (
                Event::HostVolumeChanged {
                    gain: Some(Gain::MilliDb(-20000)),
                    muted: true,
                    origin: surface,
                },
                "host volume: -20 dB, muted (surface cq)",
            ),
            (
                Event::HostVolumeChanged {
                    gain: None,
                    muted: false,
                    origin: Origin::Config,
                },
                "host volume: not saved (left alone), unmuted (configuration)",
            ),
            (
                Event::MixerChanged {
                    id: "speakers".into(),
                    origin: cli,
                },
                "mixer panel speakers changed (cli uid 1000)",
            ),
        ] {
            assert_eq!(render_event(&event), line);
        }
    }

    #[test]
    fn a_local_bridge_reads_as_router_ports() {
        let mut b = CardBridgeSpec::new(
            "dac",
            DeviceKey("main".into()),
            "hw:CARD=DAC,DEV=0",
            BridgeDirection::Playback,
            1,
            1,
        );
        b.local_only = true;
        b.card_first_channel = 3;
        assert_eq!(
            card_bridge_words(&b, Style::Ascii),
            "host router only -> hw:CARD=DAC,DEV=0 out 3"
        );
        let mut status = BridgeStatus::new(
            "dac",
            DeviceKey("main".into()),
            "hw:CARD=DAC,DEV=0",
            BridgeDirection::Playback,
            BridgeState::Running,
        );
        status.channels = 1;
        status.local_only = true;
        let text = render_bridges(&[status]);
        assert!(text.lines().nth(1).unwrap().contains(" router "), "{text}");
    }
}
