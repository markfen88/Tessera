//! The panel editor (`/mixer/edit`): create, rename and delete panels; add
//! fader and button strips (an action and its target: channels, router
//! ports or crosspoints chosen by label, or the host volume); label,
//! group, master, reorder (up and down, no dragging) and delete strips;
//! link a strip to a surface control (choose one, or learn one) and unlink
//! it.
//!
//! Every change is one `SetMixer` (or `RemoveMixer`) with the whole panel,
//! built from the daemon's current one. Linking is a surface binding: the
//! editor sends `SetSurface` (the binding, and the control when learned)
//! and then `SetMixer`; a link is never stored in the panel, it is derived
//! (`StripControl::links_to`). Unlinking removes those bindings
//! (`SetSurface`). See `docs/plans/2026-11-22-mixer-panel.md` §2 and §4.

use axum::Form;
use axum::extract::{Query as UrlQuery, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use serde::Deserialize;
use tessera_control_types::{
    BindingAction, ButtonMode, ChannelSet, Command, ControlKind, CrosspointKey, Direction,
    FaderRange, HostVolumeStatus, MixerSpec, MixerStrip, RequestBody, RouterPort, RouterReport,
    Status, StripControl, SurfaceBinding, SurfacesReport,
};

use super::model::{self, MainTarget, Target};
use super::{main_target, router, saved, surfaces};
use crate::auth::Session;
use crate::pages::surfaces::{
    Question, ask, control_anchor, learn_control, learned_control_fields,
};
use crate::pages::{
    Chrome, back, error_box, layout, need, need_post, path_segment, refusal, status_of, view,
};
use crate::{Shared, call, ports};

/// The editor's query.
#[derive(Debug, Deserialize, Default)]
pub struct EditQuery {
    #[serde(default)]
    panel: String,
    #[serde(default)]
    new: String,
}

/// What the editor shows and changes.
struct Ctx {
    status: Status,
    router: Option<RouterReport>,
    surfaces: Option<SurfacesReport>,
    saved: Vec<MixerSpec>,
    main: MainTarget,
}

async fn ctx(st: &Shared) -> Result<Ctx, String> {
    let status = status_of(st).await?;
    let main = main_target(st, Some(&status)).await;
    Ok(Ctx {
        router: router(st).await,
        surfaces: surfaces(st).await,
        saved: saved(st).await,
        status,
        main,
    })
}

fn edit_path(panel: &str) -> String {
    format!("/mixer/edit?panel={}", path_segment(panel))
}

/// The actions a strip can take: (form value, words, fader?).
const ACTIONS: [(BindingAction, &str, &str, bool); 10] = [
    (BindingAction::Gain, "gain", "Input level (channels)", true),
    (
        BindingAction::OutputGain,
        "output_gain",
        "Output level (RX channels, after the mix)",
        true,
    ),
    (
        BindingAction::HostVolume,
        "host_volume",
        "Speaker volume",
        true,
    ),
    (
        BindingAction::PortGain,
        "port_gain",
        "Router port level",
        true,
    ),
    (
        BindingAction::CrosspointGain,
        "crosspoint_gain",
        "Crosspoint gain",
        true,
    ),
    (BindingAction::Mute, "mute", "Mute (channels)", false),
    (
        BindingAction::OutputMute,
        "output_mute",
        "Output mute (RX channels)",
        false,
    ),
    (BindingAction::HostMute, "host_mute", "Speaker mute", false),
    (
        BindingAction::PortMute,
        "port_mute",
        "Router port mute",
        false,
    ),
    (
        BindingAction::CrosspointMute,
        "crosspoint_mute",
        "Crosspoint mute",
        false,
    ),
];

fn action_of(v: &str) -> Option<(BindingAction, bool)> {
    ACTIONS.iter().find(|a| a.1 == v).map(|a| (a.0, a.3))
}

/// What a strip does, in words.
#[must_use]
pub fn action_words(a: BindingAction) -> &'static str {
    ACTIONS.iter().find(|x| x.0 == a).map_or("other", |x| x.2)
}

/// The target of a strip control, in words.
fn target_words(c: &StripControl, cx: &Ctx) -> String {
    match c.action() {
        BindingAction::HostVolume | BindingAction::HostMute => "the speaker".into(),
        BindingAction::PortGain | BindingAction::PortMute => c
            .ports()
            .iter()
            .map(|p| ports::label(cx.router.as_ref(), p))
            .collect::<Vec<_>>()
            .join(", "),
        BindingAction::CrosspointGain | BindingAction::CrosspointMute => c
            .crosspoints()
            .iter()
            .map(|k| ports::xp_words(cx.router.as_ref(), k))
            .collect::<Vec<_>>()
            .join(", "),
        _ => c.target().map_or_else(String::new, |t| {
            let words = Target::of(t).words();
            let labels = channel_labels(t, cx);
            let words = if labels.is_empty() {
                words
            } else {
                format!("{labels} ({words})")
            };
            // The device's name, not its key, and only when there are several.
            match cx.status.devices.iter().find(|d| d.key == t.device) {
                Some(d) if cx.status.devices.len() > 1 => format!("{}: {words}", d.name),
                _ => words,
            }
        }),
    }
}

/// The labels of a set's channels, as the device reports them.
fn channel_labels(t: &ChannelSet, cx: &Ctx) -> String {
    let Some(d) = cx.status.devices.iter().find(|d| d.key == t.device) else {
        return String::new();
    };
    let list = match t.direction {
        Direction::Rx => &d.rx_channels,
        Direction::Tx => &d.tx_channels,
    };
    t.channels
        .iter()
        .filter_map(|n| list.iter().find(|c| c.channel == *n))
        .map(|c| c.label.as_str())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// A strip's name in the editor's list: its label; without one, what the
/// mixer shows on its scribble strip (the channels' labels, the ports').
fn strip_name(strip: &MixerStrip, cx: &Ctx) -> String {
    if !strip.label.is_empty() {
        return strip.label.clone();
    }
    let c = &strip.control;
    match c.action() {
        BindingAction::HostVolume | BindingAction::HostMute => "Main".into(),
        BindingAction::PortGain | BindingAction::PortMute => c
            .ports()
            .iter()
            .map(|p| ports::short(cx.router.as_ref(), p))
            .collect::<Vec<_>>()
            .join(" / "),
        BindingAction::CrosspointGain | BindingAction::CrosspointMute => target_words(c, cx),
        _ => {
            let labels = c
                .target()
                .map(|t| channel_labels(t, cx))
                .unwrap_or_default();
            if labels.is_empty() {
                target_words(c, cx)
            } else {
                labels
            }
        }
    }
}

/// A binding that drives `c` from surface control `control`: the same
/// action and target; a fader's law is the strip's range, the host's range
/// for the host volume, else the binding's default (−60…0 dB).
#[must_use]
pub fn binding_for(
    c: &StripControl,
    control: &str,
    host: Option<&HostVolumeStatus>,
) -> SurfaceBinding {
    let a = c.action();
    let mut b = match a {
        BindingAction::HostVolume | BindingAction::HostMute => SurfaceBinding::host(control, a),
        BindingAction::PortGain | BindingAction::PortMute => {
            SurfaceBinding::on_ports(control, a, c.ports().to_vec())
        }
        BindingAction::CrosspointGain | BindingAction::CrosspointMute => {
            SurfaceBinding::on_crosspoints(control, a, c.crosspoints().to_vec())
        }
        _ => SurfaceBinding::new(control, a, c.target().cloned().unwrap_or_default()),
    };
    match c {
        StripControl::Fader { range: Some(r), .. } => {
            b.min_mdb = r.min_mdb;
            b.max_mdb = r.max_mdb;
            b.taper = r.taper;
        }
        StripControl::Fader { .. } if a == BindingAction::HostVolume => {
            if let Some(h) = host {
                b.min_mdb = h.min_mdb;
                b.max_mdb = h.max_mdb;
            }
        }
        StripControl::Button { mode, .. } => b.mode = *mode,
        _ => {}
    }
    b
}

/// Is a surface control of kind `k` one a strip control can be linked to?
const fn kind_fits(c: &StripControl, k: ControlKind) -> bool {
    match c {
        StripControl::Fader { .. } => matches!(k, ControlKind::Fader | ControlKind::Encoder),
        _ => matches!(k, ControlKind::Button),
    }
}

// ---- the page ------------------------------------------------------------------

/// `GET /mixer/edit`.
pub async fn show(
    State(st): State<Shared>,
    headers: HeaderMap,
    UrlQuery(q): UrlQuery<EditQuery>,
) -> Response {
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let body = match ctx(&st).await {
        Err(e) => error_box(&e),
        Ok(cx) => {
            if q.new == "1" {
                new_panel(&s.csrf)
            } else if let Some(m) = cx
                .saved
                .iter()
                .find(|m| m.id == q.panel)
                .or_else(|| q.panel.is_empty().then(|| cx.saved.first()).flatten())
            {
                editor(&cx, m, &s.csrf)
            } else if cx.saved.is_empty() {
                start(&s.csrf)
            } else {
                html! { div.grid { section.card { h2 { "Mixer panels" } (error_box("There is no such panel.")) (panel_list(&cx)) } } }
            }
        }
    };
    crate::pages::page(
        &st,
        &token,
        &s,
        &headers,
        view("Edit mixer", "/mixer").scripted(),
        &body,
    )
}

fn panel_list(cx: &Ctx) -> Markup {
    html! {
        ul.panel-list {
            @for m in &cx.saved { li { a href=(edit_path(&m.id)) { (m.title) } " " span.muted { code { (m.id) } " \u{b7} " (m.strips.len()) " strip(s)" } } }
        }
        p { a.btn.sm href="/mixer/edit?new=1" { "New panel" } " " a href="/mixer" { "Back to the mixer" } }
    }
}

fn start(csrf: &str) -> Markup {
    html! {
        div.grid { section.card {
            h2 { "Edit the mixer" }
            p { "No panel is saved: the mixer shows its default panel, made from the channels in use and Main. Save it to change it, or start an empty one." }
            div.actions {
                form.inline method="post" action="/mixer/edit" {
                    input type="hidden" name="csrf" value=(csrf);
                    input type="hidden" name="op" value="save-default";
                    button.btn.primary type="submit" { "Save the default panel and edit it" }
                }
                form.inline method="post" action="/mixer/edit" {
                    input type="hidden" name="csrf" value=(csrf);
                    input type="hidden" name="op" value="create";
                    input type="hidden" name="title" value="Mixer";
                    button.btn type="submit" { "Start an empty panel" }
                }
            }
            p { a href="/mixer" { "Back to the mixer" } }
        } }
    }
}

fn new_panel(csrf: &str) -> Markup {
    html! {
        div.grid { section.card {
            h2 { "New mixer panel" }
            form.fields method="post" action="/mixer/edit" {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="op" value="create";
                label { "Title" input type="text" name="title" required maxlength="63" size="24" placeholder="for example Stage"; }
                button.btn.primary type="submit" { "Create panel" }
            }
            p { a href="/mixer" { "Back to the mixer" } }
        } }
    }
}

fn editor(cx: &Ctx, m: &MixerSpec, csrf: &str) -> Markup {
    let hidden = |op: &str| {
        html! {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="panel" value=(m.id);
            input type="hidden" name="op" value=(op);
        }
    };
    html! {
        div.grid.mixer-edit {
            section.card id="panel" {
                h2 { "Panel " bdi { (m.title) } span.n { code { (m.id) } } }
                form.inline method="post" action="/mixer/edit" {
                    (hidden("rename"))
                    label for="panel-title" { "Title" }
                    input id="panel-title" type="text" name="title" value=(m.title) required maxlength="63" size="24";
                    button.btn.sm type="submit" { "Rename" }
                }
                p.actions {
                    a.btn.sm href=(format!("/mixer?panel={}", path_segment(&m.id))) { "Open this panel" }
                    " "
                    form.inline method="post" action="/mixer/edit" {
                        (hidden("delete-panel"))
                        button.btn.sm.danger type="submit" { "Delete panel\u{2026}" }
                    }
                }
                @if cx.saved.len() > 1 {
                    h3 { "Other panels" }
                    (panel_list(cx))
                } @else {
                    p { a.btn.sm href="/mixer/edit?new=1" { "New panel" } }
                }
            }
            section.card id="strips" {
                h2 { "Strips" span.n { "in order" } }
                @if m.strips.is_empty() { p.muted { "No strips yet: add a fader or a button below." } }
                ol.strip-list {
                    @for (i, strip) in m.strips.iter().enumerate() { (strip_item(cx, m, i, strip, csrf)) }
                }
                p.muted { "A button right after a fader on the same target, with no label or group of its own, is drawn under that fader, like a channel strip." }
            }
            section.card id="add" {
                h2 { "Add a strip" }
                (strip_form(cx, m, csrf))
            }
        }
    }
}

/// One strip of the list: what it does, its links, and its actions.
fn strip_item(cx: &Ctx, m: &MixerSpec, i: usize, strip: &MixerStrip, csrf: &str) -> Markup {
    let c = &strip.control;
    let hidden = |op: &str| {
        html! {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="panel" value=(m.id);
            input type="hidden" name="strip" value=(strip.id);
            input type="hidden" name="op" value=(op);
        }
    };
    let links = model::links_for(cx.surfaces.as_ref(), c);
    let fader = matches!(c, StripControl::Fader { .. });
    // Controls of the right kind on enabled surfaces, not linked yet.
    let candidates: Vec<(String, String, String)> = cx
        .surfaces
        .iter()
        .flat_map(|r| r.surfaces.iter())
        .filter(|s| s.spec.enabled)
        .flat_map(|s| {
            s.spec
                .controls
                .iter()
                .filter(|k| kind_fits(c, k.kind))
                .map(move |k| {
                    (
                        s.spec.name.clone(),
                        k.id.clone(),
                        k.label.clone().unwrap_or_else(|| k.id.clone()),
                    )
                })
        })
        .filter(|(s, k, _)| !links.iter().any(|l| &l.surface == s && &l.control == k))
        .collect();
    let learnable: Vec<&str> = cx
        .surfaces
        .iter()
        .flat_map(|r| r.surfaces.iter())
        .filter(|s| s.spec.enabled && s.connected)
        .map(|s| s.spec.name.as_str())
        .collect();
    let id = format!("edit-{}", strip.id);
    html! {
        li.strip-item id=(id) {
            div.control-head {
                div.control-name {
                    strong { bdi { (strip_name(strip, cx)) } }
                    span.tag { (if fader { "fader" } else { "button" }) }
                    @if strip.master { span.tag { "master" } }
                    @if let Some(g) = &strip.group { " " span.muted { "starts group " bdi { (g) } } }
                }
                span.muted { (action_words(c.action())) ": " (target_words(c, cx)) }
            }
            ul.links aria-label="Linked surface controls" {
                @for l in &links {
                    li {
                        a href=(control_anchor(&l.surface, &l.control)) { bdi { (l.surface) } " \u{b7} " bdi { (l.label) } }
                        @if l.motorized { " \u{b7} motor" }
                        @if !l.connected { " (not connected)" }
                        " "
                        form.inline method="post" action="/mixer/edit" {
                            (hidden("unlink"))
                            input type="hidden" name="surface" value=(l.surface);
                            input type="hidden" name="control" value=(l.control);
                            button.btn.sm type="submit" aria-label=(format!("Unlink {} from {}", l.label, l.surface)) { "Unlink" }
                        }
                    }
                }
                @if links.is_empty() { li.muted { "Not linked to a surface control." } }
            }
            div.surface-actions {
                form.inline method="post" action="/mixer/edit" {
                    (hidden("up"))
                    button.btn.sm type="submit" disabled[i == 0] aria-label=(format!("Move strip {} up", i + 1)) { "Up" }
                }
                form.inline method="post" action="/mixer/edit" {
                    (hidden("down"))
                    button.btn.sm type="submit" disabled[i + 1 == m.strips.len()] aria-label=(format!("Move strip {} down", i + 1)) { "Down" }
                }
                form.inline method="post" action="/mixer/edit" {
                    (hidden("delete-strip"))
                    button.btn.sm.danger type="submit" aria-label=(format!("Delete strip {}", i + 1)) { "Delete\u{2026}" }
                }
            }
            details.edit {
                summary { "Label, group, master" span.sr { " of strip " (i + 1) } }
                form.fields method="post" action="/mixer/edit" {
                    (hidden("edit-strip"))
                    label { "Label" input type="text" name="label" value=(strip.label) maxlength="31" size="16" placeholder="the target's labels"; }
                    label { "Starts a group" input type="text" name="group" value=(strip.group.clone().unwrap_or_default()) maxlength="31" size="16" placeholder="no new group"; }
                    label.check { input type="checkbox" name="master" value="true" checked[strip.master]; span { "Master: drawn apart, like Main" } }
                    @if let StripControl::Button { mode, .. } = c { (mode_field(*mode)) }
                    button.btn.primary type="submit" { "Save strip" }
                }
            }
            details.edit {
                summary { "Link a surface control" span.sr { " to strip " (i + 1) } }
                @if candidates.is_empty() {
                    p.muted { "No " (if fader { "fader" } else { "button" }) " of a surface is free. Add a surface and its controls on the " a href="/surfaces" { "Surfaces page" } ", or learn one here." }
                } @else {
                    form.inline method="post" action="/mixer/edit" {
                        (hidden("link"))
                        label.sr for=(format!("{id}-link")) { "Surface control" }
                        select id=(format!("{id}-link")) name="link" {
                            @for (s, k, l) in &candidates { option value=(format!("{s}/{k}")) { (s) " \u{b7} " (l) } }
                        }
                        button.btn.sm.primary type="submit" { "Link" }
                    }
                }
                @if !learnable.is_empty() {
                    form.inline method="post" action="/mixer/edit" {
                        (hidden("learn"))
                        label.sr for=(format!("{id}-learn")) { "Surface to learn a control on" }
                        select id=(format!("{id}-learn")) name="surface" {
                            @for s in &learnable { option value=(s) { (s) } }
                        }
                        button.btn.sm type="submit" { (if fader { "Learn a fader" } else { "Learn a button" }) }
                    }
                    p.muted { "Learn: hands off for a second after the click, then " (if fader { "move the fader from the bottom all the way up" } else { "press the button once" }) " (within 15 seconds)." }
                }
                p.muted {
                    @if fader && c.action().owns_level() {
                        "A non-motorized fader then owns this level while its surface is connected: here it is shown read-only. A motorized fader follows this strip. Mute is never owned."
                    } @else if fader {
                        "The surface fader and this strip then set the same level."
                    } @else {
                        "The surface button and this strip then mute the same target."
                    }
                }
            }
        }
    }
}

fn mode_field(mode: ButtonMode) -> Markup {
    html! {
        fieldset {
            legend { "When pressed" }
            @for (m, value, words) in [
                (ButtonMode::Toggle, "toggle", "Toggle: each press mutes or unmutes"),
                (ButtonMode::Momentary, "momentary", "Hold to mute: muted while held"),
                (ButtonMode::PushToTalk, "push_to_talk", "Push to talk: live while held"),
            ] {
                label.check { input type="radio" name="mode" value=(value) checked[mode == m]; span { (words) } }
            }
        }
    }
}

fn mode_of(v: &str) -> ButtonMode {
    match v {
        "momentary" => ButtonMode::Momentary,
        "push_to_talk" => ButtonMode::PushToTalk,
        _ => ButtonMode::Toggle,
    }
}

/// "Add a strip": the action, then the target that fits it (the script
/// shows only that one; without it every picker is shown and the server
/// uses the one the action needs).
fn strip_form(cx: &Ctx, m: &MixerSpec, csrf: &str) -> Markup {
    let host = matches!(cx.main, MainTarget::HostVolume(_));
    let patched: Vec<CrosspointKey> = cx
        .router
        .iter()
        .flat_map(|r| r.crosspoints.iter())
        .filter(|x| !ports::is_bridge(&x.origin))
        .map(|x| x.crosspoint.key())
        .collect();
    html! {
        form.fields.strip-form method="post" action="/mixer/edit" data-strip-form {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="panel" value=(m.id);
            input type="hidden" name="op" value="add";
            label { "What it does"
                select name="action" {
                    optgroup label="Faders" {
                        @for (a, v, w, f) in ACTIONS { @if f && (host || a != BindingAction::HostVolume) { option value=(v) { (w) } } }
                    }
                    optgroup label="Buttons" {
                        @for (a, v, w, f) in ACTIONS { @if !f && (host || a != BindingAction::HostMute) { option value=(v) { (w) } } }
                    }
                }
            }
            fieldset.targets data-actions="gain output_gain mute output_mute" {
                legend { "Channels (one device and direction)" }
                @for d in &cx.status.devices {
                    @for (rx, chans) in [(true, &d.rx_channels), (false, &d.tx_channels)] {
                        @if !chans.is_empty() {
                            fieldset.target {
                                legend { @if let Some(n) = ports::device_name(&cx.status.devices, &d.key) { bdi { (n) } " \u{b7} " } (if rx { "RX \u{b7} network \u{2192} this host" } else { "TX \u{b7} this host \u{2192} network" }) }
                                div.chans {
                                    @for c in chans {
                                        @let p = if rx { RouterPort::NetworkRx { device: d.key.clone(), channel: c.channel } } else { RouterPort::NetworkTx { device: d.key.clone(), channel: c.channel } };
                                        label.check { input type="checkbox" name="ch" value=(ports::encode(&p)); span { (c.channel.get()) " \u{b7} " bdi { (c.label) } } }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            fieldset.targets data-actions="port_gain port_mute" {
                legend { "Router ports" }
                @match &cx.router {
                    None => { p.muted { "This tesserad does not report the router." } }
                    Some(r) => {
                        div.chans {
                            @for p in &r.ports {
                                label.check { input type="checkbox" name="port" value=(ports::encode(&p.port)); span { bdi { (ports::described(r, p)) } } }
                            }
                        }
                    }
                }
            }
            fieldset.targets data-actions="crosspoint_gain crosspoint_mute" {
                legend { "Crosspoints (patched on the Router page)" }
                @if patched.is_empty() { p.muted { "None patched." } }
                div.chans {
                    @for k in &patched {
                        label.check { input type="checkbox" name="xp" value=(ports::encode_xp(k)); span { (ports::xp_words(cx.router.as_ref(), k)) } }
                    }
                }
            }
            label { "Label" input type="text" name="label" maxlength="31" size="16" placeholder="the target's labels"; }
            label { "Starts a group" input type="text" name="group" maxlength="31" size="16" placeholder="no new group"; }
            label.check { input type="checkbox" name="master" value="true"; span { "Master: drawn apart, like Main" } }
            div data-actions="mute output_mute host_mute port_mute crosspoint_mute" { (mode_field(ButtonMode::Toggle)) }
            fieldset.range data-actions="gain output_gain port_gain crosspoint_gain" {
                legend { "Fader travel (empty: off to +12 dB)" }
                label { "Bottom (dB)" input type="number" name="min_db" step="0.5" min="-96" max="12" size="6"; }
                label { "Top (dB)" input type="number" name="max_db" step="0.5" min="-96" max="12" size="6"; }
            }
            button.btn.primary type="submit" { "Add strip" }
        }
    }
}

// ---- changing ------------------------------------------------------------------

fn field<'a>(form: &'a [(String, String)], name: &str) -> &'a str {
    form.iter()
        .find(|(k, _)| k == name)
        .map_or("", |(_, v)| v.as_str())
}

fn all<'a>(form: &'a [(String, String)], name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    form.iter()
        .filter(move |(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

async fn send(st: &Shared, s: &Session, cmd: Command) -> Result<(), String> {
    let kind = cmd.kind();
    let r = call(st, RequestBody::Command(cmd)).await;
    tracing::info!(session = %s.id, command = kind, ok = r.is_ok(), "page command");
    r.map(|_| ()).map_err(|e| refusal(&e).1)
}

/// The strip control an "Add a strip" form describes.
fn control_of(form: &[(String, String)]) -> Result<StripControl, String> {
    let Some((action, fader)) = action_of(field(form, "action")) else {
        return Err("Choose what the strip does.".into());
    };
    let mut target: Option<ChannelSet> = None;
    let mut ps: Vec<RouterPort> = Vec::new();
    let mut xs: Vec<CrosspointKey> = Vec::new();
    match action {
        BindingAction::HostVolume | BindingAction::HostMute => {}
        BindingAction::PortGain | BindingAction::PortMute => {
            for v in all(form, "port") {
                ps.push(
                    ports::decode(v)
                        .ok_or("A router port is not one any more; reload the page.")?,
                );
            }
            if ps.is_empty() {
                return Err("Tick at least one router port.".into());
            }
        }
        BindingAction::CrosspointGain | BindingAction::CrosspointMute => {
            for v in all(form, "xp") {
                xs.push(
                    ports::decode_xp(v)
                        .ok_or("A crosspoint is not one any more; reload the page.")?,
                );
            }
            if xs.is_empty() {
                return Err("Tick at least one crosspoint.".into());
            }
        }
        _ => {
            let mut set: Option<ChannelSet> = None;
            for v in all(form, "ch") {
                let (device, direction, channel) = match ports::decode(v) {
                    Some(RouterPort::NetworkRx { device, channel }) => {
                        (device, tessera_control_types::Direction::Rx, channel)
                    }
                    Some(RouterPort::NetworkTx { device, channel }) => {
                        (device, tessera_control_types::Direction::Tx, channel)
                    }
                    _ => return Err("A channel is not one any more; reload the page.".into()),
                };
                match &mut set {
                    None => {
                        set = Some(ChannelSet {
                            device,
                            direction,
                            channels: vec![channel],
                        })
                    }
                    Some(s) if s.device == device && s.direction == direction => {
                        if !s.channels.contains(&channel) {
                            s.channels.push(channel);
                        }
                    }
                    Some(_) => {
                        return Err(
                            "Tick channels of one device and one direction (RX or TX) only.".into(),
                        );
                    }
                }
            }
            let Some(mut set) = set else {
                return Err("Tick at least one channel.".into());
            };
            if matches!(
                action,
                BindingAction::OutputGain | BindingAction::OutputMute
            ) && set.direction != tessera_control_types::Direction::Rx
            {
                return Err(
                    "The output stage is on RX channels (what this host plays): tick RX channels."
                        .into(),
                );
            }
            set.channels.sort_unstable();
            target = Some(set);
        }
    }
    if fader {
        let db = |k: &str| -> Result<Option<i32>, String> {
            let v = field(form, k).trim();
            if v.is_empty() {
                return Ok(None);
            }
            let d: f64 = v
                .replace('\u{2212}', "-")
                .parse()
                .map_err(|_| format!("`{v}` is not a number of dB."))?;
            if !d.is_finite() || !(-96.0..=12.0).contains(&d) {
                return Err("A fader's travel is from \u{2212}96 to +12 dB.".into());
            }
            #[allow(clippy::cast_possible_truncation)]
            Ok(Some((d * 1000.0).round() as i32))
        };
        let range = match (db("min_db")?, db("max_db")?) {
            (None, None) => None,
            (Some(lo), Some(hi)) if lo < hi => Some(FaderRange::new(lo, hi)),
            (Some(_), Some(_)) => {
                return Err("The bottom of the travel must be below its top.".into());
            }
            _ => return Err("Give both the bottom and the top of the travel, or neither.".into()),
        };
        Ok(StripControl::Fader {
            action,
            target,
            ports: ps,
            crosspoints: xs,
            range,
        })
    } else {
        Ok(StripControl::Button {
            action,
            target,
            ports: ps,
            crosspoints: xs,
            mode: mode_of(field(form, "mode")),
        })
    }
}

/// A new strip id: `s1`, `s2`, … not taken.
fn new_id(m: &MixerSpec) -> String {
    (1..)
        .map(|n| format!("s{n}"))
        .find(|id| !m.strips.iter().any(|s| &s.id == id))
        .unwrap_or_else(|| "s".into())
}

/// A new panel id from its title, unique.
fn panel_id(title: &str, saved: &[MixerSpec]) -> String {
    let base = crate::pages::surfaces::slug(title);
    let base: String = base.chars().take(48).collect();
    let taken = |id: &str| saved.iter().any(|m| m.id == id);
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !taken(c))
        .unwrap_or(base)
}

fn label_of(form: &[(String, String)], name: &str) -> Result<String, String> {
    let v = field(form, name).trim();
    if v.len() > 31 {
        return Err("Labels and group headings are at most 31 characters.".into());
    }
    Ok(v.to_owned())
}

/// The surface and control a `link` value names (`surface/control`).
fn link_of(v: &str) -> Option<(String, String)> {
    let (s, c) = v.split_once('/')?;
    (crate::pages::surfaces::ident_ok(s) && crate::pages::surfaces::ident_ok(c))
        .then(|| (s.to_owned(), c.to_owned()))
}

/// Add the binding that links `control` of `surface` to strip control `c`
/// (`SetSurface`, the whole spec).
async fn link(
    st: &Shared,
    s: &Session,
    cx: &Ctx,
    surface: &str,
    control: &str,
    c: &StripControl,
) -> Result<String, String> {
    let Some(sf) = cx
        .surfaces
        .as_ref()
        .and_then(|r| r.surfaces.iter().find(|x| x.spec.name == surface))
    else {
        return Err(format!("There is no surface `{surface}`."));
    };
    let Some(k) = sf.spec.controls.iter().find(|k| k.id == control) else {
        return Err(format!("`{surface}` has no control `{control}` any more."));
    };
    if !kind_fits(c, k.kind) {
        return Err(
            "A fader strip links to a fader or encoder, a button strip to a button.".into(),
        );
    }
    let host = match &cx.main {
        MainTarget::HostVolume(h) => Some(h),
        _ => None,
    };
    let mut spec = sf.spec.clone();
    let b = binding_for(c, control, host);
    if !spec.bindings.iter().any(|x| x == &b) {
        spec.bindings.push(b);
    }
    send(st, s, Command::SetSurface { surface: spec }).await?;
    Ok(format!(
        "{} of {surface} linked.",
        k.label.clone().unwrap_or_else(|| control.to_owned())
    ))
}

/// `POST /mixer/edit`: `op` and its fields.
pub async fn post(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, field(&form, "csrf")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let cx = match ctx(&st).await {
        Ok(cx) => cx,
        Err(e) => return back(&st, &token, "/mixer/edit", Err(e)),
    };
    let op = field(&form, "op");
    let id = field(&form, "panel").to_owned();
    // Panels themselves.
    match op {
        "save-default" => {
            let spec = model::default_spec(&cx.status, &cx.main);
            let r = send(&st, &s, Command::SetMixer { mixer: spec })
                .await
                .map(|()| "The default panel is saved as `main`: change it below.".to_owned());
            let to = if r.is_ok() {
                edit_path("main")
            } else {
                "/mixer/edit".into()
            };
            return back(&st, &token, &to, r);
        }
        "create" => {
            let title = field(&form, "title").trim().to_owned();
            if title.is_empty() || title.len() > 63 {
                return back(
                    &st,
                    &token,
                    "/mixer/edit?new=1",
                    Err("A title is 1 to 63 characters.".into()),
                );
            }
            let pid = panel_id(&title, &cx.saved);
            let spec = MixerSpec::new(pid.clone(), title.clone(), Vec::new());
            let r = send(&st, &s, Command::SetMixer { mixer: spec })
                .await
                .map(|()| format!("Panel \u{201c}{title}\u{201d} created: add its strips."));
            let to = if r.is_ok() {
                edit_path(&pid)
            } else {
                "/mixer/edit?new=1".into()
            };
            return back(&st, &token, &to, r);
        }
        _ => {}
    }
    let Some(mut spec) = cx.saved.iter().find(|m| m.id == id).cloned() else {
        return back(
            &st,
            &token,
            "/mixer/edit",
            Err("There is no such panel any more.".into()),
        );
    };
    let to = edit_path(&spec.id);
    let strip_at = || {
        spec.strips
            .iter()
            .position(|x| x.id == field(&form, "strip"))
    };
    let r: Result<String, String> = match op {
        "rename" => {
            let title = field(&form, "title").trim().to_owned();
            if title.is_empty() || title.len() > 63 {
                Err("A title is 1 to 63 characters.".into())
            } else {
                spec.title.clone_from(&title);
                send(&st, &s, Command::SetMixer { mixer: spec })
                    .await
                    .map(|()| format!("Renamed to \u{201c}{title}\u{201d}."))
            }
        }
        "delete-panel" => {
            if field(&form, "confirm") != "yes" {
                let q = Question {
                    title: "Delete panel",
                    text: html! {
                        p { "Delete the mixer panel " strong { bdi { (spec.title) } } "?" }
                        p.muted { "No audio changes. Surface controls linked to its strips keep their mappings." }
                    },
                    action: "/mixer/edit".into(),
                    hidden: vec![("panel", spec.id.clone()), ("op", "delete-panel".into())],
                    back_to: to.clone(),
                    active: "/mixer",
                };
                return ask(&st, &headers, &s, &q);
            }
            let r = send(
                &st,
                &s,
                Command::RemoveMixer {
                    id: spec.id.clone(),
                },
            )
            .await
            .map(|()| "Panel deleted.".to_owned());
            let to = if r.is_ok() { "/mixer".to_owned() } else { to };
            return back(&st, &token, &to, r);
        }
        "add" => match control_of(&form)
            .and_then(|c| Ok((c, label_of(&form, "label")?, label_of(&form, "group")?)))
        {
            Err(e) => Err(e),
            Ok((control, label, group)) => {
                let mut strip = MixerStrip::new(new_id(&spec), control);
                strip.label = label;
                strip.group = (!group.is_empty()).then_some(group);
                strip.master = field(&form, "master") == "true";
                let sid = strip.id.clone();
                spec.strips.push(strip);
                send(&st, &s, Command::SetMixer { mixer: spec })
                    .await
                    .map(|()| format!("Strip `{sid}` added."))
            }
        },
        "up" | "down" => match strip_at() {
            None => Err("The panel changed meanwhile; reload the page.".into()),
            Some(i) => {
                let j = if op == "up" {
                    i.checked_sub(1)
                } else {
                    Some(i + 1).filter(|j| *j < spec.strips.len())
                };
                match j {
                    None => Ok("Already there.".into()),
                    Some(j) => {
                        spec.strips.swap(i, j);
                        send(&st, &s, Command::SetMixer { mixer: spec })
                            .await
                            .map(|()| "Strip moved.".to_owned())
                    }
                }
            }
        },
        "delete-strip" => match strip_at() {
            None => Err("The panel changed meanwhile; reload the page.".into()),
            Some(i) => {
                if field(&form, "confirm") != "yes" {
                    let words = target_words(&spec.strips[i].control, &cx);
                    let q = Question {
                        title: "Delete strip",
                        text: html! {
                            p { "Delete strip " (i + 1) " (" (action_words(spec.strips[i].control.action())) ": " (words) ")?" }
                            p.muted { "No audio changes. Surface controls linked to it keep their mappings; unlink them first if they should stop." }
                        },
                        action: "/mixer/edit".into(),
                        hidden: vec![
                            ("panel", spec.id.clone()),
                            ("strip", spec.strips[i].id.clone()),
                            ("op", "delete-strip".into()),
                        ],
                        back_to: to.clone(),
                        active: "/mixer",
                    };
                    return ask(&st, &headers, &s, &q);
                }
                spec.strips.remove(i);
                send(&st, &s, Command::SetMixer { mixer: spec })
                    .await
                    .map(|()| "Strip deleted.".to_owned())
            }
        },
        "edit-strip" => match strip_at() {
            None => Err("The panel changed meanwhile; reload the page.".into()),
            Some(i) => match (label_of(&form, "label"), label_of(&form, "group")) {
                (Err(e), _) | (_, Err(e)) => Err(e),
                (Ok(label), Ok(group)) => {
                    let strip = &mut spec.strips[i];
                    strip.label = label;
                    strip.group = (!group.is_empty()).then_some(group);
                    strip.master = field(&form, "master") == "true";
                    if let StripControl::Button { mode, .. } = &mut strip.control {
                        *mode = mode_of(field(&form, "mode"));
                    }
                    send(&st, &s, Command::SetMixer { mixer: spec })
                        .await
                        .map(|()| "Strip saved.".to_owned())
                }
            },
        },
        "link" => match (strip_at(), link_of(field(&form, "link"))) {
            (None, _) => Err("The panel changed meanwhile; reload the page.".into()),
            (_, None) => Err("Choose a surface control.".into()),
            (Some(i), Some((surface, control))) => {
                let c = spec.strips[i].control.clone();
                match link(&st, &s, &cx, &surface, &control, &c).await {
                    Err(e) => Err(e),
                    // The binding first, then the panel: each is valid alone.
                    Ok(m) => send(&st, &s, Command::SetMixer { mixer: spec })
                        .await
                        .map(|()| m),
                }
            }
        },
        "unlink" => match strip_at() {
            None => Err("The panel changed meanwhile; reload the page.".into()),
            Some(i) => {
                let c = &spec.strips[i].control;
                let surface = field(&form, "surface");
                let control = field(&form, "control");
                match cx
                    .surfaces
                    .as_ref()
                    .and_then(|r| r.surfaces.iter().find(|x| x.spec.name == surface))
                {
                    None => Err(format!("There is no surface `{surface}`.")),
                    Some(sf) => {
                        let mut sp = sf.spec.clone();
                        let before = sp.bindings.len();
                        sp.bindings
                            .retain(|b| !(b.control == control && c.links_to(b)));
                        if sp.bindings.len() == before {
                            Ok("Not linked any more.".into())
                        } else {
                            send(&st, &s, Command::SetSurface { surface: sp })
                                .await
                                .map(|()| format!("{control} of {surface} unlinked."))
                        }
                    }
                }
            }
        },
        "learn" => match strip_at() {
            None => Err("The panel changed meanwhile; reload the page.".into()),
            Some(i) => {
                let surface = field(&form, "surface").to_owned();
                let fader = matches!(spec.strips[i].control, StripControl::Fader { .. });
                match learn_control(&st, &s, &surface, fader).await {
                    Err(e) => Err(e),
                    Ok((learned, sp)) => {
                        let body = html! {
                            div.grid { section.card {
                                h2 { "A control moved" span.n { "surface " bdi { (surface) } } }
                                p { "Name it: one Save adds the control to " bdi { (surface) } " and links it to strip " (i + 1) " (" (action_words(spec.strips[i].control.action())) ": " (target_words(&spec.strips[i].control, &cx)) ")." }
                                form.fields.control-form.learned method="post" action="/mixer/edit" {
                                    input type="hidden" name="csrf" value=(s.csrf);
                                    input type="hidden" name="panel" value=(spec.id);
                                    input type="hidden" name="strip" value=(spec.strips[i].id);
                                    input type="hidden" name="surface" value=(surface);
                                    input type="hidden" name="op" value="learned";
                                    (learned_control_fields(&learned, &sp))
                                    button.btn.primary type="submit" { "Save control and link" }
                                }
                                p { a href=(to) { "Back without saving" } }
                            } }
                        };
                        return layout(
                            &view("Learned control", "/mixer"),
                            &Chrome::of(&st, &headers),
                            Some(&s),
                            st.take_flash(&token),
                            &body,
                        )
                        .into_response();
                    }
                }
            }
        },
        "learned" => match strip_at() {
            None => Err("The panel changed meanwhile; reload the page.".into()),
            Some(i) => {
                let surface = field(&form, "surface");
                match cx
                    .surfaces
                    .as_ref()
                    .and_then(|r| r.surfaces.iter().find(|x| x.spec.name == surface))
                {
                    None => Err(format!("There is no surface `{surface}`.")),
                    Some(sf) => {
                        let mut sp = sf.spec.clone();
                        let c = spec.strips[i].control.clone();
                        match crate::pages::surfaces::control_from_form(&form, &sp) {
                            Err(e) => Err(e),
                            Ok(control) if !kind_fits(&c, control.kind) => {
                                let _ = control;
                                Err("A fader strip links to a fader, a button strip to a button: learn again.".into())
                            }
                            Ok(control) => {
                                let host = match &cx.main {
                                    MainTarget::HostVolume(h) => Some(h),
                                    _ => None,
                                };
                                let cid = control.id.clone();
                                sp.bindings.push(binding_for(&c, &cid, host));
                                sp.controls.push(control);
                                match send(&st, &s, Command::SetSurface { surface: sp }).await {
                                    Err(e) => Err(e),
                                    Ok(()) => send(&st, &s, Command::SetMixer { mixer: spec })
                                        .await
                                        .map(|()| {
                                            format!(
                                                "Control `{cid}` of {surface} added and linked."
                                            )
                                        }),
                                }
                            }
                        }
                    }
                }
            }
        },
        _ => Err("Unknown change.".into()),
    };
    let at = match op {
        "rename" => "#panel",
        "add" => "#strips",
        _ => "",
    };
    let strip = field(&form, "strip");
    let to = if !at.is_empty() {
        format!("{to}{at}")
    } else if strip.is_empty() {
        to
    } else {
        format!("{to}#edit-{}", path_segment(strip))
    };
    back(&st, &token, &to, r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_control_types::{Channel, DeviceKey, Direction};

    fn f(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    #[test]
    fn an_add_form_becomes_a_strip_control() {
        let rx = |n| {
            ports::encode(&RouterPort::NetworkRx {
                device: DeviceKey("main".into()),
                channel: Channel::new(n).unwrap(),
            })
        };
        let tx = ports::encode(&RouterPort::NetworkTx {
            device: DeviceKey("main".into()),
            channel: Channel::new(1).unwrap(),
        });
        let c = control_of(&f(&[("action", "gain"), ("ch", &rx(2)), ("ch", &rx(1))])).unwrap();
        assert_eq!(
            c.target().unwrap().channels,
            [Channel::new(1).unwrap(), Channel::new(2).unwrap()]
        );
        assert_eq!(c.target().unwrap().direction, Direction::Rx);
        assert!(control_of(&f(&[("action", "gain"), ("ch", &rx(1)), ("ch", &tx)])).is_err());
        assert!(control_of(&f(&[("action", "output_mute"), ("ch", &tx)])).is_err());
        assert!(control_of(&f(&[("action", "port_gain")])).is_err());
        let h = control_of(&f(&[("action", "host_mute"), ("mode", "momentary")])).unwrap();
        assert_eq!(
            h,
            StripControl::Button {
                action: BindingAction::HostMute,
                target: None,
                ports: vec![],
                crosspoints: vec![],
                mode: ButtonMode::Momentary
            }
        );
        let r = control_of(&f(&[
            ("action", "gain"),
            ("ch", &rx(1)),
            ("min_db", "-40"),
            ("max_db", "6"),
        ]))
        .unwrap();
        assert!(matches!(
            r,
            StripControl::Fader {
                range: Some(FaderRange {
                    min_mdb: -40_000,
                    max_mdb: 6_000,
                    ..
                }),
                ..
            }
        ));
        assert!(control_of(&f(&[("action", "gain"), ("ch", &rx(1)), ("min_db", "-40")])).is_err());
    }

    #[test]
    fn a_links_binding_is_the_strips_action_and_target() {
        let hv = HostVolumeStatus::new("Headphones", "Digital", -50_000, 0);
        let b = binding_for(&StripControl::host_volume(), "f1", Some(&hv));
        assert_eq!(b.action, BindingAction::HostVolume);
        assert_eq!((b.min_mdb, b.max_mdb), (-50_000, 0));
        assert!(StripControl::host_volume().links_to(&b));
        let p = RouterPort::CardIn {
            card: "hw:CARD=CQ,DEV=0".into(),
            channel: Channel::new(15).unwrap(),
        };
        let c = StripControl::Button {
            action: BindingAction::PortMute,
            target: None,
            ports: vec![p.clone()],
            crosspoints: vec![],
            mode: ButtonMode::Momentary,
        };
        let b = binding_for(&c, "b1", None);
        assert_eq!(b.ports, [p]);
        assert_eq!(b.mode, ButtonMode::Momentary);
        assert!(c.links_to(&b));
    }

    #[test]
    fn panel_ids_come_from_titles_and_stay_unique() {
        let saved = vec![MixerSpec::new("stage", "Stage", vec![])];
        assert_eq!(panel_id("Stage", &saved), "stage-2");
        assert_eq!(panel_id("Front of house", &saved), "front-of-house");
        assert_eq!(link_of("cq/fader-1"), Some(("cq".into(), "fader-1".into())));
        assert_eq!(link_of("cq/a b"), None);
    }
}
