//! The audio page (`/audio`, provisional 0.x): the sound cards of this host
//! (DACs, USB interfaces, mixers) and how their audio reaches the network.
//!
//! * A card's inputs go to TX channels (a capture bridge) and RX channels
//!   play on its outputs (a playback bridge): `SetCardBridge`, with the
//!   resulting channel mapping in words.
//! * The bridges with their live state; those saved here can be switched
//!   off and removed (`SetCardBridge`, `RemoveCardBridge`); those of the
//!   configuration files are refused by the daemon (`Locked`), which the
//!   page explains.
//! * Local monitoring per device (`SetMonitor`): which local input (TX
//!   channel) each output (RX channel) also plays.
//!
//! Every change is a typed command; the daemon stays the only authority.
//! Everything works without the page's script.

use axum::Form;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use maud::{Markup, html};
use serde::Deserialize;
use tessera_control_types::{
    BridgeDirection, BridgeState, BridgeStatus, CardBridgeSpec, Channel, Command, CommandError,
    Crosspoint, DetectedDevice, DetectedKind, DeviceKey, DeviceStatus, Direction, Query, Reply,
    RequestBody, RouterPort, RouterReport, Status, SurfacesReport,
};
use tesseractl::client::ClientError;
use tesseractl::output::{self, Style};

use super::surfaces::{Question, ask, ident_ok, slug};
use super::{answer, back, error_box, khz, need, need_post, path_segment, status_of, view};
use crate::auth::Session;
use crate::{Shared, call, explain};

/// How often the page refreshes its live values, seconds.
const LIVE_EVERY: u32 = 2;
/// Card period of the low-latency option, frames (2 ms at 48 kHz).
pub(crate) const LOW_LATENCY_PERIOD_FRAMES: u32 = 96;

// ---- reading ---------------------------------------------------------------

async fn report_of(st: &Shared) -> Result<SurfacesReport, String> {
    match call(st, RequestBody::Query(Query::Surfaces)).await {
        Ok(Reply::Surfaces(r)) => Ok(r),
        Ok(other) => Err(format!("unexpected reply: {other:?}")),
        Err(e) => Err(explain(&e)),
    }
}

async fn bridges_of(st: &Shared) -> Result<Vec<BridgeStatus>, String> {
    match call(st, RequestBody::Query(Query::Bridges)).await {
        Ok(Reply::Bridges(b)) => Ok(b),
        Ok(other) => Err(format!("unexpected reply: {other:?}")),
        Err(e) => Err(explain(&e)),
    }
}

/// Everything the page shows.
struct Snapshot {
    cards: Vec<DetectedDevice>,
    bridges: Vec<BridgeStatus>,
    devices: Vec<DeviceStatus>,
    /// The host router, when the daemon reports one: once its grid is
    /// saved, local monitoring is part of it.
    router: Option<RouterReport>,
}

async fn snapshot(st: &Shared) -> Result<Snapshot, String> {
    let report = report_of(st).await?;
    let bridges = bridges_of(st).await?;
    let status: Status = status_of(st).await?;
    Ok(Snapshot {
        cards: report
            .detected
            .into_iter()
            .filter(|d| d.kind == DetectedKind::SoundCard)
            .collect(),
        bridges,
        devices: status.devices,
        router: crate::ports::router_of(st).await.ok(),
    })
}

/// Does bridge `b` use card `d`? Same PCM, or the PCM names the card.
fn on_card(b: &BridgeStatus, d: &DetectedDevice) -> bool {
    if d.pcm.as_deref() == Some(b.card.as_str()) {
        return true;
    }
    let tag = format!("CARD={}", d.node);
    b.card
        .split([':', ','])
        .any(|part| part == tag || part == d.node)
}

/// What the page calls a bridge's card: the detected card's name when it
/// is plugged in, else the PCM.
fn card_name<'a>(b: &'a BridgeStatus, cards: &'a [DetectedDevice]) -> &'a str {
    cards
        .iter()
        .find(|d| on_card(b, d))
        .map_or(b.card.as_str(), |d| d.name.as_str())
}

/// The channel mapping in words: `SC3 in 1–2 → speaker TX 1–2 (to the
/// network)`.
pub(crate) fn mapping_words(
    card: &str,
    device: &str,
    direction: BridgeDirection,
    first: u16,
    channels: u16,
    card_first: u16,
) -> String {
    let m = output::bridge_slice_words(
        card,
        device,
        direction,
        first,
        channels,
        card_first,
        Style::Typographic,
    );
    match direction {
        BridgeDirection::Capture => format!("{m} (to the network)"),
        _ => format!("{m} (from the network)"),
    }
}

/// `base`, or `base-2`, `base-3`, … whichever no bridge has yet.
fn unique_name(base: &str, bridges: &[BridgeStatus]) -> String {
    let taken = |n: &str| bridges.iter().any(|b| b.name == n);
    if !taken(base) {
        return base.to_owned();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !taken(c))
        .unwrap_or_else(|| base.to_owned())
}

/// The channels of a device in one direction already in use: carried by a
/// bridge, or (RX) subscribed to a network source.
fn used_channels(
    bridges: &[BridgeStatus],
    device: &DeviceStatus,
    direction: BridgeDirection,
) -> Vec<u16> {
    let mut used: Vec<u16> = bridges
        .iter()
        .filter(|b| b.device == device.key && b.direction == direction)
        .flat_map(|b| (0..b.channels).map(move |i| b.first_channel.saturating_add(i)))
        .collect();
    if direction == BridgeDirection::Playback {
        used.extend(device.subscriptions.iter().map(|s| s.rx_channel.get()));
    }
    used
}

/// The first device channel from which `channels` channels are free (not
/// used by a bridge, not subscribed): it may lie past the device's last
/// channel, and the device then grows.
fn first_free(
    bridges: &[BridgeStatus],
    device: &DeviceStatus,
    direction: BridgeDirection,
    channels: u16,
) -> u16 {
    let used = used_channels(bridges, device, direction);
    let n = channels.max(1);
    (1..=u16::MAX - n)
        .find(|c| (0..n).all(|i| !used.contains(&(c + i))))
        .unwrap_or(1)
}

/// The channel count a direction of a device has.
fn count_of(device: &DeviceStatus, direction: BridgeDirection) -> usize {
    match direction {
        BridgeDirection::Capture => device.tx_channels.len(),
        _ => device.rx_channels.len(),
    }
}

/// The labels a new bridge's channels start with: `SC3 L`/`SC3 R` for a
/// stereo card, else `CQ12T In 1`, `CQ12T Out 1`, ...
fn default_labels(card: &str, direction: BridgeDirection, n: u16) -> Vec<String> {
    let card: String = card.chars().filter(|c| !c.is_control()).collect();
    if n == 2 {
        return vec![format!("{card} L"), format!("{card} R")];
    }
    let word = if direction == BridgeDirection::Capture {
        "In"
    } else {
        "Out"
    };
    (1..=n).map(|i| format!("{card} {word} {i}")).collect()
}

/// A card's short name (its ALSA id, `SC3`), as channel labels use it.
fn card_id(d: &DetectedDevice) -> &str {
    if d.node.is_empty() || d.node.starts_with('/') {
        &d.name
    } else {
        &d.node
    }
}

/// The device rate a card cannot run at, with a rate it can: `None` when
/// the card offers the device's rate (or its rates are unknown).
fn rate_problem(card: &DetectedDevice, device: &DeviceStatus) -> Option<(u32, u32)> {
    let rate = device.sample_rate.0;
    if card.rates.is_empty() || card.rates.contains(&rate) {
        return None;
    }
    let pick = if card.rates.contains(&48_000) {
        48_000
    } else {
        card.rates
            .iter()
            .copied()
            .filter(|r| *r <= rate)
            .max()
            .or_else(|| card.rates.first().copied())?
    };
    Some((rate, pick))
}

const fn state_words(s: BridgeState) -> (&'static str, &'static str) {
    match s {
        BridgeState::Running => ("running", "status on"),
        BridgeState::Starting | BridgeState::Reopening => ("opening", "status off"),
        BridgeState::Failed => ("error", "status off"),
        BridgeState::Disabled => ("disabled", "status off"),
        _ => ("unknown", "status off"),
    }
}

fn bridge_path(name: &str, rest: &str) -> String {
    format!("/audio/bridges/{}/{rest}", path_segment(name))
}

// ---- the page --------------------------------------------------------------

/// `GET /audio`.
pub async fn show(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let body = match snapshot(&st).await {
        Err(e) => error_box(&e),
        Ok(snap) => render(&snap, &s.csrf),
    };
    super::page(
        &st,
        &token,
        &s,
        &headers,
        view("Audio", "/audio").every(LIVE_EVERY),
        &body,
    )
}

fn render(snap: &Snapshot, csrf: &str) -> Markup {
    let orphans: Vec<&BridgeStatus> = snap
        .bridges
        .iter()
        .filter(|b| !snap.cards.iter().any(|d| on_card(b, d)))
        .collect();
    html! {
        div.grid.audio {
            section.card id="cards" {
                h2 { "Sound cards on this host" span.n { "DACs, USB interfaces and mixers" } }
                @if snap.cards.is_empty() {
                    p.muted { "No sound card detected. Plug in the DAC or interface; this page refreshes by itself." }
                }
                p.muted {
                    "A card is this host's local I/O: its inputs and outputs are ports of the host's router. Send a card's inputs to the network (they become TX channels others can receive), feed its outputs from the network (RX channels), or keep a card for local routing only. Patch cards, applications and network channels together on the "
                    a href="/router" { "Router page" } "; route between this host and other devices on the network on the "
                    a href="/routing" { "Network routing page" } " or in their own controller."
                }
                p.muted {
                    "Added a PCM to " code { "/etc/asound.conf" } "? tesserad must restart to see new ALSA devices ("
                    code { "systemctl restart tesserad" } ", or on the " a href="/services" { "Services page" } ")."
                }
            }
            @for d in &snap.cards { (card_block(d, snap, csrf)) }
            @if !orphans.is_empty() { (bridges_block(&orphans, snap, csrf)) }
            @for d in &snap.devices {
                @if !d.rx_channels.is_empty() && !d.tx_channels.is_empty() {
                    (monitor_block(d, snap.router.as_ref(), csrf))
                }
            }
        }
    }
}

fn card_block(d: &DetectedDevice, snap: &Snapshot, csrf: &str) -> Markup {
    let users: Vec<&BridgeStatus> = snap.bridges.iter().filter(|b| on_card(b, d)).collect();
    let many = snap.devices.len() > 1;
    html! {
        section.card.soundcard id=(format!("card-{}", path_segment(&d.id))) data-live-key=(format!("card-{}", d.id)) {
            div.surface-head {
                h2 { bdi { (d.name) } @if let Some(p) = &d.pcm { span.n { code { (p) } } } }
            }
            p {
                (d.capture_channels.map_or_else(|| "inputs unknown".to_owned(), |n| format!("{n} input(s)")))
                " · "
                (d.playback_channels.map_or_else(|| "outputs unknown".to_owned(), |n| format!("{n} output(s)")))
                " · "
                (if d.rates.is_empty() { "rates unknown".to_owned() } else { output::rates_words(&d.rates) })
                @if let Some(u) = &d.used_by { " · used by " bdi { (u) } }
            }
            @for dev in &snap.devices {
                @if let Some((rate, pick)) = rate_problem(d, dev) {
                    p.flash.bad {
                        "This card can't run at " (khz(rate)) "; set the device "
                        @if many { bdi { (dev.name) } " " }
                        "to " (khz(pick)) " on the " a href="/devices" { "Device page" } "."
                    }
                }
            }
            h3 { "Bridges of this card" }
            @if users.is_empty() {
                p.muted { "None yet." }
            } @else {
                div.bridges { @for b in &users { (bridge_item(b, &d.name, snap, csrf)) } }
            }
            @if d.pcm.is_none() {
                p.muted { "The daemon did not report this card's PCM, so it cannot be bridged from here." }
            } @else if snap.devices.is_empty() {
                p.muted { "The daemon reported no devices to bridge to." }
            } @else {
                @if d.capture_channels != Some(0) {
                    details.edit {
                        summary { (if users.iter().any(|b| b.direction == BridgeDirection::Capture) { "Send more of its inputs to the network" } else { "Send its inputs to the network" }) }
                        (bridge_form(d, snap, BridgeDirection::Capture, csrf))
                    }
                }
                @if d.playback_channels != Some(0) {
                    details.edit {
                        summary { (if users.iter().any(|b| b.direction == BridgeDirection::Playback) { "Play more network audio on its outputs" } else { "Play network audio on its outputs" }) }
                        (bridge_form(d, snap, BridgeDirection::Playback, csrf))
                    }
                }
            }
        }
    }
}

/// One bridge: its mapping, live state, actions, and its device channels
/// with their labels (renamed in place).
fn bridge_item(b: &BridgeStatus, card: &str, snap: &Snapshot, csrf: &str) -> Markup {
    // Opening with an error is a card that cannot be opened (missing,
    // busy): the daemon retries, and the error says why.
    let (word, class) = match (b.state, &b.last_error) {
        (BridgeState::Starting | BridgeState::Reopening, Some(_)) => {
            ("cannot open \u{b7} retrying", "status off")
        }
        (s, _) => state_words(s),
    };
    let enabled = b.state != BridgeState::Disabled;
    let device = snap.devices.iter().find(|x| x.key == b.device);
    let (dir, dir_word) = match b.direction {
        BridgeDirection::Capture => (Direction::Tx, "tx"),
        _ => (Direction::Rx, "rx"),
    };
    html! {
        article.bridge id=(format!("bridge-{}", path_segment(&b.name))) data-live-key=(format!("bridge-{}", b.name)) {
            div.control-head {
                div.control-name {
                    strong { bdi { (b.name) } }
                    " " span class=(class) { (word) }
                }
                span.muted { "fill " (b.fill_frames) " frames · xruns " (b.xruns) }
            }
            @if b.local_only {
                p {
                    (card) " " (if b.direction == BridgeDirection::Capture { "in " } else { "out " })
                    (b.card_first_channel) "\u{2013}" (u32::from(b.card_first_channel) + u32::from(b.channels.max(1)) - 1)
                    " " span.tag { "local routing only" }
                }
                p.muted { "Not on the network: its channels are router ports. Patch them on the " a href="/router" { "Router page" } "." }
            } @else {
                p { (mapping_words(card, snap.devices.iter().find(|d| d.key == b.device).map_or(b.device.0.as_str(), |d| d.name.as_str()), b.direction, b.first_channel, b.channels, b.card_first_channel)) }
            }
            @if let Some(e) = &b.last_error { p.muted { (e) } }
            @if let (Some(dev), false) = (device, b.local_only) {
                div.table { table.labels {
                    thead { tr { th { "Card" } th { "Channel" } th { "Label" } } }
                    tbody {
                        @for i in 0..b.channels {
                            @let n = b.first_channel.saturating_add(i);
                            @let list = if dir == Direction::Tx { &dev.tx_channels } else { &dev.rx_channels };
                            @let label = list.iter().find(|c| c.channel.get() == n).map(|c| c.label.clone());
                            @let field = format!("label-{}-{}", path_segment(&b.name), n);
                            tr {
                                td { (if dir == Direction::Tx { "in " } else { "out " }) (u32::from(b.card_first_channel.max(1)) + u32::from(i)) }
                                td { (dir_word.to_uppercase()) " " (n) }
                                td {
                                    @match label {
                                        Some(l) => {
                                            form.inline method="post" action="/audio/label" {
                                                input type="hidden" name="csrf" value=(csrf);
                                                input type="hidden" name="device" value=(dev.key.0);
                                                input type="hidden" name="direction" value=(dir_word);
                                                input type="hidden" name="channel" value=(n);
                                                input type="hidden" name="bridge" value=(b.name);
                                                label.sr for=(field) { "Label of " (dir_word.to_uppercase()) " " (n) }
                                                input id=(field) type="text" name="label" value=(l) maxlength="31" size="16";
                                                button.btn.sm type="submit" { "Rename" }
                                            }
                                        }
                                        None => { span.muted { "not on the device (yet)" } }
                                    }
                                }
                            }
                        }
                    }
                } }
            }
            div.surface-actions {
                form.inline.toggle method="post" action=(bridge_path(&b.name, "enabled")) data-live-post {
                    input type="hidden" name="csrf" value=(csrf);
                    input type="hidden" name="enabled" value=(if enabled { "false" } else { "true" });
                    button.switch.on[enabled] type="submit"
                        aria-pressed=(if enabled { "true" } else { "false" })
                        aria-label=(format!("Bridge {} enabled", b.name)) {}
                    span { (if enabled { "On" } else { "Off" }) }
                    span.note data-live-local {}
                }
                form.inline method="post" action=(bridge_path(&b.name, "remove")) {
                    input type="hidden" name="csrf" value=(csrf);
                    button.btn.sm.danger type="submit" aria-label=(format!("Remove bridge {}", b.name)) { "Remove…" }
                }
            }
            details.edit {
                summary { (if b.local_only { "Put it on the network again" } else { "Use for local routing only" }) span.sr { " (" (b.name) ")" } }
                form.fields method="post" action=(bridge_path(&b.name, "local")) {
                    input type="hidden" name="csrf" value=(csrf);
                    input type="hidden" name="local" value=(if b.local_only { "false" } else { "true" });
                    @if b.local_only {
                        p.muted { "The bridge feeds its device channels again (" (if b.direction == BridgeDirection::Capture { "TX " } else { "RX " }) (b.first_channel) " on), as card bridges did before the router. Router crosspoints that repeat that path are removed first." }
                        button.btn type="submit" { "Put it on the network" }
                    } @else {
                        p.muted { "The card is opened for the host router only: its channels become router ports and leave the network unless patched. Needs a tesserad with the host router." }
                        label.check {
                            input type="checkbox" name="keep" value="true" checked;
                            span { "Keep the same path: patch its " (if b.direction == BridgeDirection::Capture { "inputs to the same TX channels" } else { "outputs from the same RX channels' host outputs" }) " in the router" }
                        }
                        button.btn type="submit" { "Use for local routing only" }
                    }
                }
            }
        }
    }
}

fn bridge_form(
    d: &DetectedDevice,
    snap: &Snapshot,
    direction: BridgeDirection,
    csrf: &str,
) -> Markup {
    let capture = direction == BridgeDirection::Capture;
    let card_channels = if capture {
        d.capture_channels
    } else {
        d.playback_channels
    };
    let dev = &snap.devices[0];
    let channels = card_channels.unwrap_or(2).max(1);
    let first = first_free(&snap.bridges, dev, direction, channels);
    let have = count_of(dev, direction);
    let last = usize::from(first) + usize::from(channels) - 1;
    let base = format!(
        "{}-{}",
        slug(card_id(d)),
        if capture { "in" } else { "out" }
    );
    let name = unique_name(&base, &snap.bridges);
    let dir_word = if capture { "TX" } else { "RX" };
    let labels = default_labels(
        card_id(d),
        direction,
        card_channels.unwrap_or(2).clamp(1, 64),
    );
    html! {
        form.fields.bridge-form method="post" action="/audio/bridges" {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="card" value=(d.pcm.as_deref().unwrap_or_default());
            input type="hidden" name="card_name" value=(d.name);
            input type="hidden" name="direction" value=(if capture { "capture" } else { "playback" });
            p.muted {
                @if capture {
                    "The card's inputs, from the first card input chosen, become TX channels of the device, from the first TX channel chosen: other devices on the network can then receive them."
                } @else {
                    "RX channels of the device, from the first one chosen, play on the card's outputs, from the first card output chosen: route network audio to those RX channels."
                }
            }
            @if snap.devices.len() == 1 {
                input type="hidden" name="device" value=(dev.key.0);
            } @else {
                label { "Device"
                    select name="device" {
                        @for x in &snap.devices { option value=(x.key.0) { (x.name) } }
                    }
                }
            }
            label { "First " (dir_word) " channel" input type="number" name="first" min="1" value=(first) required; }
            label { (if capture { "First card input" } else { "First card output" })
                input type="number" name="card_first" min="1" max=[card_channels] value="1" required;
            }
            label { "Channels"
                input type="number" name="channels" min="1" max=[card_channels] value=(channels) required;
            }
            label { "Name" input type="text" name="name" value=(name) required pattern="[A-Za-z0-9_\\-]+" title="Letters, digits, - and _" size="14"; }
            fieldset.labels {
                legend { "Channel labels" }
                @for (i, l) in labels.iter().enumerate() {
                    label { (if capture { "In " } else { "Out " }) (i + 1)
                        input type="text" name=(format!("label_{}", i + 1)) value=(l) maxlength="31" size="14";
                    }
                }
                p.muted { "The new " (dir_word) " channels take these names (the first ones, as many as are carried); others on the network see them." }
            }
            label.check {
                input type="checkbox" name="local_only" value="true";
                span { "For local routing only: the card stays off the network until its channels are patched on the " a href="/router" { "Router page" } " (first " (dir_word) " channel and labels are then not used)" }
            }
            label.check {
                input type="checkbox" name="low_latency" value="true";
                span { "Low latency: short card periods (" (LOW_LATENCY_PERIOD_FRAMES) " frames, 2 ms at 48 kHz) — needs a quiet system" }
            }
            p.muted {
                "Now: " (mapping_words(&d.name, &dev.name, direction, first, channels, 1)) ". "
                @if last > have {
                    (crate::ports::device_subject(&snap.devices, &dev.key)) " has " (have) " " (dir_word) " channel(s): it gets " (last - have) " more (the device restarts; you confirm first)."
                } @else {
                    "Channels in use by another bridge or a subscription are skipped."
                }
            }
            button.btn.primary type="submit" { (if capture { "Send inputs to the network" } else { "Play on its outputs" }) }
        }
    }
}

fn bridges_block(orphans: &[&BridgeStatus], snap: &Snapshot, csrf: &str) -> Markup {
    html! {
        section.card id="bridges" {
            h2 { "Bridges whose card is not detected" span.n { "unplugged, or named differently" } }
            div.bridges { @for b in orphans { (bridge_item(b, card_name(b, &snap.cards), snap, csrf)) } }
            p.muted { "Bridges set in the configuration files (" code { "[[bridge]]" } ") are changed there: the daemon refuses to switch or remove them from here. Switching a bridge off and on again uses the daemon's default card period." }
        }
    }
}

/// A channel's name once: its label, and `RX 2` beside it only when the
/// label says something else.
fn channel_cell(word: &str, c: &tessera_control_types::ChannelStatus) -> Markup {
    let n = c.channel.get();
    let plain = format!("{word} {n}");
    html! {
        @if c.label.trim().is_empty() || c.label == plain {
            (plain)
        } @else {
            bdi { (c.label) } " " span.muted { (plain) }
        }
    }
}

fn monitor_block(d: &DeviceStatus, router: Option<&RouterReport>, csrf: &str) -> Markup {
    let key = &d.key.0;
    let current = |rx: usize| d.monitor_rx_from_tx.get(rx).copied().unwrap_or(0);
    let id = format!("monitor-{}", path_segment(key));
    // A saved router grid replaces monitoring: its taps of TX channels into
    // host outputs are what plays, and they change on the Router page.
    if let Some(r) = router.filter(|r| r.saved && !r.read_only) {
        let taps: Vec<&tessera_control_types::CrosspointStatus> = r
            .crosspoints
            .iter()
            .filter(|x| {
                matches!(&x.crosspoint.source, RouterPort::NetworkTx { device, .. } if device == &d.key)
                    && matches!(&x.crosspoint.destination, RouterPort::HostOut { device, .. } if device == &d.key)
            })
            .collect();
        return html! {
            section.card.monitor id=(id) {
                h2 { "Local monitoring" span.n { (d.name) } }
                p {
                    "The router's grid is saved: local monitoring is part of it now (a network output played on a host output). "
                    a href="/router" { "Change it on the Router page" } "."
                }
                @if taps.is_empty() {
                    p.muted { "No network output is played locally." }
                } @else {
                    ul.taps {
                        @for x in &taps {
                            li { (crate::ports::xp_words(Some(r), &x.crosspoint.key()))
                                @if x.crosspoint.muted { " " span.tag.bad { "muted" } }
                                @if !x.active { " " span.muted { "(inactive)" } } }
                        }
                    }
                }
            }
        };
    }
    let delay_ms = f64::from(d.monitor_delay_us) / 1000.0;
    let auto = d.monitor_delay_us == 0;
    let delay_id = format!("{id}-delay");
    html! {
        section.card.monitor id=(id) {
            h2 { "Local monitoring" span.n { (d.name) } }
            p { "Plays a local input on this device's own outputs too: an RX channel (what the device plays) also plays the TX channel (a local input) chosen in its row, after that input's gain and mute." }
            @if router.is_some_and(|r| !r.read_only) {
                p.muted { "The " a href="/router" { "Router page" } " shows the same taps in its grid; saving a grid there makes them part of it." }
            }
            form.monitor-form method="post" action=(format!("/audio/monitor/{}", path_segment(key))) {
                input type="hidden" name="csrf" value=(csrf);
                div.table { table.grid-radio {
                    thead { tr {
                        th scope="col" { "Output (RX)" }
                        th scope="col" { "none" }
                        @for t in &d.tx_channels { th scope="col" { (channel_cell("TX", t)) } }
                    } }
                    tbody {
                        @for (i, r) in d.rx_channels.iter().enumerate() {
                            @let n = r.channel.get();
                            @let cur = current(i);
                            tr {
                                th scope="row" { (channel_cell("RX", r)) }
                                td { input type="radio" name=(format!("rx{n}")) value="0" checked[cur == 0] aria-label=(format!("RX {n} plays no local input")); }
                                @for t in &d.tx_channels {
                                    @let tn = t.channel.get();
                                    td { input type="radio" name=(format!("rx{n}")) value=(tn) checked[cur == tn] aria-label=(format!("RX {n} also plays TX {tn}")); }
                                }
                            }
                        }
                    }
                } }
                div.fields {
                    label for=(delay_id) { "Delay (ms)" }
                    input id=(delay_id) type="number" name="delay_ms" min="0" max="1000" step="0.5"
                        value=[(!auto).then_some(delay_ms)] placeholder="auto" aria-describedby=(format!("{delay_id}-help"));
                    p.muted id=(format!("{delay_id}-help")) {
                        @if auto { "Now: auto (the lowest safe delay). " }
                        @else { "Now: " (delay_ms) " ms, fixed. Empty or 0: auto, the lowest safe delay (recommended). " }
                        "How far behind playback the inputs are read: enough for the input to have arrived (about the device's latency)."
                    }
                }
                button.btn.primary type="submit" { "Save monitoring" }
            }
        }
    }
}

// ---- changing ----------------------------------------------------------------

async fn send(st: &Shared, s: &Session, cmd: Command) -> Result<(), (StatusCode, String)> {
    let kind = cmd.kind();
    let r = call(st, RequestBody::Command(cmd)).await;
    tracing::info!(session = %s.id, command = kind, ok = r.is_ok(), "page command");
    r.map(|_| ()).map_err(|e| match &e {
        ClientError::Command(CommandError::Locked { .. }) => (
            StatusCode::CONFLICT,
            "This bridge is set in the configuration files: change it there.".into(),
        ),
        _ => super::refusal(&e),
    })
}

fn count(s: &str, what: &str) -> Result<u16, String> {
    match s.trim().parse::<u16>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(format!("{what}: a whole number from 1.")),
    }
}

fn field<'a>(form: &'a [(String, String)], name: &str) -> &'a str {
    form.iter()
        .find(|(k, _)| k == name)
        .map_or("", |(_, v)| v.as_str())
}

/// The commands a new bridge needs, in order: more device channels when
/// the range goes past the last one (it restarts the device, so the
/// daemon asks for confirmation), the bridge, then the labels of its
/// channels. With the confirmation message.
fn bridge_commands(
    form: &[(String, String)],
    snap: &Snapshot,
) -> Result<(Vec<Command>, String), String> {
    let name = field(form, "name").trim().to_owned();
    if !ident_ok(&name) {
        return Err("A bridge name uses letters, digits, - and _ only.".into());
    }
    let direction = match field(form, "direction") {
        "capture" => BridgeDirection::Capture,
        "playback" => BridgeDirection::Playback,
        _ => return Err("Choose inputs or outputs.".into()),
    };
    let card = field(form, "card").trim();
    if card.is_empty() {
        return Err("The card has no PCM to open.".into());
    }
    let first = count(field(form, "first"), "First channel")?;
    let channels = count(field(form, "channels"), "Channels")?;
    let card_first = match field(form, "card_first").trim() {
        "" => 1,
        v => count(v, "First card channel")?,
    };
    if snap.bridges.iter().any(|b| b.name == name) {
        return Err(format!(
            "A bridge named `{name}` exists already; choose another name."
        ));
    }
    let device = field(form, "device");
    let Some(dev) = snap.devices.iter().find(|d| d.key.0 == device) else {
        return Err(format!("There is no device `{device}`."));
    };
    let detected = snap.cards.iter().find(|d| d.pcm.as_deref() == Some(card));
    let card_has = detected.and_then(|d| match direction {
        BridgeDirection::Capture => d.capture_channels,
        _ => d.playback_channels,
    });
    let card_last = u32::from(card_first) + u32::from(channels) - 1;
    if let Some(n) = card_has.filter(|n| card_last > u32::from(*n)) {
        return Err(format!(
            "The card has {n} {}: card channels {card_first} to {card_last} do not fit.",
            if direction == BridgeDirection::Capture {
                "input(s)"
            } else {
                "output(s)"
            }
        ));
    }
    if field(form, "local_only") == "true" {
        // The card stays off the network: no device channels, no labels.
        let shown = match field(form, "card_name").trim() {
            "" => card.to_owned(),
            n => n.to_owned(),
        };
        let cmd = Command::SetCardBridge {
            bridge: {
                let mut b = CardBridgeSpec::new(
                    name.clone(),
                    dev.key.clone(),
                    card,
                    direction,
                    1,
                    channels,
                );
                b.card_first_channel = card_first;
                b.local_only = true;
                b.period_frames =
                    (field(form, "low_latency") == "true").then_some(LOW_LATENCY_PERIOD_FRAMES);
                b
            },
        };
        return Ok((
            vec![cmd],
            format!(
                "Bridge `{name}` saved: {shown} {} {card_first}\u{2013}{card_last} for local routing only. Patch its channels on the Router page.",
                if direction == BridgeDirection::Capture {
                    "in"
                } else {
                    "out"
                }
            ),
        ));
    }
    let last = u32::from(first) + u32::from(channels) - 1;
    let last = u16::try_from(last).map_err(|_| "Too many channels.".to_owned())?;
    let used = used_channels(&snap.bridges, dev, direction);
    let (dir, dir_word) = match direction {
        BridgeDirection::Capture => (Direction::Tx, "TX"),
        _ => (Direction::Rx, "RX"),
    };
    if let Some(c) = (first..=last).find(|c| used.contains(c)) {
        return Err(format!(
            "{dir_word} {c}{} is already used by another bridge or a subscription: start at {} instead.",
            crate::ports::device_name(&snap.devices, &dev.key)
                .map(|n| format!(" of {n}"))
                .unwrap_or_default(),
            first_free(&snap.bridges, dev, direction, channels)
        ));
    }
    let mut cmds = Vec::new();
    let have = count_of(dev, direction);
    let mut grow = String::new();
    if usize::from(last) > have {
        let (rx, tx) = match direction {
            BridgeDirection::Capture => (dev.rx_channels.len(), usize::from(last)),
            _ => (usize::from(last), dev.tx_channels.len()),
        };
        cmds.push(Command::SetDeviceChannels {
            device: dev.key.clone(),
            rx: u16::try_from(rx).unwrap_or(u16::MAX),
            tx: u16::try_from(tx).unwrap_or(u16::MAX),
            dry_run: false,
        });
        grow = format!(
            " {} now has {last} {dir_word} channel(s).",
            crate::ports::device_subject(&snap.devices, &dev.key)
        );
    }
    let shown = match field(form, "card_name").trim() {
        "" => card.to_owned(),
        n => n.to_owned(),
    };
    let words = mapping_words(&shown, &dev.name, direction, first, channels, card_first);
    cmds.push(Command::SetCardBridge {
        bridge: {
            let mut b = CardBridgeSpec::new(
                name.clone(),
                dev.key.clone(),
                card,
                direction,
                first,
                channels,
            );
            b.card_first_channel = card_first;
            b.period_frames =
                (field(form, "low_latency") == "true").then_some(LOW_LATENCY_PERIOD_FRAMES);
            b
        },
    });
    let list = if dir == Direction::Tx {
        &dev.tx_channels
    } else {
        &dev.rx_channels
    };
    let mut labelled = 0;
    for i in 0..channels {
        let label = field(form, &format!("label_{}", i + 1)).trim();
        let n = first + i;
        let Some(channel) = Channel::new(n) else {
            continue;
        };
        let same = list
            .iter()
            .any(|c| c.channel.get() == n && c.label == label);
        if label.is_empty() || same {
            continue;
        }
        labelled += 1;
        cmds.push(Command::SetChannelLabel {
            device: dev.key.clone(),
            direction: dir,
            channel,
            label: Some(label.to_owned()),
        });
    }
    let named = if labelled > 0 {
        format!(" {labelled} channel(s) named.")
    } else {
        String::new()
    };
    Ok((
        cmds,
        format!("Bridge `{name}` saved: {words}.{grow}{named}"),
    ))
}

/// `POST /audio/bridges`: a new bridge of a card, enabled, its channels
/// named; the device grows first when the range needs it (after the usual
/// confirmation, since that restarts it).
pub async fn add_bridge(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, field(&form, "csrf")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let prepared = match snapshot(&st).await {
        Ok(snap) => bridge_commands(&form, &snap),
        Err(e) => Err(e),
    };
    match prepared {
        Err(m) => back(&st, &token, "/audio#cards", Err(m)),
        Ok((cmds, words)) => {
            let outcome = super::drive(&st, &s, cmds).await;
            super::settle(
                &st,
                &token,
                &s,
                &headers,
                "/audio#cards",
                |_| words,
                outcome,
            )
        }
    }
}

/// `POST /audio/label`: rename one channel of a bridge (`SetChannelLabel`;
/// empty: back to the default).
pub async fn label(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, field(&form, "csrf")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let to = format!("/audio#bridge-{}", path_segment(field(&form, "bridge")));
    let r: Result<String, (StatusCode, String)> = async {
        let bad = |m: &str| (StatusCode::BAD_REQUEST, m.to_owned());
        let direction =
            super::parse_dir(field(&form, "direction")).ok_or_else(|| bad("Name rx or tx."))?;
        let channel = field(&form, "channel")
            .trim()
            .parse::<u16>()
            .ok()
            .and_then(Channel::new)
            .ok_or_else(|| bad("Name a channel from 1."))?;
        let device = DeviceKey(field(&form, "device").to_owned());
        let label = field(&form, "label").trim();
        // The device's name in the reply on a host with several devices;
        // never its key.
        let status = status_of(&st).await.ok();
        let named = status
            .as_ref()
            .and_then(|s| crate::ports::device_name(&s.devices, &device))
            .map(|n| format!("{n} "))
            .unwrap_or_default();
        let words = format!(
            "{named}{} {}",
            if direction == Direction::Tx {
                "TX"
            } else {
                "RX"
            },
            channel.get()
        );
        send(
            &st,
            &s,
            Command::SetChannelLabel {
                device,
                direction,
                channel,
                label: (!label.is_empty()).then(|| label.to_owned()),
            },
        )
        .await?;
        Ok(if label.is_empty() {
            format!("{words}: default label again.")
        } else {
            format!("{words} is now \u{201c}{label}\u{201d}.")
        })
    }
    .await;
    back(&st, &token, &to, r.map_err(|(_, m)| m))
}

/// `POST /audio/bridges/{name}/enabled`.
#[derive(Debug, Deserialize)]
pub struct EnabledForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    enabled: String,
}

/// `POST /audio/bridges/{name}/enabled`: switch a saved bridge on or off.
pub async fn bridge_enabled(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(f): Form<EnabledForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let on = f.enabled == "true";
    let r: Result<String, (StatusCode, String)> = async {
        let bridges = bridges_of(&st)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
        let Some(b) = bridges.into_iter().find(|b| b.name == name) else {
            return Err((
                StatusCode::NOT_FOUND,
                format!("There is no bridge `{name}`."),
            ));
        };
        let mut spec = CardBridgeSpec::new(
            b.name.clone(),
            b.device,
            b.card,
            b.direction,
            b.first_channel,
            b.channels,
        );
        spec.enabled = on;
        spec.card_first_channel = b.card_first_channel;
        spec.local_only = b.local_only;
        send(&st, &s, Command::SetCardBridge { bridge: spec }).await?;
        Ok(if on {
            format!("Bridge `{name}` switched on.")
        } else {
            format!("Bridge `{name}` switched off: the card is left alone.")
        })
    }
    .await;
    let to = format!("/audio#bridge-{}", path_segment(&name));
    answer(&st, &token, &headers, &to, r, serde_json::json!({}))
}

/// The crosspoints that carry a network bridge's path in the router: card
/// inputs to its TX channels (capture), or its RX channels' host outputs
/// to card outputs (playback).
fn bridge_path_crosspoints(b: &BridgeStatus) -> Vec<(RouterPort, RouterPort)> {
    (0..b.channels)
        .filter_map(|i| {
            let dev = Channel::new(b.first_channel.checked_add(i)?)?;
            let card = Channel::new(b.card_first_channel.checked_add(i)?)?;
            Some(match b.direction {
                BridgeDirection::Capture => (
                    RouterPort::CardIn {
                        card: b.card.clone(),
                        channel: card,
                    },
                    RouterPort::NetworkTx {
                        device: b.device.clone(),
                        channel: dev,
                    },
                ),
                _ => (
                    RouterPort::HostOut {
                        device: b.device.clone(),
                        channel: dev,
                    },
                    RouterPort::CardOut {
                        card: b.card.clone(),
                        channel: card,
                    },
                ),
            })
        })
        .collect()
}

/// `POST /audio/bridges/{name}/local`: `local=true` opens the card for
/// the host router only (`local_only`), and with `keep` patches the same
/// path in the router (§7 of the host router plan: card in → TX, host out
/// → card out; the default app → TX crosspoint of those TX channels is
/// removed). `local=false` removes those crosspoints, then feeds the
/// device channels again.
pub async fn bridge_local(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, field(&form, "csrf")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let to = format!("/audio#bridge-{}", path_segment(&name));
    let local = field(&form, "local") == "true";
    let keep = field(&form, "keep") == "true";
    let bridges = match bridges_of(&st).await {
        Ok(b) => b,
        Err(e) => return back(&st, &token, &to, Err(e)),
    };
    let Some(b) = bridges.into_iter().find(|b| b.name == name) else {
        return back(
            &st,
            &token,
            "/audio",
            Err(format!("There is no bridge `{name}`.")),
        );
    };
    let mut spec = CardBridgeSpec::new(
        b.name.clone(),
        b.device.clone(),
        b.card.clone(),
        b.direction,
        b.first_channel,
        b.channels,
    );
    spec.enabled = b.state != BridgeState::Disabled;
    spec.card_first_channel = b.card_first_channel;
    spec.local_only = local;
    let path = bridge_path_crosspoints(&b);
    let mut cmds = Vec::new();
    let msg = if local {
        cmds.push(Command::SetCardBridge { bridge: spec });
        if keep {
            for (src, dst) in &path {
                cmds.push(Command::SetCrosspoint {
                    crosspoint: Crosspoint::new(src.clone(), dst.clone()),
                });
                if let RouterPort::NetworkTx { device, channel } = dst {
                    cmds.push(Command::RemoveCrosspoint {
                        source: RouterPort::AppOut {
                            device: device.clone(),
                            channel: *channel,
                        },
                        destination: dst.clone(),
                    });
                }
            }
            format!("Bridge `{name}` is for local routing now; the router carries the same path.")
        } else {
            format!(
                "Bridge `{name}` is for local routing now: its card is off the network until patched on the Router page."
            )
        }
    } else {
        for (src, dst) in &path {
            cmds.push(Command::RemoveCrosspoint {
                source: src.clone(),
                destination: dst.clone(),
            });
        }
        cmds.push(Command::SetCardBridge { bridge: spec });
        format!("Bridge `{name}` feeds its device channels again.")
    };
    let outcome = super::drive(&st, &s, cmds).await;
    super::settle(&st, &token, &s, &headers, &to, |_| msg, outcome)
}

/// `POST /audio/bridges/{name}/remove`.
#[derive(Debug, Deserialize)]
pub struct RemoveForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    confirm: String,
}

/// `POST /audio/bridges/{name}/remove`: asks first, then `RemoveCardBridge`.
pub async fn remove_bridge(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(f): Form<RemoveForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if f.confirm != "yes" {
        let text = html! {
            p { "Remove the bridge " strong { bdi { (name) } } "?" }
            p.muted { "Its card is left alone and its channels carry nothing from it any more." }
        };
        let q = Question {
            title: "Remove bridge",
            text,
            action: bridge_path(&name, "remove"),
            hidden: Vec::new(),
            back_to: "/audio#bridges".into(),
            active: "/audio",
        };
        return ask(&st, &headers, &s, &q);
    }
    let r = send(&st, &s, Command::RemoveCardBridge { name: name.clone() })
        .await
        .map(|()| format!("Bridge `{name}` removed."));
    back(&st, &token, "/audio#bridges", r.map_err(|(_, m)| m))
}

/// `POST /audio/monitor/{device}`: `rx<n>` = the TX channel RX `n` also
/// plays (0: none), and `delay_ms`.
pub async fn monitor(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let field = |k: &str| {
        form.iter()
            .find(|(n, _)| n == k)
            .map_or("", |(_, v)| v.as_str())
    };
    let (token, s) = match need_post(&st, &headers, field("csrf")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let to = format!("/audio#monitor-{}", path_segment(&key));
    let r: Result<String, (StatusCode, String)> = async {
        if grid_saved(&st).await {
            return Err((StatusCode::CONFLICT, GRID_SAVED.into()));
        }
        let bad = |m: String| (StatusCode::BAD_REQUEST, m);
        let status = status_of(&st)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
        let Some(d) = status.devices.iter().find(|d| d.key.0 == key) else {
            return Err((
                StatusCode::NOT_FOUND,
                format!("There is no device `{key}`."),
            ));
        };
        let tx_count = u16::try_from(d.tx_channels.len()).unwrap_or(u16::MAX);
        let mut map: Vec<u16> = Vec::with_capacity(d.rx_channels.len());
        for r in &d.rx_channels {
            let n = r.channel.get();
            let v = field(&format!("rx{n}")).trim();
            let t: u16 = if v.is_empty() {
                0
            } else {
                v.parse()
                    .map_err(|_| bad(format!("RX {n}: `{v}` is not a TX channel.")))?
            };
            if t > tx_count {
                return Err(bad(format!(
                    "RX {n}: {} has {tx_count} TX channel(s), not {t}.",
                    crate::ports::device_subject(&status.devices, &d.key)
                )));
            }
            map.push(t);
        }
        while map.last() == Some(&0) {
            map.pop();
        }
        // Empty (the field shows "auto"): 0, auto.
        let ms: f64 = match field("delay_ms").trim() {
            "" => 0.0,
            v => v.parse().map_err(|_| {
                bad("Delay: give a number of milliseconds, or leave it empty for auto.".into())
            })?,
        };
        if !(0.0..=1000.0).contains(&ms) {
            return Err(bad("Delay: from 0 to 1000 ms.".into()));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let delay_us = (ms * 1000.0).round() as u32;
        let pairs: Vec<String> = map
            .iter()
            .enumerate()
            .filter(|(_, t)| **t != 0)
            .map(|(i, t)| format!("RX {} plays TX {t}", i + 1))
            .collect();
        send(
            &st,
            &s,
            Command::SetMonitor {
                device: d.key.clone(),
                rx_from_tx: map,
                delay_us,
            },
        )
        .await?;
        Ok(if pairs.is_empty() {
            crate::ports::device_lead(
                crate::ports::device_name(&status.devices, &d.key),
                "local monitoring off.",
            )
        } else {
            let delay = if delay_us == 0 {
                "auto delay".to_owned()
            } else {
                format!("{ms} ms behind")
            };
            crate::ports::device_lead(
                crate::ports::device_name(&status.devices, &d.key),
                &format!("{} too ({delay}).", pairs.join(", ")),
            )
        })
    }
    .await;
    back(&st, &token, &to, r.map_err(|(_, m)| m))
}

/// Once a router grid is saved, local monitoring is part of it and the
/// portal changes it only on the Router page (the Audio page's card and the
/// routing matrix's local-mix cells are read-only).
const GRID_SAVED: &str =
    "The router's grid is saved: local monitoring is part of it. Change it on the Router page.";

/// Is a router grid saved, and can the daemon change it?
async fn grid_saved(st: &Shared) -> bool {
    crate::ports::router_of(st)
        .await
        .is_ok_and(|r| r.saved && !r.read_only)
}

// ---- a local-mix crosspoint of the routing matrix -----------------------------

/// A local-mix crosspoint: RX `rx` of `device` also plays TX `tx` (0:
/// nothing).
#[derive(Debug, Deserialize)]
pub struct LocalRoute {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    device: String,
    #[serde(default)]
    rx: u16,
    #[serde(default)]
    tx: u16,
}

/// The monitor map with RX `rx` set to TX `tx`, checked against the device.
fn local_map(d: &DeviceStatus, rx: u16, tx: u16) -> Result<Vec<u16>, String> {
    let rx_count = d.rx_channels.len();
    if rx == 0 || usize::from(rx) > rx_count {
        return Err(format!("{} has no RX channel {rx}.", d.name));
    }
    if usize::from(tx) > d.tx_channels.len() {
        return Err(format!("{} has no TX channel {tx}.", d.name));
    }
    let mut map = d.monitor_rx_from_tx.clone();
    map.resize(rx_count.max(map.len()), 0);
    map[usize::from(rx) - 1] = tx;
    while map.last() == Some(&0) {
        map.pop();
    }
    Ok(map)
}

fn channel_name(d: &DeviceStatus, dir: &str, n: u16) -> String {
    let list = if dir == "RX" {
        &d.rx_channels
    } else {
        &d.tx_channels
    };
    list.iter().find(|c| c.channel.get() == n).map_or_else(
        || format!("{dir} {n}"),
        |c| format!("{dir} {n} ({})", c.label),
    )
}

/// `GET /routing/local-review`: confirm a local-mix crosspoint (the routing
/// matrix's dialog shows `#routing-confirmation`).
pub async fn local_review(
    State(st): State<Shared>,
    headers: HeaderMap,
    axum::extract::Query(f): axum::extract::Query<LocalRoute>,
) -> Response {
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let prepared: Result<(String, String), String> = async {
        if grid_saved(&st).await {
            return Err(GRID_SAVED.to_owned());
        }
        let status = status_of(&st).await?;
        let d = status
            .devices
            .iter()
            .find(|d| d.key.0 == f.device)
            .ok_or_else(|| format!("There is no device `{}` on this host.", f.device))?;
        local_map(d, f.rx, f.tx)?;
        let now = d
            .monitor_rx_from_tx
            .get(usize::from(f.rx) - 1)
            .copied()
            .unwrap_or(0);
        let rx = channel_name(d, "RX", f.rx);
        let what = if f.tx == 0 {
            format!("Stop playing {} on {rx}.", channel_name(d, "TX", now))
        } else {
            format!(
                "Also play {} (a local input) on {rx} (local mix).",
                channel_name(d, "TX", f.tx)
            )
        };
        let current = if now == 0 {
            "It plays no local input now.".to_owned()
        } else {
            format!("It plays {} now.", channel_name(d, "TX", now))
        };
        Ok((what, current))
    }
    .await;
    let body = html! { section.card id="routing-confirmation" {
        h2 { "Review local mix" }
        @match prepared {
            Ok((what, current)) => {
                p { (what) }
                p.muted { (current) " A device never receives its own transmission over the network: this crosspoint mixes the input into the output on this host (local monitoring, after the input's gain and mute). Each RX channel plays at most one local input." }
                form method="post" action="/routing/local" {
                    input type="hidden" name="csrf" value=(s.csrf);
                    input type="hidden" name="device" value=(f.device);
                    input type="hidden" name="rx" value=(f.rx);
                    input type="hidden" name="tx" value=(f.tx);
                    button.btn type="submit" { "Confirm change" }
                }
            }
            Err(e) => { (error_box(&e)) }
        }
        a href="/routing" { "Cancel / return to routing" }
        " · "
        a href="/audio" { "Local monitoring on the Audio page" }
        " · "
        a href="/router" { "The Router page" }
    } };
    super::page(
        &st,
        &token,
        &s,
        &headers,
        view("Review local mix", "/routing"),
        &body,
    )
}

/// `POST /routing/local`: set one RX channel's local input (`SetMonitor`
/// with the rest of the map and the delay unchanged).
pub async fn local_route(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<LocalRoute>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let r: Result<String, (StatusCode, String)> = async {
        if grid_saved(&st).await {
            return Err((StatusCode::CONFLICT, GRID_SAVED.into()));
        }
        let status = status_of(&st)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
        let d = status
            .devices
            .iter()
            .find(|d| d.key.0 == f.device)
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    format!("There is no device `{}`.", f.device),
                )
            })?;
        let map = local_map(d, f.rx, f.tx).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        let words = if f.tx == 0 {
            crate::ports::device_lead(
                crate::ports::device_name(&status.devices, &d.key),
                &format!("RX {} plays no local input.", f.rx),
            )
        } else {
            crate::ports::device_lead(
                crate::ports::device_name(&status.devices, &d.key),
                &format!("RX {} also plays TX {} (local mix).", f.rx, f.tx),
            )
        };
        send(
            &st,
            &s,
            Command::SetMonitor {
                device: d.key.clone(),
                rx_from_tx: map,
                delay_us: d.monitor_delay_us,
            },
        )
        .await?;
        Ok(words)
    }
    .await;
    back(&st, &token, "/routing", r.map_err(|(_, m)| m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_control_types::{Encoding, LatencyUs, SampleRate};

    fn card(rates: &[u32]) -> DetectedDevice {
        let mut d = DetectedDevice::new("card:SC3", DetectedKind::SoundCard, "SC3", "SC3");
        d.rates = rates.to_vec();
        d.pcm = Some("hw:CARD=SC3,DEV=0".into());
        d
    }

    fn device(rate: u32) -> DeviceStatus {
        DeviceStatus::new(
            DeviceKey("speaker".into()),
            "Speaker",
            SampleRate(rate),
            Encoding(24),
            LatencyUs(1_000),
        )
    }

    #[test]
    fn a_rate_the_card_lacks_suggests_one_it_has() {
        assert_eq!(
            rate_problem(&card(&[44_100, 48_000]), &device(96_000)),
            Some((96_000, 48_000))
        );
        assert_eq!(
            rate_problem(&card(&[44_100, 48_000]), &device(48_000)),
            None
        );
        assert_eq!(rate_problem(&card(&[]), &device(96_000)), None);
        assert_eq!(
            rate_problem(&card(&[44_100, 88_200]), &device(96_000)),
            Some((96_000, 88_200))
        );
    }

    #[test]
    fn bridges_are_matched_to_their_card() {
        let d = card(&[48_000]);
        let mut b = BridgeStatus::new(
            "x",
            DeviceKey("speaker".into()),
            "hw:CARD=SC3,DEV=0",
            BridgeDirection::Capture,
            BridgeState::Running,
        );
        assert!(on_card(&b, &d));
        b.card = "plughw:CARD=SC3".into();
        assert!(on_card(&b, &d));
        b.card = "hw:CARD=SC30,DEV=0".into();
        assert!(!on_card(&b, &d));
        assert_eq!(
            mapping_words("SC3", "speaker", BridgeDirection::Capture, 1, 2, 1),
            "SC3 in 1\u{2013}2 \u{2192} speaker TX 1\u{2013}2 (to the network)"
        );
    }
}
