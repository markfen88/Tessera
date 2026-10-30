//! Human-readable rendering of status, events and clock information.

use std::fmt::Write as _;

use crate::sanitize::cleaned;

use tessera_control_types::{
    BridgeDirection, BridgeState, BridgeStatus, ClockRole, ClockSettings, ClockState, ClockStatus,
    DeviceStatus, Direction, Event, Metrics, NetworkStatus, Origin, PrimarySource, Status,
    SubscriptionState, SubscriptionStatus, Timestamping,
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
        } else {
            let last = u32::from(b.first_channel) + u32::from(b.channels) - 1;
            format!("{}..{last}", b.first_channel)
        };
        rows.push(vec![
            b.name.clone(),
            b.device.0.clone(),
            dir.to_owned(),
            channels,
            b.card.clone(),
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
            let _ = writeln!(out, "    {:>3}  {}", c.channel, c.label);
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
        other => format!("{other:?}"),
    }
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
