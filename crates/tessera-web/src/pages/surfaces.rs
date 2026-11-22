//! The control-surface page (`/surfaces`, provisional 0.x): the devices the
//! host has, the configured surfaces with their live control values,
//! learning a control, and mapping controls to channel mute and gain.
//!
//! Every change is one `SetSurface` (or `RemoveSurface`) with the whole
//! updated spec, built from the daemon's current one: the daemon stays the
//! only authority over surfaces. Everything works without the page's
//! script; with it, values update in place and a learn shows a countdown.

use std::time::Duration;

use axum::Form;
use axum::extract::{Multipart, Path, Query as UrlQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use serde::Deserialize;
use tessera_control_types::{
    BindingAction, ButtonMode, Channel, ChannelSet, Command, ControlKind, ControlValue,
    CrosspointKey, CurvePoint, DetectedKind, DeviceKey, DeviceStatus, Direction, GAIN_CEILING_MDB,
    GAIN_FLOOR_MDB, Gain, LearnedControl, MidiAddress, MidiKind, Query, Reply, RequestBody,
    RouterReport, Status, SurfaceBinding, SurfaceControl, SurfaceSource, SurfaceSpec,
    SurfaceStatus, SurfacesReport, Taper,
};
use tessera_surface::{BindingConfig, SurfaceConfig};
use tesseractl::output::{self, Style};

use super::{
    Chrome, answer, back, error_box, layout, need, need_post, path_segment, refusal, slider_text,
    status_of, view,
};
use crate::auth::Session;
use crate::profiles::{self, Profile, ProfileExt};
use crate::{Shared, call, explain, local_call_waiting};

/// How long a learn watches the device (the daemon allows 30 s). The first
/// second of it is taken as idle: hands off.
pub(crate) const LEARN_WAIT_MS: u32 = 15_000;
/// How often the page refreshes its live values, seconds.
const LIVE_EVERY: u32 = 2;

// ---- reading ---------------------------------------------------------------

async fn report_of(st: &Shared) -> Result<SurfacesReport, String> {
    match call(st, RequestBody::Query(Query::Surfaces)).await {
        Ok(Reply::Surfaces(r)) => Ok(r),
        Ok(other) => Err(format!("unexpected reply: {other:?}")),
        Err(e) => Err(explain(&e)),
    }
}

/// Letters, digits, `-` and `_`: surface names and control ids.
pub(crate) fn ident_ok(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A surface name from a product name: `fifine SC3` → `fifine-sc3`.
#[must_use]
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "surface".into()
    } else {
        out
    }
}

/// `base`, or `base-2`, `base-3`, … whichever `taken` does not have yet.
fn unique(base: &str, taken: &[&str]) -> String {
    if !taken.contains(&base) {
        return base.to_owned();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !taken.contains(&c.as_str()))
        .unwrap_or_else(|| base.to_owned())
}

fn source_words(s: &SurfaceSource) -> String {
    match s {
        SurfaceSource::TcpMidi { host, port } => {
            format!("MIDI over the network, {host} port {port}")
        }
        _ => output::source_words(s),
    }
}

/// What a mapping form can choose from: channels, router ports and
/// crosspoints, the host volume, other surfaces' controls to follow.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Pick<'a> {
    status: Option<&'a Status>,
    router: Option<&'a RouterReport>,
    report: Option<&'a SurfacesReport>,
}

impl<'a> Pick<'a> {
    fn devices(&self) -> &'a [DeviceStatus] {
        self.status
            .map(|s| s.devices.as_slice())
            .unwrap_or_default()
    }

    /// The host volume's range, when the daemon owns it.
    fn host(&self) -> Option<&'a tessera_control_types::HostVolumeStatus> {
        self.status.and_then(|s| s.host_volume.as_ref())
    }
}

/// What a mapping drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum What {
    Channels,
    Host,
    Ports,
    Crosspoints,
    Follow,
}

impl What {
    const ALL: [(Self, &'static str); 5] = [
        (Self::Channels, "channels"),
        (Self::Host, "host"),
        (Self::Ports, "ports"),
        (Self::Crosspoints, "crosspoints"),
        (Self::Follow, "follow"),
    ];

    fn of(v: &str) -> Self {
        Self::ALL
            .iter()
            .find(|(_, n)| *n == v)
            .map_or(Self::Channels, |(w, _)| *w)
    }

    const fn of_action(a: BindingAction) -> Self {
        match a {
            BindingAction::HostVolume | BindingAction::HostMute => Self::Host,
            BindingAction::PortGain | BindingAction::PortMute => Self::Ports,
            BindingAction::CrosspointGain | BindingAction::CrosspointMute => Self::Crosspoints,
            BindingAction::Follow => Self::Follow,
            _ => Self::Channels,
        }
    }
}

/// A mapping in words: the command-line wording for channel actions with
/// the channels in the portal's words (labels, the device's name: see
/// [`crate::ports::channels_words`]), the portal's own for the router, host
/// and follow actions.
fn binding_words(
    b: &SurfaceBinding,
    c: Option<&SurfaceControl>,
    owning: bool,
    router: Option<&RouterReport>,
    status: Option<&Status>,
) -> String {
    let button = c.is_some_and(|c| c.kind == ControlKind::Button);
    let law = || {
        if button {
            String::new()
        } else if let Some(hi) = b.curve.last() {
            // A fader law starts at off (raw 0); its first point is only
            // the bottom of the travel above that, not a range worth naming.
            law_range(hi.mdb)
        } else {
            format!(
                ", {}\u{2026}{} dB",
                db_words(b.min_mdb),
                db_words(b.max_mdb)
            )
        }
    };
    let mode = || match b.mode {
        ButtonMode::Momentary => ", hold to mute",
        ButtonMode::PushToTalk => ", push to talk",
        _ => ", toggle",
    };
    let owner = || {
        if !b.action.owns_level() || button {
            ""
        } else if c.is_some_and(|c| c.motorized) {
            ", motorized"
        } else if owning {
            ", owns the level"
        } else {
            ", owns the level while connected"
        }
    };
    let ports_words = || {
        b.ports
            .iter()
            .map(|p| crate::ports::label(router, p))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let xp_words = || {
        b.crosspoints
            .iter()
            .map(|k| crate::ports::xp_words(router, k))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match b.action {
        BindingAction::HostVolume => format!("Speaker volume{}{}", law(), owner()),
        BindingAction::HostMute => format!("Speaker mute{}", mode()),
        BindingAction::PortGain => format!("Level: {}{}{}", ports_words(), law(), owner()),
        BindingAction::PortMute => format!("Mute: {}{}", ports_words(), mode()),
        BindingAction::CrosspointGain => {
            format!("Crosspoint gain: {}{}{}", xp_words(), law(), owner())
        }
        BindingAction::CrosspointMute => format!("Crosspoint mute: {}{}", xp_words(), mode()),
        BindingAction::Follow => match &b.follow {
            Some(f) => format!("Follows into {} \u{b7} {}{}", f.surface, f.control, law()),
            None => "Follows nothing".into(),
        },
        _ => {
            // The command line names the channels by device key and number
            // (`main TX 1–2`); the portal by their labels.
            let cli = output::binding_words(b, c, owning, Style::Typographic);
            let key = output::channel_set_words(&b.target, Style::Typographic);
            let words = cli.replacen(&key, &crate::ports::channels_words(&b.target, status), 1);
            // The command line gives a law's first point; the portal says
            // `off`, as for the other actions.
            match (b.curve.first(), b.curve.last()) {
                (Some(lo), Some(hi)) => words.replacen(
                    &format!(
                        "{}\u{2026}{} dB (the desk's fader law)",
                        output::db_number(lo.mdb, Style::Typographic),
                        output::db_number(hi.mdb, Style::Typographic)
                    ),
                    law_range(hi.mdb).trim_start_matches(", "),
                    1,
                ),
                _ => words,
            }
        }
    }
}

/// A fader law's range in words: `, off…+10 dB (the desk's fader law)`. A
/// law starts at off (raw 0); its first point is only the bottom of the
/// travel above that, not a level worth naming.
fn law_range(top_mdb: i32) -> String {
    format!(
        ", off\u{2026}{} dB (the desk's fader law)",
        output::db_number(top_mdb, Style::Typographic)
    )
}

/// What kind of device a surface reads: it decides which fields describe
/// where a control lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Src {
    /// HID report fields.
    Hid,
    /// MIDI messages.
    Midi,
    /// Key codes or mixer elements.
    Other,
}

const fn src_of(s: &SurfaceSource) -> Src {
    match s {
        SurfaceSource::Hidraw { .. } => Src::Hid,
        SurfaceSource::Midi { .. } | SurfaceSource::TcpMidi { .. } => Src::Midi,
        _ => Src::Other,
    }
}

const fn kind_word(k: ControlKind) -> &'static str {
    match k {
        ControlKind::Button => "button",
        ControlKind::Fader => "fader",
        ControlKind::Encoder => "encoder",
        _ => "control",
    }
}

const fn dir_word(d: Direction) -> &'static str {
    match d {
        Direction::Rx => "rx",
        Direction::Tx => "tx",
    }
}

/// How a direction is labelled wherever channels are chosen.
const fn dir_label(d: Direction) -> &'static str {
    match d {
        Direction::Rx => "RX \u{b7} network \u{2192} this host",
        Direction::Tx => "TX \u{b7} this host \u{2192} network",
    }
}

pub(crate) fn surface_anchor(name: &str) -> String {
    format!("/surfaces#surface-{}", path_segment(name))
}

pub(crate) fn control_anchor(name: &str, id: &str) -> String {
    format!(
        "/surfaces#control-{}-{}",
        path_segment(name),
        path_segment(id)
    )
}

fn surface_path(name: &str, rest: &str) -> String {
    format!("/surfaces/{}/{rest}", path_segment(name))
}

/// The detected device a surface uses now, or would use: the one the
/// daemon has open, else one whose source matches.
fn device_of(r: &SurfacesReport, sf: &SurfaceStatus) -> Option<String> {
    sf.device.clone().or_else(|| {
        r.detected
            .iter()
            .find(|d| {
                d.source.as_ref() == Some(&sf.spec.source)
                    || d.used_by.as_deref() == Some(sf.spec.name.as_str())
            })
            .map(|d| d.id.clone())
    })
}

// ---- the page --------------------------------------------------------------

/// `GET /surfaces`.
/// The Surfaces page's query: which controls of one surface to show (the
/// filter's form without the page's script; with it the filter acts in
/// place and the query stays empty).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ControlFilter {
    /// The surface the filter is for; empty: none.
    #[serde(default)]
    surface: String,
    /// Words a control's name, id or MIDI message must contain.
    #[serde(default)]
    q: String,
    /// `mapped`: only controls with a mapping; anything else: all.
    #[serde(default)]
    show: String,
}

impl ControlFilter {
    /// Does this filter apply to surface `name`?
    fn on(&self, name: &str) -> bool {
        self.surface == name
    }

    /// Is a control with these words shown?
    fn shows(&self, name: &str, words: &str, mapped: bool) -> bool {
        if !self.on(name) {
            return true;
        }
        let q = self.q.trim().to_lowercase();
        (self.show != "mapped" || mapped) && (q.is_empty() || words.to_lowercase().contains(&q))
    }
}

/// Above this many controls, a surface's controls get a filter.
const FILTER_FROM: usize = 8;

/// The words a control is found by: its label, id, kind and MIDI message.
fn control_search(c: &SurfaceControl) -> String {
    let mut w = format!(
        "{} {} {}",
        c.label.as_deref().unwrap_or(""),
        c.id,
        kind_word(c.kind)
    );
    if let Some(m) = &c.midi {
        w.push(' ');
        w.push_str(&output::midi_words(m));
    }
    w
}

/// The filter of one surface's controls: words and "mapped only / all".
fn filter_form(name: &str, f: &ControlFilter, shown: usize, total: usize) -> Markup {
    let on = f.on(name);
    let id = format!("filter-{}", path_segment(name));
    let mapped = on && f.show == "mapped";
    html! {
        form.controls-filter method="get" action=(format!("/surfaces#surface-{}", path_segment(name))) data-controls-filter role="search" aria-label=(format!("Filter the controls of {name}")) {
            input type="hidden" name="surface" value=(name);
            label for=(id) { "Find" }
            input id=(id) type="search" name="q" value=(if on { f.q.as_str() } else { "" }) placeholder="name, id or MIDI message" size="22" maxlength="64";
            fieldset.seg-radio {
                legend.sr { "Which controls" }
                label.check { input type="radio" name="show" value="all" checked[!mapped]; span { "All" } }
                label.check { input type="radio" name="show" value="mapped" checked[mapped]; span { "Mapped only" } }
            }
            button.btn.sm.nojs type="submit" { "Filter" }
            span.count data-controls-count aria-live="polite" { "Showing " (shown) " of " (total) " controls" }
        }
    }
}

pub async fn show(
    State(st): State<Shared>,
    headers: HeaderMap,
    UrlQuery(filter): UrlQuery<ControlFilter>,
) -> Response {
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let report = report_of(&st).await;
    let base = daemon_profiles(&st).await;
    let status = status_of(&st).await.ok();
    let router = crate::ports::router_of(&st).await.ok();
    let body = match &report {
        Err(e) => error_box(e),
        Ok(r) => {
            let imported = st
                .imported_profiles
                .lock()
                .map(|m| m.clone())
                .unwrap_or_default();
            let pick = Pick {
                status: status.as_ref(),
                router: router.as_ref(),
                report: Some(r),
            };
            render(r, pick, &imported, &base, &filter, &s.csrf)
        }
    };
    super::page(
        &st,
        &token,
        &s,
        &headers,
        view("Surfaces", "/surfaces").every(LIVE_EVERY),
        &body,
    )
}

fn render(
    r: &SurfacesReport,
    pick: Pick<'_>,
    imported: &std::collections::HashMap<String, Profile>,
    base: &[Profile],
    filter: &ControlFilter,
    csrf: &str,
) -> Markup {
    let names: Vec<&str> = r.surfaces.iter().map(|s| s.spec.name.as_str()).collect();
    html! {
        div.grid.surfaces {
            section.card id="detected" {
                h2 { "Devices on this host" span.n { "USB HID interfaces, MIDI ports and sound cards" } }
                @if r.detected.is_empty() {
                    p.muted { "No devices detected. Plug in the mixer or controller; this list refreshes by itself." }
                } @else {
                    // Under 720 px each row becomes a card of labelled lines
                    // (`data-label`), so no column is clipped on a phone.
                    div.table { table.stack {
                        thead { tr { th { "Name" } th { "USB ID" } th { "Interface" } th { "Node" } th { "Kind" } th { "Used by" } th { span.sr { "Action" } } } }
                        tbody {
                            @for d in &r.detected {
                                @let audio = d.kind == DetectedKind::SoundCard;
                                tr.audio[audio] data-live-key=(format!("detected-{}", d.id)) {
                                    td data-label="Name" { bdi { (d.name) } }
                                    td data-label="USB ID" { @if let (Some(v), Some(p)) = (d.vendor_id, d.product_id) { code { (format!("{v:04x}:{p:04x}")) } } @else { "–" } }
                                    td data-label="Interface" { (d.interface.map_or_else(|| "–".into(), |i| i.to_string())) }
                                    td data-label="Node" { code { (d.node) } }
                                    td data-label="Kind" { @if audio { "sound card (audio)" } @else { (output::detected_kind(d.kind)) } }
                                    td data-label="Used by" {
                                        @if let Some(u) = &d.used_by { bdi { (u) } }
                                        @else if audio { span.muted { "no bridge" } }
                                        @else { span.muted { "nothing" } }
                                    }
                                    td data-label="Action" {
                                        @if audio {
                                            a href="/audio#cards" { "Route its audio on the Audio page" }
                                        } @else if matches!(d.kind, DetectedKind::Hid | DetectedKind::Midi) && d.source.is_some() && d.used_by.is_none() {
                                            @let field = format!("add-{}", path_segment(&d.id));
                                            form.inline method="post" action="/surfaces/add" {
                                                input type="hidden" name="csrf" value=(csrf);
                                                input type="hidden" name="device" value=(d.id);
                                                label.sr for=(field) { "Name of the new surface for " (d.name) }
                                                input id=(field) type="text" name="name" value=(unique(&slug(&d.name), &names))
                                                    required pattern="[A-Za-z0-9_\\-]+" title="Letters, digits, - and _" size="12";
                                                button.btn.sm.primary type="submit" { "Add as surface" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } }
                    p.muted { "A HID interface or a MIDI port (a mixing desk, a MIDI controller) can become a control surface. Sound cards carry audio: route them on the " a href="/audio" { "Audio page" } "." }
                }
            }
            section.card id="network-surface" {
                h2 { "A surface over the network" span.n { "MIDI over TCP" } }
                p.muted {
                    "A mixing desk that offers MIDI over TCP (the Allen & Heath CQ on port "
                    (tessera_control_types::CQ_TCP_MIDI_PORT)
                    ") becomes a surface like a USB one. tesserad connects to it and reconnects while the surface is enabled; nothing listens on this host."
                }
                form.fields method="post" action="/surfaces/add-tcp" {
                    input type="hidden" name="csrf" value=(csrf);
                    label { "Name" input type="text" name="name" value=(unique("desk", &names)) required pattern="[A-Za-z0-9_\\-]+" title="Letters, digits, - and _" size="12"; }
                    label { "Host or address" input type="text" name="host" required maxlength="253" size="20" placeholder="for example 192.168.1.40"; }
                    label { "TCP port" input type="number" name="port" min="1" max="65535" value=(tessera_control_types::CQ_TCP_MIDI_PORT) required; }
                    button.btn.primary type="submit" { "Add network surface" }
                }
            }
            @if r.surfaces.is_empty() {
                section.card {
                    h2 { "Control surfaces" }
                    p { "None yet. Add a HID device or MIDI port above as a surface, apply its profile or learn its buttons and faders, and map them to channel mute and gain." }
                }
            }
            @for sf in &r.surfaces { (surface_card(r, sf, pick, imported, base, filter, csrf)) }
        }
    }
}

fn state_badge(sf: &SurfaceStatus) -> Markup {
    html! {
        @if !sf.spec.enabled {
            span.status.off { "disabled" }
        } @else if sf.connected {
            span.status.on { "connected" }
        } @else if sf.problem.is_some() {
            span.status.off { "problem" }
        } @else {
            span.status.off { "not found" }
        }
    }
}

fn surface_card(
    r: &SurfacesReport,
    sf: &SurfaceStatus,
    pick: Pick<'_>,
    imported: &std::collections::HashMap<String, Profile>,
    base: &[Profile],
    filter: &ControlFilter,
    csrf: &str,
) -> Markup {
    let spec = &sf.spec;
    let known = known_profiles(base, imported.get(&spec.name));
    let extra: &[Profile] = &known;
    let name = &spec.name;
    let owning = spec.enabled && sf.connected;
    let src = src_of(&spec.source);
    let learnable = device_of(r, sf).is_some();
    let cfg = SurfaceConfig::from(spec);
    html! {
        section.card.surface id=(format!("surface-{}", path_segment(name))) {
            div.surface-head {
                h2 { bdi { (name) } span.n { (source_words(&spec.source)) } }
                (state_badge(sf))
            }
            @if spec.enabled && !sf.connected {
                @if let Some(p) = &sf.problem {
                    p.flash.bad { (p) }
                } @else {
                    p.muted { "The device is not plugged in, or not detected yet." }
                }
            }
            div.surface-actions {
                form.inline.toggle method="post" action=(surface_path(name, "enabled")) data-live-post {
                    input type="hidden" name="csrf" value=(csrf);
                    input type="hidden" name="enabled" value=(if spec.enabled { "false" } else { "true" });
                    button.switch.on[spec.enabled] type="submit"
                        aria-pressed=(if spec.enabled { "true" } else { "false" })
                        aria-label=(format!("Surface {name} enabled")) {}
                    span { (if spec.enabled { "Enabled" } else { "Disabled: leaves the device alone and sets no gain" }) }
                }
                span.note data-live-local {}
                form.inline method="post" action=(surface_path(name, "remove")) {
                    input type="hidden" name="csrf" value=(csrf);
                    button.btn.sm.danger type="submit" { "Remove surface…" }
                }
            }
            @if src != Src::Other { (profile_block(r, sf, extra, csrf)) }
            h3 { "Learn a control" }
            @if learnable {
                (learn_form(name, csrf))
            } @else {
                p.muted { "Plug the device in to learn its controls." }
            }
            h3 { "Controls" }
            @if spec.controls.is_empty() {
                p.muted { (if src == Src::Midi { "No controls yet: apply a profile or learn one above." } else { "No controls yet: learn one above." }) }
            }
            @let mapped = |c: &SurfaceControl| spec.bindings.iter().any(|b| b.control == c.id);
            @let shown: Vec<bool> = spec.controls.iter().map(|c| filter.shows(name, &control_search(c), mapped(c))).collect();
            @if spec.controls.len() > FILTER_FROM {
                (filter_form(name, filter, shown.iter().filter(|s| **s).count(), spec.controls.len()))
            }
            div.controls {
                @for (c, show) in spec.controls.iter().zip(&shown) {
                    (control_block(sf, &cfg, c, pick, owning, src, extra, !*show, csrf))
                }
            }
            @if spec.controls.len() > FILTER_FROM {
                p.muted.controls-none data-controls-none hidden[shown.iter().any(|s| *s)] { "No control matches the filter." }
            }
            details.manual {
                summary { "Add a control by hand" }
                (control_form(name, &ControlDraft::empty(), src, csrf))
            }
        }
    }
}

/// A control's live value in words: a fader's position as the gain it sets
/// through its first gain mapping (that mapping's range and taper), or as a
/// share of its travel when unmapped, with the raw value beside it; a button
/// `pressed` or `up`.
fn live_value(
    c: &SurfaceControl,
    sf: &SurfaceStatus,
    gain: Option<&BindingConfig>,
    law: Option<&[CurvePoint]>,
) -> Markup {
    let v = sf
        .controls
        .iter()
        .find(|x| x.id == c.id)
        .and_then(|x| x.value);
    let what = c.label.clone().unwrap_or_else(|| c.id.clone());
    html! {
        @match v {
            Some(ControlValue::Fader { value, max }) => {
                span.fader {
                    meter min="0" max=(max.max(1)) value=(value) aria-label=(format!("{what} position")) {}
                    span.val {
                        @if let Some(pts) = law { (law_words(pts, value)) }
                        @else if let Some(b) = gain { (fader_words(b, value, max)) }
                        @else { (percent(value, max)) " %" }
                    }
                    span.raw title="The raw value the device reports" { span.sr { "raw " } (value) "/" (max) }
                }
            }
            Some(ControlValue::Button { pressed: true }) => { span.led.on { "pressed" } }
            Some(ControlValue::Button { pressed: false }) => { span.led { "up" } }
            Some(_) | None => {
                span.muted { (if sf.connected { "touch it to see it" } else { "not connected" }) }
            }
        }
    }
}

/// The gain a fader position sets through binding `b`, written as the
/// Device page writes gains (`off`, `−12.5 dB`).
fn fader_words(b: &BindingConfig, value: u32, max: u32) -> String {
    let db = tessera_surface::fader_db(b, value, max);
    let gain = if db.is_finite() {
        #[allow(clippy::cast_possible_truncation)]
        let m = (f64::from(db) * 1000.0).round() as i32;
        Gain::MilliDb(m)
    } else {
        Gain::Off
    };
    slider_text(gain)
}

/// The gain a fader law gives at raw position `value`.
fn law_words(points: &[CurvePoint], value: u32) -> String {
    slider_text(profiles::curve_mdb(points, value).map_or(Gain::Off, Gain::MilliDb))
}

/// A position as a whole percentage of the travel.
fn percent(value: u32, max: u32) -> u64 {
    (u64::from(value) * 100 / u64::from(max.max(1))).min(100)
}

#[allow(clippy::too_many_arguments)]
fn control_block(
    sf: &SurfaceStatus,
    cfg: &SurfaceConfig,
    c: &SurfaceControl,
    pick: Pick<'_>,
    owning: bool,
    src: Src,
    extra: &[Profile],
    hidden: bool,
    csrf: &str,
) -> Markup {
    let name = &sf.spec.name;
    let law = law_for(&sf.spec, c, extra);
    let mode = mode_for(c, extra);
    let suggested = suggestion_for(c, extra);
    let bound: Vec<(usize, &SurfaceBinding)> = sf
        .spec
        .bindings
        .iter()
        .enumerate()
        .filter(|(_, b)| b.control == c.id)
        .collect();
    let gain = cfg
        .bindings
        .iter()
        .find(|b| b.control == c.id && b.action == tessera_surface::BindingAction::Gain);
    html! {
        article.control id=(format!("control-{}-{}", path_segment(name), path_segment(&c.id)))
            data-search=(control_search(c)) data-mapped=(if bound.is_empty() { "false" } else { "true" }) hidden[hidden] {
            div.control-head {
                div.control-name {
                    strong { bdi { (c.label.as_deref().unwrap_or(&c.id)) } }
                    @if c.label.is_some() { " " code { (c.id) } }
                    span.tag { (kind_word(c.kind)) @if c.motorized { " · motorized" } }
                    @if let Some(m) = &c.midi { " " code.midi title="The MIDI message of this control" { (output::midi_words(m)) } }
                }
                (live_value(c, sf, gain, law.as_deref()))
            }
            ul.bindings {
                @for (i, b) in &bound {
                    li {
                        span.words { (binding_words(b, Some(c), owning, pick.router, pick.status)) }
                        details.edit {
                            summary { "Edit mapping" span.sr { " " (i + 1) " of " (c.id) } }
                            (binding_form(name, c, Some((*i, b)), pick, law.as_deref(), mode, None, csrf))
                        }
                        form.inline method="post" action=(surface_path(name, "bindings/remove")) {
                            input type="hidden" name="csrf" value=(csrf);
                            input type="hidden" name="control" value=(c.id);
                            input type="hidden" name="index" value=(i);
                            button.btn.sm.danger type="submit" aria-label=(format!("Remove mapping {} of {}", i + 1, c.id)) { "Remove" }
                        }
                    }
                }
                @if bound.is_empty() { li.muted { "Not mapped yet." } }
            }
            details.edit {
                summary {
                    (if bound.is_empty() { "Add a mapping" } else { "Add another mapping" })
                    span.sr { " (" (c.id) ")" }
                }
                (binding_form(name, c, None, pick, law.as_deref(), mode, suggested.as_ref(), csrf))
            }
            details.edit {
                summary { (advanced_title(src)) span.sr { " of " (c.id) } }
                (control_form(name, &ControlDraft::of(c), src, csrf))
                form.inline method="post" action=(surface_path(name, "controls/remove")) {
                    input type="hidden" name="csrf" value=(csrf);
                    input type="hidden" name="id" value=(c.id);
                    button.btn.sm.danger type="submit" { "Remove control…" }
                }
            }
        }
    }
}

const fn advanced_title(src: Src) -> &'static str {
    match src {
        Src::Midi => "Advanced: MIDI message",
        _ => "Advanced: report layout",
    }
}

/// The profiles the daemon knows (built in and installed), as the portal
/// uses them; none when it cannot say.
async fn daemon_profiles(st: &Shared) -> Vec<Profile> {
    match call(st, RequestBody::Query(Query::SurfaceProfiles)).await {
        Ok(Reply::SurfaceProfiles(list)) => list.iter().map(profiles::from_info).collect(),
        _ => Vec::new(),
    }
}

/// The profiles for one surface: the daemon's and the one imported for it.
fn known_profiles(base: &[Profile], extra: Option<&Profile>) -> Vec<Profile> {
    let mut v = base.to_vec();
    if let Some(p) = extra {
        v.retain(|b| b.name != p.name);
        v.push(p.clone());
    }
    v
}

/// The fader law of a fader's gain mappings: the one its mappings use
/// already, else the one of a profile that describes this very control.
fn law_for(spec: &SurfaceSpec, c: &SurfaceControl, extra: &[Profile]) -> Option<Vec<CurvePoint>> {
    if c.kind != ControlKind::Fader {
        return None;
    }
    if let Some(b) = spec
        .bindings
        .iter()
        .find(|b| b.control == c.id && profiles::takes_law(b.action) && !b.curve.is_empty())
    {
        return Some(b.curve.clone());
    }
    extra.iter().find_map(|p| p.curve_for(c))
}

/// The mode a new mute mapping of a button starts with: its profile's, else
/// toggle.
fn mode_for(c: &SurfaceControl, extra: &[Profile]) -> ButtonMode {
    if c.kind != ControlKind::Button {
        return ButtonMode::Toggle;
    }
    extra.iter().find_map(|p| p.mode_for(c)).unwrap_or_default()
}

/// What a profile suggests for a new mapping of `c`: its mode, the output
/// stage, and example channels.
fn suggestion_for(
    c: &SurfaceControl,
    extra: &[Profile],
) -> Option<tessera_surface::profile::ProfileBinding> {
    extra.iter().find_map(|p| p.suggestion_for(c))
}

/// Applying, importing and exporting profiles.
fn profile_block(r: &SurfacesReport, sf: &SurfaceStatus, extra: &[Profile], csrf: &str) -> Markup {
    let spec = &sf.spec;
    let name = &spec.name;
    let midi = src_of(&spec.source) == Src::Midi;
    let detected = device_of(r, sf).and_then(|id| r.detected.iter().find(|d| d.id == id));
    let all: Vec<Profile> = extra
        .iter()
        .filter(|p| p.is_midi() == midi)
        .cloned()
        .collect();
    let (fit, other): (Vec<&Profile>, Vec<&Profile>) =
        all.iter().partition(|p| p.fits(&spec.source, detected));
    html! {
        h3 { "Profile" }
        @if let (Some(p), true) = (fit.first(), spec.controls.is_empty()) {
            p.flash.ok { "This looks like " strong { (profiles::title(p)) } ": apply its profile to add its " (p.controls.len()) " controls at once." }
        }
        p.muted { "A profile is a device's controls (and its fader law) in a file: apply one, then map the controls you use. Profiles are TOML or JSON files anyone can write; see the portal guide." }
        @if !all.is_empty() {
            form.fields.profile-apply method="post" action=(surface_path(name, "profile")) {
                input type="hidden" name="csrf" value=(csrf);
                label { "Profile"
                    select name="profile" {
                        @if !fit.is_empty() {
                            optgroup label="Fits this device" {
                                @for p in &fit { option value=(p.name) { (profiles::title(p)) } }
                            }
                        }
                        @if !other.is_empty() {
                            optgroup label=(if fit.is_empty() { "Profiles" } else { "Other profiles" }) {
                                @for p in &other { option value=(p.name) { (profiles::title(p)) } }
                            }
                        }
                    }
                }
                button.btn.primary type="submit" { "Apply profile" }
            }
        }
        form.fields.profile-import method="post" action=(surface_path(name, "profile/import")) enctype="multipart/form-data" {
            input type="hidden" name="csrf" value=(csrf);
            label { "Import a profile (TOML or JSON)"
                input type="file" name="file" accept=".toml,.json,application/toml,application/json,text/plain" required;
            }
            button.btn type="submit" { "Import and apply" }
        }
        @if !spec.controls.is_empty() {
            p { a.btn.sm href=(surface_path(name, "profile.toml")) download=(format!("{name}.toml")) { "Export this surface as a profile" } }
        }
    }
}

fn learn_form(name: &str, csrf: &str) -> Markup {
    let secs = LEARN_WAIT_MS / 1000;
    html! {
        form.learn method="post" action=(surface_path(name, "learn")) data-learn=(secs) {
            input type="hidden" name="csrf" value=(csrf);
            fieldset {
                legend { "What will you move?" }
                label.check { input type="radio" name="hint" value="button" checked; span { "A button: press it once" } }
                label.check { input type="radio" name="hint" value="fader"; span { "A fader: move it from the bottom all the way " strong { "up" } } }
            }
            button.btn.primary type="submit" { "Learn a control" }
            p.muted {
                "After you click: hands off for a second, then move it when the countdown says so (within "
                (secs) " seconds). Then name the control and choose what it does, and save both at once."
            }
            p.note data-live-local aria-live="polite" {}
        }
    }
}

// ---- forms: a control's layout ---------------------------------------------

/// A control form's values: from a saved control, a learned one, or empty.
#[derive(Debug, Clone, Default)]
struct ControlDraft {
    original: String,
    id: String,
    /// What an empty ID becomes, shown as the field's placeholder.
    id_hint: String,
    label: String,
    kind: Option<ControlKind>,
    report_id: Option<u8>,
    offset: Option<u32>,
    bits: Option<u8>,
    max: Option<u32>,
    inverted: bool,
    active_low: bool,
    motorized: bool,
    code: String,
    /// MIDI: the message, as typed or learned.
    midi_kind: Option<MidiKind>,
    midi_channel: Option<u8>,
    midi_number: Option<u16>,
}

impl ControlDraft {
    fn empty() -> Self {
        Self::default()
    }

    fn of(c: &SurfaceControl) -> Self {
        Self {
            original: c.id.clone(),
            id: c.id.clone(),
            id_hint: String::new(),
            label: c.label.clone().unwrap_or_default(),
            kind: Some(c.kind),
            report_id: c.report_id,
            offset: c.offset,
            bits: c.bits,
            max: c.max,
            inverted: c.inverted,
            active_low: c.active_low,
            motorized: c.motorized,
            code: c.code.clone().unwrap_or_default(),
            midi_kind: c.midi.map(|m| m.kind),
            midi_channel: c.midi.map(|m| m.channel),
            midi_number: c.midi.map(|m| m.number),
        }
    }

    /// A learned layout: no ID yet (it comes from the label), `id_hint`
    /// the one used without a label.
    fn learned(l: &LearnedControl, id_hint: String) -> Self {
        Self {
            original: String::new(),
            id: String::new(),
            id_hint,
            label: String::new(),
            kind: Some(l.kind),
            report_id: if l.midi.is_some() { None } else { l.report_id },
            offset: if l.midi.is_some() {
                None
            } else {
                Some(l.offset)
            },
            bits: if l.midi.is_some() { None } else { Some(l.bits) },
            max: l.max,
            inverted: l.inverted,
            active_low: l.active_low,
            motorized: false,
            code: String::new(),
            midi_kind: l.midi.map(|m| m.kind),
            midi_channel: l.midi.map(|m| m.channel),
            midi_number: l.midi.map(|m| m.number),
        }
    }
}

fn opt<T: ToString>(v: Option<T>) -> String {
    v.map_or_else(String::new, |v| v.to_string())
}

/// A control ID from its label (`Mic mute` → `mic-mute`), unique on the
/// surface; `fader-1`, `button-2`, … for a control without a label.
fn derive_id(label: &str, kind: ControlKind, taken: &[&str]) -> String {
    if label.bytes().any(|b| b.is_ascii_alphanumeric()) {
        return unique(&slug(label), taken);
    }
    let base = kind_word(kind);
    (1..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !taken.contains(&c.as_str()))
        .unwrap_or_else(|| base.to_owned())
}

fn label_field(d: &ControlDraft) -> Markup {
    html! {
        label { "Label" input type="text" name="label" value=(d.label) placeholder="for example Mic" size="16"; }
    }
}

/// The options that fit the control's kind: Inverted for a fader, Active
/// low for a button (both while the kind is unknown). The page's script
/// shows the other one when the kind is changed; the server ignores an
/// option that does not fit the kind saved. A motorized fader stays one
/// (the portal does not offer motors yet).
fn options_fields(d: &ControlDraft) -> Markup {
    let button = d.kind == Some(ControlKind::Button);
    let moving = matches!(d.kind, Some(ControlKind::Fader | ControlKind::Encoder));
    html! {
        fieldset.checks {
            legend { "Options" }
            label.check data-kinds="fader encoder" hidden[button] {
                input type="checkbox" name="inverted" value="true" checked[d.inverted];
                span { "Inverted: the value grows when the fader moves down" }
            }
            label.check data-kinds="button" hidden[moving] {
                input type="checkbox" name="active_low" value="true" checked[d.active_low];
                span { "Active low: the button reads 0 while pressed" }
            }
        }
        @if d.motorized { input type="hidden" name="motorized" value="true"; }
    }
}

/// ID, kind and where the control lives in the device's report.
fn raw_fields(d: &ControlDraft, src: Src) -> Markup {
    // Encoders are not offered yet; a control that is one stays one.
    let mut kinds = vec![
        (ControlKind::Button, "Button"),
        (ControlKind::Fader, "Fader"),
    ];
    if d.kind == Some(ControlKind::Encoder) {
        kinds.push((ControlKind::Encoder, "Encoder"));
    }
    let hint = if d.id_hint.is_empty() {
        "from the label".to_owned()
    } else {
        format!("from the label, else {}", d.id_hint)
    };
    html! {
        label { "ID" input type="text" name="id" value=(d.id) pattern="[A-Za-z0-9_\\-]+" title="Letters, digits, - and _" placeholder=(hint) size="14"; }
        label { "Kind"
            select name="kind" {
                @for (k, l) in kinds { option value=(kind_word(k)) selected[d.kind == Some(k)] { (l) } }
            }
        }
        @match src {
            Src::Hid => {
                label { "Report ID" input type="number" name="report_id" min="0" max="255" value=(opt(d.report_id)) placeholder="none"; }
            label { "Bit offset" input type="number" name="offset" min="0" value=(opt(d.offset)) required; }
            label { "Bits" input type="number" name="bits" min="1" max="32" value=(opt(d.bits)) placeholder="1 or 8"; }
            label { "Max" input type="number" name="max" min="1" value=(opt(d.max)) placeholder="full range"; }
                p.muted { "ID: what mappings refer to; empty means one made from the label. The offset counts bits after the report ID byte. Bits: empty means 1 for a button, 8 otherwise; Max: empty means the field's full range." }
            }
            Src::Midi => {
                label { "Message"
                    select name="midi_kind" {
                        @for (k, value, words) in [
                            (MidiKind::ControlChange, "control_change", "CC (control change)"),
                            (MidiKind::Nrpn, "nrpn", "NRPN"),
                            (MidiKind::Note, "note", "Note"),
                        ] {
                            option value=(value) selected[d.midi_kind == Some(k)] { (words) }
                        }
                    }
                }
                label { "MIDI channel" input type="number" name="midi_channel" min="1" max="16" value=(opt(d.midi_channel)) placeholder="1" required; }
                label { "Number" input type="number" name="midi_number" min="0" max="16383" value=(opt(d.midi_number)) required; }
                label { "Max" input type="number" name="max" min="1" value=(opt(d.max)) placeholder="127 or 16383"; }
                p.muted { "ID: what mappings refer to; empty means one made from the label. Number: the CC or note number (0 to 127), or the NRPN parameter: MSB \u{d7} 128 + LSB (40/00 is 8192). Max: a fader's top value; empty means 127 (NRPN: 16383)." }
            }
            Src::Other => {
                label { "Code" input type="text" name="code" value=(d.code) placeholder="key code or element"; }
            }
        }
    }
}

/// A saved control's editor, and "Add a control by hand": every field shown.
fn control_form(name: &str, d: &ControlDraft, src: Src, csrf: &str) -> Markup {
    html! {
        form.fields.control-form method="post" action=(surface_path(name, "controls")) {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="original" value=(d.original);
            (label_field(d))
            (raw_fields(d, src))
            (options_fields(d))
            button.btn.primary type="submit" { "Save control" }
        }
    }
}

/// A learned control: its label and its mapping in one form, so one Save
/// adds both; the learned report layout folded under "Advanced".
fn learned_form(
    name: &str,
    d: &ControlDraft,
    b: &BindingDraft,
    pick: Pick<'_>,
    src: Src,
    open_raw: bool,
    csrf: &str,
) -> Markup {
    let kind = d.kind.unwrap_or(ControlKind::Button);
    html! {
        form.fields.control-form.learned method="post" action=(surface_path(name, "controls")) {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="original" value=(d.original);
            input type="hidden" name="mapping" value="1";
            (label_field(d))
            div.mapping {
                h3 { "What it does" }
                p.muted { "Choose what it drives now, or leave it empty and map it later." }
                (binding_fields(kind, d.motorized, b, pick, name, None, true))
            }
            (options_fields(d))
            details.advanced open[open_raw] {
                summary { (advanced_title(src)) }
                div.fields { (raw_fields(d, src)) }
            }
            button.btn.primary type="submit" { "Save control and mapping" }
        }
    }
}

/// Learn one control of surface `name` for another page (the mixer
/// editor): the learned layout and the surface as it is.
///
/// # Errors
/// No such surface, its device is not detected, nothing moved, or the
/// daemon's refusal, in words.
pub(crate) async fn learn_control(
    st: &Shared,
    s: &Session,
    name: &str,
    fader: bool,
) -> Result<(LearnedControl, SurfaceSpec), String> {
    let report = report_of(st).await?;
    let Some(sf) = report.surfaces.iter().find(|x| x.spec.name == name) else {
        return Err(format!("There is no surface `{name}`."));
    };
    let Some(device) = device_of(&report, sf) else {
        return Err(format!(
            "The device of `{name}` is not detected: plug it in, then learn."
        ));
    };
    let r = local_call_waiting(
        st,
        RequestBody::Query(Query::LearnControl {
            device,
            timeout_ms: LEARN_WAIT_MS,
            moving_up: fader,
        }),
        Duration::from_millis(u64::from(LEARN_WAIT_MS)),
    )
    .await;
    tracing::info!(session = %s.id, ok = r.is_ok(), "page learn control");
    match r {
        Ok(Reply::LearnedControl(Some(l))) => Ok((l, sf.spec.clone())),
        Ok(Reply::LearnedControl(None)) => Err(format!(
            "Nothing moved in {} s: keep your hands off it for a second after the click, then move it.",
            LEARN_WAIT_MS / 1000
        )),
        Ok(other) => Err(format!("unexpected reply: {other:?}")),
        Err(e) => Err(explain(&e)),
    }
}

/// The fields of a learned control (its label, the layout folded under
/// "Advanced") for a form that posts elsewhere; read back with
/// [`control_from_form`].
pub(crate) fn learned_control_fields(l: &LearnedControl, spec: &SurfaceSpec) -> Markup {
    let taken: Vec<&str> = spec.controls.iter().map(|c| c.id.as_str()).collect();
    let d = ControlDraft::learned(l, derive_id("", l.kind, &taken));
    let src = src_of(&spec.source);
    html! {
        input type="hidden" name="original" value="";
        (label_field(&d))
        @if let Some(m) = &l.midi { p { "Message: " code { (output::midi_words(m)) } } }
        (options_fields(&d))
        details.advanced {
            summary { (advanced_title(src)) }
            div.fields { (raw_fields(&d, src)) }
        }
    }
}

/// The control a form of [`learned_control_fields`] describes, with an
/// id no other control of `spec` has.
///
/// # Errors
/// A field that does not fit, in words.
pub(crate) fn control_from_form(
    form: &[(String, String)],
    spec: &SurfaceSpec,
) -> Result<SurfaceControl, String> {
    let taken: Vec<&str> = spec.controls.iter().map(|c| c.id.as_str()).collect();
    let c = ControlForm::of(form).control(&taken)?;
    if taken.contains(&c.id.as_str()) {
        return Err(format!(
            "A control `{}` exists already; choose another ID.",
            c.id
        ));
    }
    Ok(c)
}

// ---- forms: a binding ------------------------------------------------------

/// A mapping form's values: from a saved binding, the defaults, or a post.
#[derive(Debug, Clone)]
struct BindingDraft {
    /// What it drives.
    what: What,
    /// The chosen channel group, `<dir>:<device>`; empty: none yet.
    target: String,
    /// Ticked boxes: (group, channel).
    ticked: Vec<(String, u16)>,
    /// Ticked router ports and crosspoints (form values).
    ports: Vec<String>,
    crosspoints: Vec<String>,
    /// The followed control, `surface/control`.
    follow: String,
    mode: ButtonMode,
    min_db: String,
    max_db: String,
    taper: Taper,
    step_db: String,
    /// The RX output stage (after the local mix) rather than the channel.
    output: bool,
}

fn target_value(dir: Direction, device: &str) -> String {
    format!("{}:{device}", dir_word(dir))
}

fn db_text(mdb: i32) -> String {
    format!("{}", f64::from(mdb) / 1000.0)
}

/// A dB value in words, with a real minus: `\u{2212}60`, `0`, `6.5`.
fn db_words(mdb: i32) -> String {
    let t = db_text(mdb.abs());
    if mdb < 0 { format!("\u{2212}{t}") } else { t }
}

fn empty_set() -> ChannelSet {
    ChannelSet {
        device: DeviceKey(String::new()),
        direction: Direction::Rx,
        channels: Vec::new(),
    }
}

impl BindingDraft {
    fn of(b: &SurfaceBinding) -> Self {
        let tv = target_value(b.target.direction, &b.target.device.0);
        let channels = b.action.uses_channels();
        Self {
            what: What::of_action(b.action),
            ticked: b
                .target
                .channels
                .iter()
                .map(|c| (tv.clone(), c.get()))
                .collect(),
            target: if channels { tv } else { String::new() },
            ports: b.ports.iter().map(crate::ports::encode).collect(),
            crosspoints: b.crosspoints.iter().map(crate::ports::encode_xp).collect(),
            follow: b
                .follow
                .as_ref()
                .map_or_else(String::new, |f| format!("{}/{}", f.surface, f.control)),
            mode: b.mode,
            min_db: db_text(b.min_mdb),
            max_db: db_text(b.max_mdb),
            taper: b.taper,
            step_db: db_text(b.step_mdb),
            output: matches!(
                b.action,
                BindingAction::OutputGain | BindingAction::OutputMute
            ),
        }
    }

    fn defaults() -> Self {
        let mut d = Self::of(&SurfaceBinding::new("", BindingAction::Gain, empty_set()));
        d.target.clear();
        d.ticked.clear();
        d
    }

    /// What was posted, for the form shown again after a refusal.
    fn from_form(form: &[(String, String)]) -> Self {
        let mut d = Self::defaults();
        d.what = What::of(field(form, "what"));
        field(form, "target").clone_into(&mut d.target);
        d.ticked = form
            .iter()
            .filter_map(|(k, v)| {
                let n = v.trim().parse().ok()?;
                Some((k.strip_prefix("ch:")?.to_owned(), n))
            })
            .collect();
        d.ports = form
            .iter()
            .filter(|(k, _)| k == "port")
            .map(|(_, v)| v.clone())
            .collect();
        d.crosspoints = form
            .iter()
            .filter(|(k, _)| k == "xp")
            .map(|(_, v)| v.clone())
            .collect();
        field(form, "follow").clone_into(&mut d.follow);
        d.mode = mode_of(field(form, "mode"));
        for (name, slot) in [
            ("min_db", &mut d.min_db),
            ("max_db", &mut d.max_db),
            ("step_db", &mut d.step_db),
        ] {
            let v = field(form, name).trim();
            if !v.is_empty() {
                v.clone_into(slot);
            }
        }
        if field(form, "taper") == "linear" {
            d.taper = Taper::Linear;
        }
        d.output = field(form, "stage") == "output";
        d
    }
}

fn mode_of(v: &str) -> ButtonMode {
    match v {
        "momentary" => ButtonMode::Momentary,
        "push_to_talk" => ButtonMode::PushToTalk,
        _ => ButtonMode::Toggle,
    }
}

/// A fader law in words: where 0 dB is, as a share of the fader's travel
/// (the law's raw range; the raw position stays in a tooltip for experts),
/// and what the top gives.
fn law_summary(points: &[CurvePoint]) -> Markup {
    let top = points.last().map_or(0, |p| p.mdb);
    let (low, high) = (
        points.first().map_or(0, |p| p.raw),
        points.last().map_or(0, |p| p.raw),
    );
    let unity = points
        .iter()
        .find(|p| p.mdb == 0)
        .filter(|_| high > low)
        .map(|p| {
            let share =
                (f64::from(p.raw.saturating_sub(low)) * 100.0 / f64::from(high - low)).round();
            (share, p.raw)
        });
    html! {
        "Follows the device's own fader law (from its profile): off at the bottom"
        @if let Some((share, raw)) = unity {
            ", "
            span title=(format!("Raw position {raw} of {low}\u{2013}{high}")) { "0 dB at about " (share) "\u{a0}% of travel" }
        }
        ", " (slider_text(Gain::MilliDb(top))) " at the top."
    }
}

/// The controls of other surfaces a control of kind `kind` can follow (a
/// fader follows a fader, a button a button): (`surface/control`, words).
fn followable(pick: Pick<'_>, me: &str, kind: ControlKind) -> Vec<(String, String)> {
    let fader = kind != ControlKind::Button;
    pick.report
        .iter()
        .flat_map(|r| r.surfaces.iter())
        .filter(|s| s.spec.name != me)
        .flat_map(|s| {
            s.spec
                .controls
                .iter()
                .filter(move |c| (c.kind != ControlKind::Button) == fader)
                .map(move |c| {
                    (
                        format!("{}/{}", s.spec.name, c.id),
                        format!(
                            "{} \u{b7} {}{}",
                            s.spec.name,
                            c.label.as_deref().unwrap_or(&c.id),
                            if c.motorized { " (motorized)" } else { "" }
                        ),
                    )
                })
        })
        .collect()
}

/// What the control drives (channels, the speaker volume, router ports or
/// crosspoints, or a control of another surface) and how. `every_kind`:
/// also the parts for the other kind, hidden, for a form whose kind can
/// still change (the script shows the one that fits; without it their
/// defaults are posted and ignored). Each target picker is shown with the
/// script only when chosen (`data-what`).
#[allow(clippy::too_many_arguments)]
fn binding_fields(
    kind: ControlKind,
    motorized: bool,
    v: &BindingDraft,
    pick: Pick<'_>,
    me: &str,
    law: Option<&[CurvePoint]>,
    every_kind: bool,
) -> Markup {
    let button = kind == ControlKind::Button;
    let fader = kind == ControlKind::Fader;
    let devices = pick.devices();
    let host = pick.host().is_some() || v.what == What::Host;
    let follows = followable(pick, me, kind);
    let patched: Vec<CrosspointKey> = pick
        .router
        .iter()
        .flat_map(|r| r.crosspoints.iter())
        .filter(|x| !crate::ports::is_bridge(&x.origin))
        .map(|x| x.crosspoint.key())
        .collect();
    html! {
        fieldset.what {
            legend { "What it drives" }
            label.check { input type="radio" name="what" value="channels" checked[v.what == What::Channels]; span { "Channels" } }
            @if host { label.check { input type="radio" name="what" value="host" checked[v.what == What::Host]; span { "Speaker volume (the host volume; a button mutes it)" } } }
            @else {
                // Shown, not hidden: a profile may suggest it, and the reason
                // it cannot be chosen is a missing table, not a missing feature.
                // The reason sits in the label: it is part of the choice's
                // accessible name, and each form on the page has its own.
                label.check.unavailable {
                    input type="radio" name="what" value="host" disabled;
                    // tesserad reads `config.toml` and `config.d/*.toml`
                    // only: a file elsewhere in /etc/tessera/ is ignored.
                    span {
                        "Speaker volume (the host volume; a button mutes it): not available. It needs "
                        code { "[host_volume]" } " in the configuration (the card and control the daemon owns; see the configuration guide). Add it to a file in "
                        code { "/etc/tessera/config.d/" } " (for example " code { "60-host-volume.toml" } "), then reload on the "
                        a href="/config" { "Configuration page" } "."
                    }
                }
            }
            @if pick.router.is_some() {
                label.check { input type="radio" name="what" value="ports" checked[v.what == What::Ports]; span { "Router ports" } }
                label.check { input type="radio" name="what" value="crosspoints" checked[v.what == What::Crosspoints]; span { "Router crosspoints" } }
            }
            label.check { input type="radio" name="what" value="follow" checked[v.what == What::Follow]; span { "A control of another surface (follow)" } }
        }
        fieldset.targets data-what="channels" {
            legend { "Channels" }
            @if devices.is_empty() { p.muted { "The daemon reported no devices." } }
            @else { p.muted.pick { "Choose RX or TX of a device, then tick its channels. Only the chosen group's ticks count." } }
            @for d in devices {
                @for (dir, chans) in [(Direction::Rx, &d.rx_channels), (Direction::Tx, &d.tx_channels)] {
                    @if !chans.is_empty() {
                        @let tv = target_value(dir, &d.key.0);
                        @let here = v.target == tv;
                        fieldset.target {
                            legend {
                                label.check {
                                    input type="radio" name="target" value=(tv) checked[here];
                                    span { @if devices.len() > 1 { bdi { (d.name) } " · " } (dir_label(dir)) }
                                }
                            }
                            div.chans {
                                @for ch in chans {
                                    @let n = ch.channel.get();
                                    label.check {
                                        input type="checkbox" name=(format!("ch:{tv}")) value=(n) checked[v.ticked.contains(&(tv.clone(), n))];
                                        span { (n) " · " bdi { (ch.label) } }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            @if button || every_kind {
                fieldset data-kinds="button" hidden[!button] {
                    legend { "What it mutes" }
                    label.check { input type="radio" name="stage" value="input" checked[!v.output]; span { "Mute input: the channel itself" } }
                    label.check { input type="radio" name="stage" value="output" checked[v.output]; span { "Mute output (RX, after the mix): a master or speaker mute" } }
                }
            }
            @if fader || every_kind {
                fieldset data-kinds="fader" hidden[!fader] {
                    legend { "What it sets" }
                    label.check { input type="radio" name="stage" value="input" checked[!v.output]; span { "Input level (before the mix)" } }
                    label.check { input type="radio" name="stage" value="output" checked[v.output]; span { "Output level (after the mix, e.g. a master): RX channels only" } }
                }
            }
        }
        @if let Some(r) = pick.router {
            fieldset.targets data-what="ports" {
                legend { "Router ports" }
                div.chans {
                    @for p in &r.ports {
                        @let val = crate::ports::encode(&p.port);
                        label.check { input type="checkbox" name="port" value=(val) checked[v.ports.contains(&val)]; span { bdi { (crate::ports::described(r, p)) } } }
                    }
                }
            }
            fieldset.targets data-what="crosspoints" {
                legend { "Router crosspoints (patched)" }
                @if patched.is_empty() { p.muted { "None patched: patch on the " a href="/router" { "Router page" } " first." } }
                div.chans {
                    @for k in &patched {
                        @let val = crate::ports::encode_xp(k);
                        label.check { input type="checkbox" name="xp" value=(val) checked[v.crosspoints.contains(&val)]; span { (crate::ports::xp_words(Some(r), k)) } }
                    }
                }
            }
        }
        fieldset.targets data-what="follow" {
            legend { "The control it drives" }
            @if follows.is_empty() {
                p.muted { "No other surface has a " (if button { "button" } else { "fader" }) " yet." }
            } @else {
                label { "Control"
                    select name="follow" {
                        option value="" selected[v.follow.is_empty()] { "choose\u{2026}" }
                        @for (val, words) in &follows { option value=(val) selected[v.follow == *val] { (words) } }
                    }
                }
            }
            p.muted.follow-about {
                "Moving this control is the same as moving the other one: the other control's mappings apply (its levels and mutes, and its own follows in turn), a motorized fader there moves to match, a button's light follows. "
                "The value travels in dB: this mapping's fader law turns this control into dB, the other control's own first fader mapping turns it back (\u{2212}60\u{2026}0 dB when it has none). "
                "The follow itself owns no level. For feedback both ways, give the other surface a follow back; longer chains that loop are refused."
            }
        }
        @if button || every_kind {
            div data-kinds="button" hidden[!button] {
                fieldset {
                    legend { "When pressed" }
                    @for (m, value, words) in [
                        (ButtonMode::Toggle, "toggle", "Toggle: each press mutes or unmutes"),
                        (ButtonMode::Momentary, "momentary", "Hold to mute (cough button): muted while held"),
                        (ButtonMode::PushToTalk, "push_to_talk", "Push to talk: live while held, muted otherwise"),
                    ] {
                        label.check { input type="radio" name="mode" value=(value) checked[v.mode == m]; span { (words) } }
                    }
                }
            }
        }
        @if fader || every_kind {
            div data-kinds="fader" hidden[!fader] {
                @if let Some(pts) = law {
                    p.law { (law_summary(pts)) }
                }
                fieldset.range hidden[law.is_some()] {
                    legend { "Fader range" }
                    label { "Bottom (dB, just above off)" input type="number" name="min_db" step="0.5" min=(GAIN_FLOOR_MDB / 1000) max=(GAIN_CEILING_MDB / 1000) value=(v.min_db) required; }
                    label { "Top (dB)" input type="number" name="max_db" step="0.5" min=(GAIN_FLOOR_MDB / 1000) max=(GAIN_CEILING_MDB / 1000) value=(v.max_db) required; }
                    label { "Taper"
                        select name="taper" {
                            option value="db" selected[v.taper == Taper::Db] { "Even dB per travel" }
                            option value="linear" selected[v.taper == Taper::Linear] { "Linear amplitude" }
                        }
                    }
                }
                p.muted {
                    @if motorized {
                        "Motorized: the level can also be set elsewhere, and the fader moves to follow it."
                    } @else {
                        "While the surface is connected, this fader alone sets the level it drives (channel input level, speaker volume, port level or crosspoint gain): elsewhere it is shown read-only. An output level is never owned, and mute always works."
                    }
                }
            }
        }
        @if kind == ControlKind::Encoder {
            label { "dB per step" input type="number" name="step_db" step="0.5" min="0.5" max="24" value=(v.step_db) required; }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn binding_form(
    name: &str,
    c: &SurfaceControl,
    existing: Option<(usize, &SurfaceBinding)>,
    pick: Pick<'_>,
    law: Option<&[CurvePoint]>,
    mode: ButtonMode,
    suggested: Option<&tessera_surface::profile::ProfileBinding>,
    csrf: &str,
) -> Markup {
    let devices = pick.devices();
    let draft = existing.map_or_else(
        || {
            let mut d = BindingDraft::defaults();
            d.mode = mode;
            if let Some(sg) = suggested {
                d.output = profiles::is_output(sg.action);
                // A profile's speaker volume suggestion (the CQ's Main):
                // offered only where this host has a host volume.
                if matches!(
                    sg.action,
                    BindingAction::HostVolume | BindingAction::HostMute
                ) && pick.host().is_some()
                {
                    d.what = What::Host;
                }
                // Example channels, when they can only mean one device.
                if let (Some(dir), [dev]) = (sg.suggested_direction(), devices) {
                    let tv = target_value(dir, &dev.key.0);
                    d.ticked = sg.channels.iter().map(|n| (tv.clone(), *n)).collect();
                    d.target = tv;
                }
            }
            d
        },
        |(_, b)| BindingDraft::of(b),
    );
    html! {
        form.binding method="post" action=(surface_path(name, "bindings")) data-binding-form {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="control" value=(c.id);
            input type="hidden" name="index" value=(existing.map_or_else(String::new, |(i, _)| i.to_string()));
            (binding_fields(c.kind, c.motorized, &draft, pick, name, law, false))
            button.btn.primary type="submit" { "Save mapping" }
        }
    }
}

// ---- changing a surface ----------------------------------------------------

async fn send(st: &Shared, s: &Session, cmd: Command) -> Result<(), (StatusCode, String)> {
    let kind = cmd.kind();
    let r = call(st, RequestBody::Command(cmd)).await;
    tracing::info!(session = %s.id, command = kind, ok = r.is_ok(), "page command");
    r.map(|_| ()).map_err(|e| refusal(&e))
}

/// Read the surface `name`, change a copy with `edit` and send it whole.
async fn edit_surface(
    st: &Shared,
    s: &Session,
    name: &str,
    edit: impl FnOnce(&mut SurfaceSpec) -> Result<String, String>,
) -> Result<String, (StatusCode, String)> {
    let r = report_of(st)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    let Some(sf) = r.surfaces.into_iter().find(|x| x.spec.name == name) else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("There is no surface `{name}`."),
        ));
    };
    let mut spec = sf.spec;
    let msg = edit(&mut spec).map_err(|m| (StatusCode::BAD_REQUEST, m))?;
    send(st, s, Command::SetSurface { surface: spec }).await?;
    Ok(msg)
}

/// A question before a removal.
pub(crate) struct Question<'a> {
    /// The page title, also the button that confirms.
    pub(crate) title: &'a str,
    pub(crate) text: Markup,
    /// The form posted again, with `confirm=yes`.
    pub(crate) action: String,
    pub(crate) hidden: Vec<(&'a str, String)>,
    /// Where Cancel goes.
    pub(crate) back_to: String,
    /// The navigation entry of the page.
    pub(crate) active: &'a str,
}

/// Ask before a removal: the same form again with `confirm=yes`.
pub(crate) fn ask(st: &Shared, headers: &HeaderMap, s: &Session, q: &Question<'_>) -> Response {
    let body = html! {
        div.grid { section.card {
            h2 { (q.title) }
            (q.text)
            form.confirm method="post" action=(q.action) {
                input type="hidden" name="csrf" value=(s.csrf);
                @for (k, v) in &q.hidden { input type="hidden" name=(k) value=(v); }
                input type="hidden" name="confirm" value="yes";
                button.btn.danger type="submit" { (q.title) }
                a href=(q.back_to) { "Cancel" }
            }
        } }
    };
    layout(
        &view(q.title, q.active),
        &Chrome::of(st, headers),
        Some(s),
        None,
        &body,
    )
    .into_response()
}

/// `POST /surfaces/add`.
#[derive(Debug, Deserialize)]
pub struct AddForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    device: String,
    #[serde(default)]
    name: String,
}

/// `POST /surfaces/add`: a new surface for a detected HID device, enabled,
/// without controls yet.
pub async fn add(State(st): State<Shared>, headers: HeaderMap, Form(f): Form<AddForm>) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let name = f.name.trim().to_owned();
    let r: Result<String, (StatusCode, String)> = async {
        if !ident_ok(&name) {
            return Err((
                StatusCode::BAD_REQUEST,
                "A surface name uses letters, digits, - and _ only.".into(),
            ));
        }
        let report = report_of(&st)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
        if report.surfaces.iter().any(|x| x.spec.name == name) {
            return Err((
                StatusCode::CONFLICT,
                format!("A surface named `{name}` exists already; choose another name."),
            ));
        }
        let Some(source) = report
            .detected
            .iter()
            .find(|d| d.id == f.device)
            .and_then(|d| d.source.clone())
        else {
            return Err((
                StatusCode::NOT_FOUND,
                "That device is no longer detected, or cannot be a surface.".into(),
            ));
        };
        let spec = SurfaceSpec::new(name.clone(), source, Vec::new(), Vec::new());
        send(&st, &s, Command::SetSurface { surface: spec }).await?;
        Ok(format!(
            "Surface `{name}` added. Learn its controls, then map them."
        ))
    }
    .await;
    let to = if r.is_ok() {
        surface_anchor(&name)
    } else {
        "/surfaces#detected".into()
    };
    answer(&st, &token, &headers, &to, r, serde_json::json!({}))
}

/// `POST /surfaces/add-tcp`: a new surface on MIDI over TCP (`name`,
/// `host`, `port`), enabled, without controls yet.
pub async fn add_tcp(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, field(&form, "csrf")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let name = field(&form, "name").trim().to_owned();
    let host = field(&form, "host").trim().to_owned();
    let r: Result<String, (StatusCode, String)> = async {
        let bad = |m: &str| (StatusCode::BAD_REQUEST, m.to_owned());
        if !ident_ok(&name) {
            return Err(bad("A surface name uses letters, digits, - and _ only."));
        }
        if host.is_empty()
            || host.len() > 253
            || host.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(bad("Give the desk's host name or address (no spaces)."));
        }
        let port = field(&form, "port")
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| bad("The TCP port is a number from 1 to 65535."))?;
        let report = report_of(&st)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
        if report.surfaces.iter().any(|x| x.spec.name == name) {
            return Err((
                StatusCode::CONFLICT,
                format!("A surface named `{name}` exists already; choose another name."),
            ));
        }
        let spec = SurfaceSpec::new(
            name.clone(),
            SurfaceSource::TcpMidi {
                host: host.clone(),
                port,
            },
            Vec::new(),
            Vec::new(),
        );
        send(&st, &s, Command::SetSurface { surface: spec }).await?;
        Ok(format!(
            "Surface `{name}` added: tesserad connects to {host} port {port}. Apply a profile or learn its controls, then map them."
        ))
    }
    .await;
    let to = if r.is_ok() {
        surface_anchor(&name)
    } else {
        "/surfaces#network-surface".into()
    };
    back(&st, &token, &to, r.map_err(|(_, m)| m))
}

/// `POST /surfaces/{name}/enabled`.
#[derive(Debug, Deserialize)]
pub struct EnabledForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    enabled: String,
}

/// `POST /surfaces/{name}/enabled`: switch a surface on or off.
pub async fn enabled(
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
    let r = edit_surface(&st, &s, &name, |spec| {
        spec.enabled = on;
        Ok(if on {
            format!("Surface `{name}` enabled.")
        } else {
            format!("Surface `{name}` disabled: it leaves the device alone and sets no gain.")
        })
    })
    .await;
    answer(
        &st,
        &token,
        &headers,
        &surface_anchor(&name),
        r,
        serde_json::json!({}),
    )
}

/// A removal form: the token, and `confirm=yes` once asked.
#[derive(Debug, Deserialize)]
pub struct RemoveForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    confirm: String,
    #[serde(default)]
    id: String,
}

/// `POST /surfaces/{name}/remove`: asks first, then `RemoveSurface`.
pub async fn remove(
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
            p { "Remove the surface " strong { bdi { (name) } } " with its learnt controls and mappings?" }
            p.muted { "Channels whose gain its faders set can then be set from elsewhere again. Their current gain and mute stay as they are." }
        };
        let q = Question {
            title: "Remove surface",
            text,
            action: surface_path(&name, "remove"),
            hidden: Vec::new(),
            back_to: surface_anchor(&name),
            active: "/surfaces",
        };
        return ask(&st, &headers, &s, &q);
    }
    let r = send(&st, &s, Command::RemoveSurface { name: name.clone() })
        .await
        .map(|()| format!("Surface `{name}` removed."));
    let to = if r.is_ok() {
        "/surfaces".to_owned()
    } else {
        surface_anchor(&name)
    };
    back(&st, &token, &to, r.map_err(|(_, m)| m))
}

/// `POST /surfaces/{name}/learn`.
#[derive(Debug, Deserialize)]
pub struct LearnForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    hint: String,
}

/// `POST /surfaces/{name}/learn`: watch the surface's device for up to
/// [`LEARN_WAIT_MS`] (a fader is moved up, which tells an inverted one),
/// then ask for the control's label and its mapping in one form.
pub async fn learn(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(f): Form<LearnForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let to = surface_anchor(&name);
    let report = match report_of(&st).await {
        Ok(r) => r,
        Err(e) => return back(&st, &token, &to, Err(e)),
    };
    let Some(sf) = report.surfaces.iter().find(|x| x.spec.name == name) else {
        return back(
            &st,
            &token,
            "/surfaces",
            Err(format!("There is no surface `{name}`.")),
        );
    };
    let Some(device) = device_of(&report, sf) else {
        return back(
            &st,
            &token,
            &to,
            Err(format!(
                "The device of `{name}` is not detected: plug it in, then learn."
            )),
        );
    };
    let r = local_call_waiting(
        &st,
        RequestBody::Query(Query::LearnControl {
            device,
            timeout_ms: LEARN_WAIT_MS,
            moving_up: f.hint == "fader",
        }),
        Duration::from_millis(u64::from(LEARN_WAIT_MS)),
    )
    .await;
    tracing::info!(session = %s.id, ok = r.is_ok(), "page learn control");
    let learned = match r {
        Ok(Reply::LearnedControl(Some(l))) => l,
        Ok(Reply::LearnedControl(None)) => {
            return back(
                &st,
                &token,
                &to,
                Err(format!(
                    "Nothing moved in {} s. Click \u{201c}Learn a control\u{201d}, keep your hands off it for a second, then press the button or move the fader when the countdown says so.",
                    LEARN_WAIT_MS / 1000
                )),
            );
        }
        Ok(other) => {
            return back(
                &st,
                &token,
                &to,
                Err(format!("unexpected reply: {other:?}")),
            );
        }
        Err(e) => return back(&st, &token, &to, Err(explain(&e))),
    };
    let taken: Vec<&str> = sf.spec.controls.iter().map(|c| c.id.as_str()).collect();
    let id_hint = derive_id("", learned.kind, &taken);
    let mut notes: Vec<String> = Vec::new();
    if let Some(m) = &learned.midi {
        if let Some(c) = sf.spec.controls.iter().find(|c| c.midi.as_ref() == Some(m)) {
            notes.push(format!(
                "This message ({}) is already the control `{}`: saving adds a second control for it.",
                output::midi_words(m),
                c.id
            ));
        }
    }
    match (f.hint.as_str(), learned.kind) {
        _ if learned.midi.is_some() => {}
        ("fader", ControlKind::Button) => notes.push(
            "You chose a fader, but a single bit changed, as a button does. If you moved a fader, learn it again and move it the whole way."
                .into(),
        ),
        ("button", ControlKind::Fader | ControlKind::Encoder) => notes.push(
            "A value of several bits changed, as a fader does. Its direction is unknown: learn it again choosing \u{201c}A fader\u{201d} and move it up, or set Inverted yourself."
                .into(),
        ),
        _ => {}
    }
    let src = src_of(&sf.spec.source);
    let draft = ControlDraft::learned(&learned, id_hint);
    let status = status_of(&st).await.ok();
    let router = crate::ports::router_of(&st).await.ok();
    let pick = Pick {
        status: status.as_ref(),
        router: router.as_ref(),
        report: Some(&report),
    };
    let body = html! {
        div.grid { section.card {
            h2 { "A " (kind_word(learned.kind)) " moved" span.n { "surface " bdi { (name) } } }
            @for n in &notes { p.flash.bad { (n) } }
            p { "Name it and choose what it does: one Save adds the control and its mapping." }
            @if let Some(m) = &learned.midi { p { "Message: " code { (output::midi_words(m)) } } }
            (learned_form(&name, &draft, &BindingDraft::defaults(), pick, src, false, &s.csrf))
            p { a href=(to) { "Back without saving" } }
        } }
    };
    layout(
        &view("Learned control", "/surfaces"),
        &Chrome::of(&st, &headers),
        Some(&s),
        st.take_flash(&token),
        &body,
    )
    .into_response()
}

/// `POST /surfaces/{name}/controls`: a control's fields as posted.
#[derive(Debug, Default)]
struct ControlForm {
    original: String,
    id: String,
    label: String,
    kind: String,
    report_id: String,
    offset: String,
    bits: String,
    max: String,
    /// Present only in the form of a source that is not HID.
    code: Option<String>,
    /// Present only in the form of a MIDI source.
    midi_kind: Option<String>,
    midi_channel: String,
    midi_number: String,
    inverted: bool,
    active_low: bool,
    motorized: bool,
}

fn midi_kind_of(s: &str) -> Option<MidiKind> {
    match s {
        "control_change" => Some(MidiKind::ControlChange),
        "nrpn" => Some(MidiKind::Nrpn),
        "note" => Some(MidiKind::Note),
        _ => None,
    }
}

fn number<T: std::str::FromStr>(s: &str, what: &str) -> Result<Option<T>, String> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse()
        .map(Some)
        .map_err(|_| format!("{what}: `{t}` is not a whole number in range."))
}

fn kind_of(s: &str) -> Option<ControlKind> {
    match s {
        "button" => Some(ControlKind::Button),
        "fader" => Some(ControlKind::Fader),
        "encoder" => Some(ControlKind::Encoder),
        _ => None,
    }
}

impl ControlForm {
    fn of(form: &[(String, String)]) -> Self {
        let text = |k: &str| field(form, k).to_owned();
        let has = |k: &str| form.iter().any(|(n, _)| n == k);
        Self {
            original: text("original"),
            id: text("id"),
            label: text("label"),
            kind: text("kind"),
            report_id: text("report_id"),
            offset: text("offset"),
            bits: text("bits"),
            max: text("max"),
            code: has("code").then(|| text("code")),
            midi_kind: has("midi_kind").then(|| text("midi_kind")),
            midi_channel: text("midi_channel"),
            midi_number: text("midi_number"),
            inverted: has("inverted"),
            active_low: has("active_low"),
            motorized: has("motorized"),
        }
    }

    /// The values typed, for the form shown again after a refusal.
    fn draft(&self) -> ControlDraft {
        ControlDraft {
            original: self.original.clone(),
            id: self.id.clone(),
            id_hint: String::new(),
            label: self.label.clone(),
            kind: kind_of(&self.kind),
            report_id: self.report_id.trim().parse().ok(),
            offset: self.offset.trim().parse().ok(),
            bits: self.bits.trim().parse().ok(),
            max: self.max.trim().parse().ok(),
            inverted: self.inverted,
            active_low: self.active_low,
            motorized: self.motorized,
            code: self.code.clone().unwrap_or_default(),
            midi_kind: self.midi_kind.as_deref().and_then(midi_kind_of),
            midi_channel: self.midi_channel.trim().parse().ok(),
            midi_number: self.midi_number.trim().parse().ok(),
        }
    }

    /// Which kind of source the posted form was for.
    const fn src(&self) -> Src {
        if self.midi_kind.is_some() {
            Src::Midi
        } else if self.code.is_some() {
            Src::Other
        } else {
            Src::Hid
        }
    }

    /// The MIDI message, when the form is for a MIDI source.
    fn midi(&self) -> Result<Option<MidiAddress>, String> {
        let Some(k) = self.midi_kind.as_deref() else {
            return Ok(None);
        };
        let kind = midi_kind_of(k).ok_or("Choose the kind of MIDI message.")?;
        let channel: u8 = number(&self.midi_channel, "MIDI channel")?.unwrap_or(1);
        if !(1..=16).contains(&channel) {
            return Err("MIDI channel: from 1 to 16.".into());
        }
        let Some(n) = number::<u16>(&self.midi_number, "Number")? else {
            return Err("Number: give the CC, note or NRPN number.".into());
        };
        let top = if kind == MidiKind::Nrpn { 16_383 } else { 127 };
        if n > top {
            return Err(format!("Number: from 0 to {top} for this kind of message."));
        }
        Ok(Some(MidiAddress::new(kind, channel, n)))
    }

    /// The control; an empty ID is made from the label, unique among
    /// `taken`.
    fn control(&self, taken: &[&str]) -> Result<SurfaceControl, String> {
        let Some(kind) = kind_of(&self.kind) else {
            return Err("Choose the kind: button or fader.".into());
        };
        let label = self.label.trim();
        let typed = self.id.trim();
        let id = if typed.is_empty() {
            derive_id(label, kind, taken)
        } else {
            typed.to_owned()
        };
        if !ident_ok(&id) {
            return Err("A control ID uses letters, digits, - and _ only.".into());
        }
        let bits: Option<u8> = number(&self.bits, "Bits")?;
        if bits.is_some_and(|b| !(1..=32).contains(&b)) {
            return Err("Bits: from 1 to 32.".into());
        }
        let code = self.code.as_deref().unwrap_or_default().trim();
        let midi = self.midi()?;
        Ok(SurfaceControl {
            id,
            label: (!label.is_empty()).then(|| label.to_owned()),
            kind,
            report_id: number(&self.report_id, "Report ID")?,
            offset: number(&self.offset, "Bit offset")?,
            bits,
            max: number(&self.max, "Max")?,
            inverted: kind != ControlKind::Button && self.inverted,
            active_low: kind == ControlKind::Button && self.active_low,
            motorized: kind == ControlKind::Fader && self.motorized,
            code: (!code.is_empty()).then(|| code.to_owned()),
            midi,
        })
    }
}

const fn action_of(k: ControlKind) -> BindingAction {
    match k {
        ControlKind::Button => BindingAction::Mute,
        _ => BindingAction::Gain,
    }
}

/// Did the learn page's form choose channels (a group or a box)?
fn mapping_chosen(form: &[(String, String)]) -> bool {
    let any = |name: &str| form.iter().any(|(k, v)| k == name && !v.is_empty());
    match What::of(field(form, "what")) {
        What::Channels => {
            !field(form, "target").is_empty() || form.iter().any(|(k, _)| k.starts_with("ch:"))
        }
        What::Host => true,
        What::Ports => any("port"),
        What::Crosspoints => any("xp"),
        What::Follow => any("follow"),
    }
}

/// A mapping in words for a confirmation, without the ownership note.
fn mapping_words(b: &SurfaceBinding, c: &SurfaceControl, status: Option<&Status>) -> String {
    let words = binding_words(b, Some(c), false, None, status);
    words
        .strip_suffix(", owns gain while connected")
        .or_else(|| words.strip_suffix(", owns the level while connected"))
        .map_or_else(|| words.clone(), str::to_owned)
}

/// `POST /surfaces/{name}/controls`: add a control, or replace the one
/// named `original` (its mappings follow a new ID; mappings that no longer
/// fit a new kind are dropped, and the message says so). An empty ID is
/// made from the label. From the learn page (`mapping=1`) the same post adds
/// the control's first mapping when channels were chosen: one `SetSurface`
/// for both, or nothing when either is refused.
pub async fn save_control(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, field(&form, "csrf")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let f = ControlForm::of(&form);
    let with_mapping = field(&form, "mapping") == "1";
    let original = f.original.trim().to_owned();
    let mut saved = String::new();
    let extra = known_profiles(&daemon_profiles(&st).await, imported(&st, &name).as_ref());
    // For the message: the channels by their labels.
    let status = status_of(&st).await.ok();
    let r = edit_surface(&st, &s, &name, |spec| {
        let taken: Vec<&str> = spec
            .controls
            .iter()
            .map(|c| c.id.as_str())
            .filter(|id| *id != original)
            .collect();
        let control = f.control(&taken)?;
        let id = control.id.clone();
        if taken.contains(&id.as_str()) {
            return Err(format!(
                "A control `{id}` exists already; choose another ID."
            ));
        }
        let mapping = if with_mapping && mapping_chosen(&form) {
            let base = SurfaceBinding::new(id.clone(), action_of(control.kind), empty_set());
            let law = law_for(spec, &control, &extra);
            let mut base = base;
            base.mode = mode_for(&control, &extra);
            let b = binding_of(&form, &control, base, law.as_deref(), devices_of(status.as_ref()))?;
            Some((mapping_words(&b, &control, status.as_ref()), b))
        } else {
            None
        };
        saved.clone_from(&id);
        let mut dropped = 0;
        if original.is_empty() {
            spec.controls.push(control);
        } else {
            let Some(slot) = spec.controls.iter_mut().find(|c| c.id == original) else {
                return Err(format!(
                    "There is no control `{original}` any more; reload the page."
                ));
            };
            *slot = control;
            let kind = slot.kind;
            for b in spec.bindings.iter_mut().filter(|b| b.control == original) {
                b.control.clone_from(&id);
            }
            let before = spec.bindings.len();
            spec.bindings
                .retain(|b| b.control != id || profiles::action_fits(kind, b.action));
            dropped = before - spec.bindings.len();
        }
        Ok(match mapping {
            Some((words, b)) => {
                spec.bindings.push(b);
                format!("Control `{id}` saved and mapped: {words}")
            }
            None if with_mapping => format!(
                "Control `{id}` saved, not mapped yet: use \u{201c}Add a mapping\u{201d} on its card."
            ),
            None if dropped > 0 => format!(
                "Control `{id}` saved; {dropped} mapping(s) that no longer fit its kind were removed."
            ),
            None => format!("Control `{id}` saved."),
        })
    })
    .await;
    match r {
        Ok(m) => back(&st, &token, &control_anchor(&name, &saved), Ok(m)),
        Err(e) => {
            // The form again with what was typed: a learned layout and the
            // channels chosen are not lost.
            let draft = f.draft();
            let src = f.src();
            let again = if with_mapping {
                let status = status_of(&st).await.ok();
                let router = crate::ports::router_of(&st).await.ok();
                let report = report_of(&st).await.ok();
                let pick = Pick {
                    status: status.as_ref(),
                    router: router.as_ref(),
                    report: report.as_ref(),
                };
                let b = BindingDraft::from_form(&form);
                learned_form(&name, &draft, &b, pick, src, true, &s.csrf)
            } else {
                control_form(&name, &draft, src, &s.csrf)
            };
            control_page(&st, &headers, &s, &name, &again, e)
        }
    }
}

fn control_page(
    st: &Shared,
    headers: &HeaderMap,
    s: &Session,
    name: &str,
    again: &Markup,
    (code, msg): (StatusCode, String),
) -> Response {
    let body = html! {
        div.grid { section.card {
            h2 { "Control not saved" span.n { "surface " bdi { (name) } } }
            (error_box(&msg))
            (again)
            p { a href=(surface_anchor(name)) { "Back without saving" } }
        } }
    };
    (
        code,
        layout(
            &view("Control not saved", "/surfaces"),
            &Chrome::of(st, headers),
            Some(s),
            None,
            &body,
        ),
    )
        .into_response()
}

/// `POST /surfaces/{name}/controls/remove`: asks first, then removes the
/// control and its mappings.
pub async fn remove_control(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(f): Form<RemoveForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let id = f.id.clone();
    if f.confirm != "yes" {
        let text = html! {
            p { "Remove the control " strong { bdi { (id) } } " of " bdi { (name) } " and its mappings?" }
        };
        let q = Question {
            title: "Remove control",
            text,
            action: surface_path(&name, "controls/remove"),
            hidden: vec![("id", id.clone())],
            back_to: control_anchor(&name, &id),
            active: "/surfaces",
        };
        return ask(&st, &headers, &s, &q);
    }
    let r = edit_surface(&st, &s, &name, |spec| {
        let before = spec.controls.len();
        spec.controls.retain(|c| c.id != id);
        if spec.controls.len() == before {
            return Err(format!("There is no control `{id}` any more."));
        }
        spec.bindings.retain(|b| b.control != id);
        Ok(format!("Control `{id}` removed."))
    })
    .await;
    back(&st, &token, &surface_anchor(&name), r.map_err(|(_, m)| m))
}

fn field<'a>(form: &'a [(String, String)], name: &str) -> &'a str {
    form.iter()
        .find(|(k, _)| k == name)
        .map_or("", |(_, v)| v.as_str())
}

fn db_field(form: &[(String, String)], name: &str, what: &str) -> Result<i32, String> {
    let v: f64 = field(form, name)
        .trim()
        .replace('\u{2212}', "-")
        .parse()
        .map_err(|_| format!("{what}: give a number of dB."))?;
    let m = (v * 1000.0).round();
    if !(f64::from(GAIN_FLOOR_MDB)..=f64::from(GAIN_CEILING_MDB)).contains(&m) {
        return Err(format!(
            "{what}: from \u{2212}{} to +{} dB.",
            -GAIN_FLOOR_MDB / 1000,
            GAIN_CEILING_MDB / 1000
        ));
    }
    #[allow(clippy::cast_possible_truncation)]
    Ok(m as i32)
}

/// The devices a status lists (none without one).
fn devices_of(status: Option<&Status>) -> &[DeviceStatus] {
    status.map_or(&[], |s| s.devices.as_slice())
}

/// A channel group (`rx:main`) in the words of the form's legends: `RX`
/// on a host with one device, `Studio · RX` (the device's name, never its
/// key) with several.
fn group_words(tv: &str, devices: &[DeviceStatus]) -> String {
    match tv.split_once(':') {
        Some((dir, device)) => {
            let dir = dir.to_uppercase();
            match crate::ports::device_name(devices, &DeviceKey(device.to_owned())) {
                Some(name) => format!("{name} \u{b7} {dir}"),
                None => dir,
            }
        }
        None => tv.to_owned(),
    }
}

/// The channel target and action of a channels mapping form.
fn channel_target(
    form: &[(String, String)],
    c: &SurfaceControl,
    devices: &[DeviceStatus],
) -> Result<(BindingAction, ChannelSet), String> {
    let target = field(form, "target");
    let mut stray: Vec<&str> = form
        .iter()
        .filter_map(|(k, _)| k.strip_prefix("ch:"))
        .filter(|g| *g != target)
        .collect();
    stray.dedup();
    let Some((dir, device)) = target.split_once(':') else {
        return Err(match stray.first() {
            Some(g) => format!(
                "Channels are ticked under {}, but no group is chosen: choose its RX or TX button too.",
                group_words(g, devices)
            ),
            None => "Choose the device and direction of the channels.".into(),
        });
    };
    if let Some(g) = stray.first() {
        return Err(format!(
            "Channels are ticked under {}, but the chosen group is {}: only its ticks count. Untick the others, or choose their group.",
            group_words(g, devices),
            group_words(target, devices)
        ));
    }
    let direction = super::parse_dir(dir).ok_or("Choose RX or TX.")?;
    let output = field(form, "stage") == "output";
    if output && direction != Direction::Rx {
        return Err(
            "The output stage is on RX channels (what this host plays): choose an RX group, or the input level."
                .into(),
        );
    }
    let key = format!("ch:{target}");
    let mut channels: Vec<Channel> = form
        .iter()
        .filter(|(k, _)| *k == key)
        .filter_map(|(_, v)| v.trim().parse::<u16>().ok().and_then(Channel::new))
        .collect();
    channels.sort_unstable();
    channels.dedup();
    if channels.is_empty() {
        return Err(format!(
            "Choose at least one channel of {}.",
            group_words(target, devices)
        ));
    }
    let action = match (c.kind, output) {
        (ControlKind::Button, false) => BindingAction::Mute,
        (ControlKind::Button, true) => BindingAction::OutputMute,
        (_, false) => BindingAction::Gain,
        (_, true) => BindingAction::OutputGain,
    };
    Ok((
        action,
        ChannelSet {
            device: DeviceKey(device.to_owned()),
            direction,
            channels,
        },
    ))
}

/// The binding a form describes, on top of `base` (the binding being
/// edited, or the defaults). What it drives comes from `what` (channels
/// when absent): channels, the speaker volume, router ports, router
/// crosspoints or a control of another surface. Boxes ticked under a
/// channel group that is not the chosen one are refused, not dropped: the
/// operator sees why. `law`: the fader law a fader's level mapping takes
/// (its profile's).
fn binding_of(
    form: &[(String, String)],
    c: &SurfaceControl,
    mut b: SurfaceBinding,
    law: Option<&[CurvePoint]>,
    devices: &[DeviceStatus],
) -> Result<SurfaceBinding, String> {
    let button = c.kind == ControlKind::Button;
    b.control.clone_from(&c.id);
    b.target = ChannelSet::default();
    b.ports.clear();
    b.crosspoints.clear();
    b.follow = None;
    match What::of(field(form, "what")) {
        What::Channels => {
            let (action, target) = channel_target(form, c, devices)?;
            b.action = action;
            b.target = target;
        }
        What::Host => {
            b.action = if button {
                BindingAction::HostMute
            } else {
                BindingAction::HostVolume
            };
        }
        What::Ports => {
            b.action = if button {
                BindingAction::PortMute
            } else {
                BindingAction::PortGain
            };
            for v in form.iter().filter(|(k, _)| k == "port").map(|(_, v)| v) {
                b.ports.push(
                    crate::ports::decode(v)
                        .ok_or("A router port is not one any more; reload the page.")?,
                );
            }
            if b.ports.is_empty() {
                return Err("Tick at least one router port.".into());
            }
        }
        What::Crosspoints => {
            b.action = if button {
                BindingAction::CrosspointMute
            } else {
                BindingAction::CrosspointGain
            };
            for v in form.iter().filter(|(k, _)| k == "xp").map(|(_, v)| v) {
                b.crosspoints.push(
                    crate::ports::decode_xp(v)
                        .ok_or("A crosspoint is not one any more; reload the page.")?,
                );
            }
            if b.crosspoints.is_empty() {
                return Err("Tick at least one patched crosspoint.".into());
            }
        }
        What::Follow => {
            let Some((surface, control)) = field(form, "follow").split_once('/') else {
                return Err("Choose the control of another surface to drive.".into());
            };
            if !ident_ok(surface) || !ident_ok(control) {
                return Err("Choose the control of another surface to drive.".into());
            }
            b.action = BindingAction::Follow;
            b.follow = Some(tessera_control_types::ControlRef::new(
                surface.to_owned(),
                control.to_owned(),
            ));
        }
    }
    match c.kind {
        ControlKind::Button => b.mode = mode_of(field(form, "mode")),
        ControlKind::Fader => {
            b.min_mdb = db_field(form, "min_db", "Bottom")?;
            b.max_mdb = db_field(form, "max_db", "Top")?;
            if b.min_mdb >= b.max_mdb {
                return Err("The bottom of the fader range must be below its top.".into());
            }
            b.taper = if field(form, "taper") == "linear" {
                Taper::Linear
            } else {
                Taper::Db
            };
            if let Some(l) = law {
                l.clone_into(&mut b.curve);
            }
        }
        ControlKind::Encoder => {
            b.step_mdb = db_field(form, "step_db", "dB per step")?;
            if b.step_mdb <= 0 {
                return Err("dB per step: more than 0.".into());
            }
        }
        _ => return Err("This portal cannot map that kind of control.".into()),
    }
    Ok(b)
}

/// `POST /surfaces/{name}/bindings`: add a mapping of a control, or replace
/// the one at `index`. Channel checkboxes are named `ch:<dir>:<device>`, so
/// one form offers every device and direction without the script; ticks
/// outside the chosen `target` are refused with the reason.
pub async fn save_binding(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, field(&form, "csrf")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let control = field(&form, "control").to_owned();
    let index = field(&form, "index").trim().to_owned();
    let to = control_anchor(&name, &control);
    let extra = known_profiles(&daemon_profiles(&st).await, imported(&st, &name).as_ref());
    // For the message: the channels by their labels.
    let status = status_of(&st).await.ok();
    let r = edit_surface(&st, &s, &name, |spec| {
        let Some(c) = spec.controls.iter().find(|c| c.id == control).cloned() else {
            return Err(format!(
                "There is no control `{control}` any more; reload the page."
            ));
        };
        let at = if index.is_empty() {
            None
        } else {
            let i: usize = index.parse().map_err(|_| "Bad mapping index.")?;
            if spec.bindings.get(i).is_none_or(|b| b.control != control) {
                return Err("The surface changed meanwhile; reload the page.".into());
            }
            Some(i)
        };
        let base = at.map_or_else(
            || SurfaceBinding::new(control.clone(), action_of(c.kind), empty_set()),
            |i| spec.bindings[i].clone(),
        );
        let law = law_for(spec, &c, &extra);
        let b = binding_of(&form, &c, base, law.as_deref(), devices_of(status.as_ref()))?;
        let words = mapping_words(&b, &c, status.as_ref());
        match at {
            Some(i) => spec.bindings[i] = b,
            None => spec.bindings.push(b),
        }
        Ok(format!("Mapping saved: {words}"))
    })
    .await;
    back(&st, &token, &to, r.map_err(|(_, m)| m))
}

/// `POST /surfaces/{name}/bindings/remove`.
#[derive(Debug, Deserialize)]
pub struct RemoveBindingForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    control: String,
    #[serde(default)]
    index: String,
}

/// `POST /surfaces/{name}/bindings/remove`: remove one mapping.
pub async fn remove_binding(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(f): Form<RemoveBindingForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let to = control_anchor(&name, &f.control);
    let r = edit_surface(&st, &s, &name, |spec| {
        let i: usize = f.index.trim().parse().map_err(|_| "Bad mapping index.")?;
        if spec.bindings.get(i).is_none_or(|b| b.control != f.control) {
            return Err("The surface changed meanwhile; reload the page.".into());
        }
        spec.bindings.remove(i);
        Ok("Mapping removed.".into())
    })
    .await;
    back(&st, &token, &to, r.map_err(|(_, m)| m))
}

// ---- profiles ----------------------------------------------------------------

/// The profile imported for surface `name` in this portal run, if any.
fn imported(st: &Shared, name: &str) -> Option<Profile> {
    st.imported_profiles.lock().ok()?.get(name).cloned()
}

/// `POST /surfaces/{name}/profile`.
#[derive(Debug, Deserialize)]
pub struct ProfileForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    profile: String,
}

/// `POST /surfaces/{name}/profile`: apply a known profile (its controls,
/// and its fader law to mapped faders) in one `SetSurface`.
pub async fn apply_profile(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(f): Form<ProfileForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let extra = imported(&st, &name);
    let Some(p) = known_profiles(&daemon_profiles(&st).await, extra.as_ref())
        .into_iter()
        .find(|p| p.name == f.profile)
    else {
        return back(
            &st,
            &token,
            &surface_anchor(&name),
            Err(format!("There is no profile `{}`.", f.profile)),
        );
    };
    let r = edit_surface(&st, &s, &name, |spec| p.apply(spec)).await;
    back(&st, &token, &surface_anchor(&name), r.map_err(|(_, m)| m))
}

/// `GET /surfaces/{name}/profile.toml`: the surface as a profile file.
pub async fn export_profile(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let (token, _s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let report = match report_of(&st).await {
        Ok(r) => r,
        Err(e) => return back(&st, &token, "/surfaces", Err(e)),
    };
    let Some(sf) = report.surfaces.iter().find(|x| x.spec.name == name) else {
        return back(
            &st,
            &token,
            "/surfaces",
            Err(format!("There is no surface `{name}`.")),
        );
    };
    let mut p = profiles::of_surface(&sf.spec);
    // What it was made from, when that is known.
    if let Some(known) = imported(&st, &name)
        .into_iter()
        .chain(daemon_profiles(&st).await)
        .find(|k| k.fits(&sf.spec.source, None))
    {
        p.vendor = known.vendor;
        p.model = known.model;
    }
    let text = match p.to_toml_string() {
        Ok(t) => t,
        Err(e) => return back(&st, &token, &surface_anchor(&name), Err(e.to_string())),
    };
    let body = format!(
        "# Surface profile exported from the tessera portal (surface `{name}`).\n\
         # Format: docs/surface-profiles.md. Rename `name` before sharing it.\n{text}"
    );
    let file: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    (
        [
            (
                header::CONTENT_TYPE,
                "application/toml; charset=utf-8".to_owned(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{file}.toml\""),
            ),
        ],
        body,
    )
        .into_response()
}

/// A multipart field as text, read a chunk at a time and given up past
/// `max` bytes.
async fn read_capped(
    mut field: axum::extract::multipart::Field<'_>,
    max: usize,
) -> Result<String, String> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match field.chunk().await {
            Ok(Some(c)) => {
                if buf.len() + c.len() > max {
                    return Err(format!(
                        "The file is larger than {} KiB: not a profile.",
                        max / 1024
                    ));
                }
                buf.extend_from_slice(&c);
            }
            Ok(None) => break,
            Err(e) => return Err(format!("The upload failed: {e}")),
        }
    }
    String::from_utf8(buf).map_err(|_| "The file is not text (UTF-8): not a profile.".into())
}

/// The largest body `POST /surfaces/{name}/profile/import` takes: a
/// profile, its token and the multipart framing.
pub const IMPORT_BODY_LIMIT: usize = profiles::MAX_PROFILE_BYTES + 16 * 1024;

/// `POST /surfaces/{name}/profile/import` (multipart: `csrf` first, then
/// `file`; the session and the token are checked before the file is read):
/// read an uploaded TOML or JSON profile and apply it. Nothing is written
/// on the host: the controls are saved with the surface, and the profile
/// is kept in memory so faders mapped later take its fader law.
pub async fn import_profile(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
    mut parts: Multipart,
) -> Response {
    // The session first: without one, nothing of the body is read.
    if let Err(r) = need(&st, &headers) {
        return r;
    }
    // The form sends its token before the file: check it before reading
    // the file (a body whose first field is not the token is refused).
    let csrf = match parts.next_field().await {
        Ok(Some(field)) if field.name() == Some("csrf") => {
            read_capped(field, 256).await.unwrap_or_default()
        }
        _ => String::new(),
    };
    let (token, s) = match need_post(&st, &headers, &csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let mut file: Option<Result<String, String>> = None;
    loop {
        match parts.next_field().await {
            Ok(Some(field)) if field.name() == Some("file") && file.is_none() => {
                file = Some(
                    read_capped(field, profiles::MAX_PROFILE_BYTES)
                        .await
                        .and_then(|t| {
                            if t.is_empty() {
                                Err("Choose a profile file.".into())
                            } else {
                                Ok(t)
                            }
                        }),
                );
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                file.get_or_insert(Err(format!("The upload failed: {e}")));
                break;
            }
        }
    }
    let to = surface_anchor(&name);
    let parsed = file
        .unwrap_or_else(|| Err("Choose a profile file.".into()))
        .and_then(|text| profiles::parse(&text));
    let p = match parsed {
        Ok(p) => p,
        Err(e) => return back(&st, &token, &to, Err(format!("Profile not imported: {e}"))),
    };
    let r = edit_surface(&st, &s, &name, |spec| p.apply(spec)).await;
    if r.is_ok() {
        if let Ok(mut m) = st.imported_profiles.lock() {
            m.insert(name.clone(), p);
        }
    }
    back(&st, &token, &to, r.map_err(|(_, m)| m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_slugged_and_made_unique() {
        assert_eq!(slug("fifine SC3"), "fifine-sc3");
        assert_eq!(slug("  MV-SILICON  fifine  "), "mv-silicon-fifine");
        assert_eq!(slug("ÄÖÜ"), "surface");
        assert_eq!(unique("sc3", &["sc3", "sc3-2"]), "sc3-3");
        assert_eq!(unique("sc3", &[]), "sc3");
        assert!(ident_ok("mic_fader-1"));
        assert!(!ident_ok("mic fader"));
        assert!(!ident_ok(""));
    }

    #[test]
    fn a_binding_form_takes_the_chosen_groups_channels_and_refuses_strays() {
        let c = SurfaceControl {
            id: "f".into(),
            label: None,
            kind: ControlKind::Fader,
            report_id: None,
            offset: Some(0),
            bits: None,
            max: None,
            inverted: false,
            active_low: false,
            motorized: false,
            code: None,
            midi: None,
        };
        let form: Vec<(String, String)> = [
            ("target", "tx:main"),
            ("ch:tx:main", "2"),
            ("ch:tx:main", "1"),
            ("min_db", "\u{2212}50"),
            ("max_db", "6"),
            ("taper", "linear"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
        let base = SurfaceBinding::new(
            "x",
            BindingAction::Mute,
            ChannelSet {
                device: DeviceKey(String::new()),
                direction: Direction::Rx,
                channels: Vec::new(),
            },
        );
        let b = binding_of(&form, &c, base.clone(), None, &[]).unwrap();
        // A box ticked under the group not chosen is refused, never dropped.
        let mut strays = form.clone();
        strays.push(("ch:rx:main".into(), "1".into()));
        let e = binding_of(&strays, &c, base.clone(), None, &[]).unwrap_err();
        // One device: the groups are named as the form's legends name
        // them, never by the device's configuration key.
        assert!(
            e.contains("ticked under RX,")
                && e.contains("chosen group is TX:")
                && !e.contains("main"),
            "{e}"
        );
        // Several: by the device's name.
        let device = |key: &str, name: &str| {
            DeviceStatus::new(
                DeviceKey(key.into()),
                name,
                tessera_control_types::SampleRate(48_000),
                tessera_control_types::Encoding(24),
                tessera_control_types::LatencyUs(1_000),
            )
        };
        let two = [device("main", "Speaker"), device("aux", "Studio")];
        let e = binding_of(&strays, &c, base.clone(), None, &two).unwrap_err();
        assert!(
            e.contains("Speaker \u{b7} RX")
                && e.contains("Speaker \u{b7} TX")
                && !e.contains("main"),
            "{e}"
        );
        strays.retain(|(k, _)| k != "target");
        let e = binding_of(&strays, &c, base.clone(), None, &[]).unwrap_err();
        assert!(e.contains("no group is chosen"), "{e}");
        assert_eq!(b.action, BindingAction::Gain);
        assert_eq!(b.target.direction, Direction::Tx);
        assert_eq!(
            b.target.channels,
            vec![Channel::new(1).unwrap(), Channel::new(2).unwrap()]
        );
        assert_eq!((b.min_mdb, b.max_mdb), (-50_000, 6_000));
        assert_eq!(b.taper, Taper::Linear);
        let mut bad = form.clone();
        bad.retain(|(k, _)| k != "ch:tx:main");
        assert!(binding_of(&bad, &c, base.clone(), None, &[]).is_err());
        let mut upside = form;
        upside.retain(|(k, _)| k != "max_db");
        upside.push(("max_db".into(), "-60".into()));
        assert!(binding_of(&upside, &c, base, None, &[]).is_err());
    }

    #[test]
    fn a_control_id_comes_from_its_label_or_its_kind() {
        assert_eq!(
            derive_id("Mic mute", ControlKind::Button, &["mic"]),
            "mic-mute"
        );
        assert_eq!(derive_id("Mic", ControlKind::Fader, &["mic"]), "mic-2");
        assert_eq!(derive_id("  ", ControlKind::Fader, &["fader-1"]), "fader-2");
        let f = ControlForm::of(&[
            ("id".to_owned(), String::new()),
            ("label".to_owned(), "Line out".to_owned()),
            ("kind".to_owned(), "fader".to_owned()),
            ("offset".to_owned(), "0".to_owned()),
            ("active_low".to_owned(), "true".to_owned()),
            ("inverted".to_owned(), "true".to_owned()),
        ]);
        let c = f.control(&[]).unwrap();
        assert_eq!(c.id, "line-out");
        // Options that do not fit the kind are not saved.
        assert!(c.inverted && !c.active_low);
    }

    #[test]
    fn a_fader_position_reads_as_the_gain_its_mapping_sets() {
        let spec = SurfaceSpec::new(
            "s",
            SurfaceSource::Hidraw {
                vendor_id: 1,
                product_id: 2,
                interface: None,
            },
            Vec::new(),
            vec![SurfaceBinding::new("f", BindingAction::Gain, empty_set())],
        );
        let cfg = SurfaceConfig::from(&spec);
        let b = &cfg.bindings[0];
        assert_eq!(fader_words(b, 0, 4095), "off");
        assert_eq!(fader_words(b, 4095, 4095), "0.0 dB");
        assert_eq!(fader_words(b, 2048, 4095), "\u{2212}30.0 dB");
        assert_eq!(percent(3575, 4095), 87);
        assert_eq!(percent(9, 0), 100);
    }
}
