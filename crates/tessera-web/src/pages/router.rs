//! The Router page (`/router`, provisional 0.x): the host router's one grid
//! between local I/O and the network channels (ADR 0017).
//!
//! Sources are rows (network RX, applications, card inputs, then taps of
//! destinations), destinations are columns (host output, network TX, card
//! outputs). A patched crosspoint opens its editor (gain, mute, remove); an
//! empty one patches with one click (`SetCrosspoint`). Crosspoints a
//! network card bridge implies are read-only and link to the bridge. A port
//! header opens the port's editor: its level and mute (`SetPortGain`,
//! `SetPortMute`), its label (`SetChannelLabel` for network channels,
//! `SetPortLabel` for local ports and host outputs) and, for a card,
//! "Patch to network". On a phone the grid is a list per destination.
//!
//! Every change is one of the daemon's router commands; the daemon is the
//! only authority and refuses what its runtime cannot apply
//! (`Unsupported`), which the page shows. A daemon whose report says
//! `read_only` gets the grid without edit controls.

use axum::Form;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use maud::{Markup, html};
use serde::Deserialize;
use tessera_control_types::{
    Channel, Command, CommandError, Crosspoint, CrosspointKey, CrosspointStatus, DeviceStatus,
    Direction, Gain, RequestBody, RouterPort, RouterReport, Status,
};
use tesseractl::client::ClientError;

use super::surfaces::{Question, ask};
use super::{
    SLIDER_MAX_DB, SLIDER_OFF_DB, back, error_box, fill_class, need, need_post, page, path_segment,
    refusal, slider_text, slider_value, status_of, view,
};
use crate::ports::{self, decode, encode, encode_xp, port_id, xp_id};
use crate::{Shared, call, explain};

/// How often the page reads the router again, seconds.
const LIVE_EVERY: u32 = 2;

/// Where a router form may send the browser back: `/router` (with
/// `?taps=1` and a fragment), or one of its editors. Only characters a
/// [`path_segment`]-encoded value uses.
#[must_use]
pub fn back_to(back: &str) -> Option<String> {
    let rest = back.strip_prefix("/router")?;
    let next = rest.chars().next();
    if !matches!(next, None | Some('?' | '#' | '/')) || back.contains("//") || back.len() > 2048 {
        return None;
    }
    let ok = rest.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'~' | b'%' | b'=' | b'&' | b'?' | b'#' | b'/'
            )
    });
    let known = rest.is_empty()
        || rest.starts_with('#')
        || rest.starts_with("?taps=1")
        || rest.starts_with("/crosspoint?x=")
        || rest.starts_with("/port?p=");
    (ok && known).then(|| back.to_owned())
}

/// The editor of a crosspoint.
#[must_use]
pub fn crosspoint_path(k: &CrosspointKey) -> String {
    format!("/router/crosspoint?x={}", path_segment(&encode_xp(k)))
}

/// The editor of a port.
#[must_use]
pub fn port_path(p: &RouterPort) -> String {
    format!("/router/port?p={}", path_segment(&encode(p)))
}

/// What the page needs: the router and the devices.
struct Snapshot {
    router: RouterReport,
    status: Option<Status>,
}

/// The router, or why there is none to show: an older daemon answers
/// `Unsupported` or does not know the query.
async fn snapshot(st: &Shared) -> Result<Snapshot, String> {
    let router = ports::router_of(st).await.map_err(|e| match &e {
        ClientError::Command(CommandError::Unsupported { .. }) | ClientError::Protocol(_) => format!(
            "This tesserad does not report a host router yet ({}). Network routing and the Audio page's card bridges work as before.",
            explain(&e)
        ),
        _ => explain(&e),
    })?;
    let status = status_of(st).await.ok();
    Ok(Snapshot { router, status })
}

/// Destinations in column order: host output, network TX, card outputs.
fn dest_order(p: &RouterPort) -> u8 {
    match p {
        RouterPort::HostOut { .. } => 0,
        RouterPort::NetworkTx { .. } => 1,
        _ => 2,
    }
}

/// Sources in row order: network RX, apps, card inputs.
fn source_order(p: &RouterPort) -> u8 {
    match p {
        RouterPort::NetworkRx { .. } => 0,
        RouterPort::AppOut { .. } => 1,
        RouterPort::CardIn { .. } => 2,
        _ => 3,
    }
}

/// The group a port is shown under: kind in plain words, then the device's
/// name (not its key) when there are several.
fn group_title(p: &RouterPort, status: Option<&Status>, tap: bool) -> String {
    let base = match p {
        RouterPort::CardIn { card, .. } => {
            format!("{} inputs (sound card)", ports::card_short(card))
        }
        RouterPort::CardOut { card, .. } => {
            format!("{} outputs (sound card)", ports::card_short(card))
        }
        other => ports::kind_title(other).to_owned(),
    };
    let base = match (p.device(), status) {
        (Some(d), Some(s)) if s.devices.len() > 1 => {
            let name = s
                .devices
                .iter()
                .find(|x| &x.key == d)
                .map_or(d.0.as_str(), |x| x.name.as_str());
            format!("{base} \u{b7} {name}")
        }
        _ => base,
    };
    if tap { format!("Taps: {base}") } else { base }
}

/// What a group is, for the ones whose title cannot say it all.
const fn group_hint(p: &RouterPort, tap: bool) -> Option<&'static str> {
    if tap {
        return Some("what a destination carries, after its level and mute, used as a source");
    }
    match p {
        RouterPort::AppOut { .. } => Some(
            "what local applications play into this device (through the tessera ALSA or PipeWire plugin)",
        ),
        RouterPort::HostOut { .. } => Some(
            "what this host plays: local applications capture it (ALSA or PipeWire plugin); on a speaker, its DSP and amplifier play it",
        ),
        RouterPort::NetworkRx { .. } => Some("audio received from the network"),
        RouterPort::NetworkTx { .. } => Some("audio this device sends to the network"),
        _ => None,
    }
}

/// Ports in groups, keeping order.
fn grouped<'a>(
    ports: &[&'a tessera_control_types::PortStatus],
    status: Option<&Status>,
    tap: bool,
) -> Vec<(String, Vec<&'a tessera_control_types::PortStatus>)> {
    let mut out: Vec<(String, Vec<&tessera_control_types::PortStatus>)> = Vec::new();
    for p in ports {
        let t = group_title(&p.port, status, tap);
        match out.last_mut() {
            Some((title, v)) if *title == t => v.push(p),
            _ => out.push((t, vec![p])),
        }
    }
    out
}

/// A gain in a cell: `0`, `−6`, `+3`, `off`.
fn cell_gain(g: Gain) -> String {
    match g.milli_db() {
        None => "off".into(),
        Some(0) => "0".into(),
        Some(m) => {
            let v = f64::from(m) / 1000.0;
            let s = if (v.fract()).abs() < 1e-9 {
                format!("{:.0}", v.abs())
            } else {
                format!("{:.1}", v.abs())
            };
            if m < 0 {
                format!("\u{2212}{s}")
            } else {
                format!("+{s}")
            }
        }
    }
}

/// The words of a patched crosspoint for its link and the list.
fn xp_state_words(x: &CrosspointStatus) -> String {
    let (_, origin) = ports::origin_words(&x.origin);
    let mut w = format!("patched, {}", slider_text(x.crosspoint.gain));
    if x.crosspoint.muted {
        w.push_str(", muted");
    }
    w.push_str(&format!(", {origin}"));
    if !x.active {
        w.push_str(", inactive");
        if let Some(p) = &x.problem {
            w.push_str(&format!(": {p}"));
        }
    }
    if let Some(o) = &x.gain_owner {
        w.push_str(&format!(", gain set by the {o} fader"));
    }
    w
}

/// One crosspoint cell. A patched one opens its editor (in place with the
/// page's script, a page without it); an empty one patches with one click
/// (in place with the script, a form post without it).
#[allow(clippy::too_many_arguments)]
fn cell(
    r: &RouterReport,
    src: &RouterPort,
    dst: &RouterPort,
    row: usize,
    col: usize,
    back: &str,
    csrf: &str,
) -> Markup {
    let key = CrosspointKey::new(src.clone(), dst.clone());
    let id = xp_id(&key);
    let words = ports::xp_words(Some(r), &key);
    let x = ports::crosspoint(r, &key);
    html! {
        @if src == dst {
            td.xp-cell.na data-row=(row) data-col=(col) { span.sr { "the same port" } }
        } @else if let Some(x) = x {
            @let (badge, _) = ports::origin_words(&x.origin);
            td.xp-cell id=(id) data-row=(row) data-col=(col) {
                @if let tessera_control_types::CrosspointOrigin::Bridge { name } = &x.origin {
                    a.xp.on.bridge.inactive[!x.active] href=(format!("/audio#bridge-{}", path_segment(name)))
                        aria-label=(format!("{words}: {}; change it on the Audio page", xp_state_words(x)))
                        title=(format!("{words}: {}", xp_state_words(x))) {
                        span.g aria-hidden="true" { "B" }
                    }
                } @else {
                    a.xp.on.inactive[!x.active].muted[x.crosspoint.muted] href=(crosspoint_path(&key)) data-origin=(badge)
                        data-router-open
                        aria-label=(format!("{words}: {}; edit", xp_state_words(x)))
                        title=(format!("{words}: {}", xp_state_words(x))) {
                        span.g aria-hidden="true" { (if x.crosspoint.muted { "M".to_owned() } else { cell_gain(x.crosspoint.gain) }) }
                    }
                }
            }
        } @else if r.read_only {
            td.xp-cell id=(id) data-row=(row) data-col=(col) { span.xp aria-hidden="true" {} }
        } @else {
            td.xp-cell id=(id) data-row=(row) data-col=(col) {
                form.xp-form method="post" action="/router/crosspoint" data-router-patch {
                    input type="hidden" name="csrf" value=(csrf);
                    input type="hidden" name="source" value=(encode(src));
                    input type="hidden" name="destination" value=(encode(dst));
                    input type="hidden" name="on" value="true";
                    input type="hidden" name="back" value=(format!("{back}#{id}"));
                    button.xp type="submit" aria-label=(format!("Patch {words}")) title=(format!("Patch {words}")) { span.g aria-hidden="true" { "+" } }
                }
            }
        }
    }
}

/// A port's header: its name, where it is when the name says something
/// else, a link to its editor.
fn port_head(
    r: &RouterReport,
    p: &tessera_control_types::PortStatus,
    status: Option<&Status>,
) -> Markup {
    let place = ports::place_on(&p.port, status);
    // A card input feeding a named network channel goes by that channel's
    // name, with the card input below it (as a named port shows where it
    // is), not "CQ input 15 → CQ Main L", which reads like a route inside
    // the grid.
    let carried = (!ports::named(p))
        .then(|| ports::network_name(r, &p.port))
        .flatten();
    if let Some(network) = carried {
        return html! {
            a.port.off[!p.available] href=(port_path(&p.port)) data-router-open
                title=(format!("{place}, sent to the network as {network}")) {
                bdi.name { (network) }
                " " span.where { span.sr { "from " } (place) }
                @if p.muted { " " span.tag.bad { "muted" } }
                @if !p.available { span.sr { " (not available now)" } }
            }
        };
    }
    let name = ports::name(r, p);
    html! {
        a.port.off[!p.available] href=(port_path(&p.port)) data-router-open title=(place) {
            bdi.name { (name) }
            @if name != place && !name.starts_with(&place) { " " span.where { (place) } }
            @if p.muted { " " span.tag.bad { "muted" } }
            @if !p.available { span.sr { " (not available now)" } }
        }
    }
}

/// Source rows in groups: title, what the group is, (row number, port).
type Rows<'a> = Vec<(
    String,
    Option<&'static str>,
    Vec<(usize, &'a tessera_control_types::PortStatus)>,
)>;

/// Up to this many destinations, their headers are horizontal (and wrap);
/// more are rotated to keep the grid narrow.
const HORIZONTAL_HEADERS: usize = 10;

/// The grid: sources as rows, destinations as columns.
#[allow(clippy::too_many_arguments)]
fn matrix(
    r: &RouterReport,
    sources: &[&tessera_control_types::PortStatus],
    dests: &[&tessera_control_types::PortStatus],
    status: Option<&Status>,
    taps: &[&tessera_control_types::PortStatus],
    back: &str,
    csrf: &str,
) -> Markup {
    let dgroups = grouped(dests, status, false);
    let mut row = 0usize;
    let rows: Rows<'_> = grouped(sources, status, false)
        .into_iter()
        .map(|(t, v)| (t, v, false))
        .chain(
            grouped(taps, status, true)
                .into_iter()
                .map(|(t, v)| (t, v, true)),
        )
        .map(|(title, v, tap)| {
            let hint = v.first().and_then(|p| group_hint(&p.port, tap));
            let v = v
                .into_iter()
                .map(|p| {
                    row += 1;
                    (row - 1, p)
                })
                .collect();
            (title, hint, v)
        })
        .collect();
    let ncols = dests.len();
    let wide = ncols <= HORIZONTAL_HEADERS;
    html! {
        div.table.router-scroll {
            table.router-grid.wide[wide] data-router-grid {
                caption.sr { "Host router: sources in rows, destinations in columns. A patched crosspoint opens its editor; an empty one patches." }
                thead {
                    tr {
                        td.corner rowspan="2" { span.muted { "source \u{2193} \u{b7} destination \u{2192}" } }
                        @for (title, v) in &dgroups {
                            @let hint = v.first().and_then(|p| group_hint(&p.port, false));
                            th.dgroup scope="colgroup" colspan=(v.len()) title=[hint] { (title) }
                        }
                    }
                    tr {
                        @for d in dests {
                            th.dport scope="col" id=(port_id(&d.port)) { (port_head(r, d, status)) }
                        }
                    }
                }
                @for (title, hint, v) in &rows {
                    tbody {
                        tr.sgroup { th scope="rowgroup" colspan=(ncols + 1) { (title) @if let Some(h) = hint { " " span.hint { (h) } } } }
                        @for (this, s) in v {
                            tr {
                                th.sport scope="row" { (port_head(r, s, status)) }
                                @for (col, d) in dests.iter().enumerate() {
                                    (cell(r, &s.port, &d.port, *this, col, back, csrf))
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The phone layout: per destination, the sources that feed it, each with
/// Edit and Remove, and a picker to add one.
fn list(
    r: &RouterReport,
    sources: &[&tessera_control_types::PortStatus],
    dests: &[&tessera_control_types::PortStatus],
    status: Option<&Status>,
    back: &str,
    csrf: &str,
) -> Markup {
    html! {
        div.router-list {
            @for d in dests {
                @let feeding: Vec<&CrosspointStatus> = r.crosspoints.iter().filter(|x| x.crosspoint.destination == d.port).collect();
                @let anchor = format!("l{}", port_id(&d.port));
                section.dest id=(anchor) {
                    h3 { (port_head(r, d, status)) }
                    @if feeding.is_empty() { p.muted { "Nothing feeds it." } }
                    ul.feeds {
                        @for x in &feeding {
                            @let k = x.crosspoint.key();
                            @let words = ports::xp_words(Some(r), &k);
                            li.off[!x.active] {
                                span.feed { bdi { (ports::label(Some(r), &k.source)) } " " span.muted { (xp_state_words(x)) } }
                                @if let tessera_control_types::CrosspointOrigin::Bridge { name } = &x.origin {
                                    span.feed-actions { a.btn.sm href=(format!("/audio#bridge-{}", path_segment(name))) aria-label=(format!("Change {words} on the Audio page")) { "Change on the Audio page" } }
                                } @else {
                                    span.feed-actions {
                                        a.btn.sm href=(crosspoint_path(&k)) data-router-open aria-label=(format!("Edit {words}")) { "Edit" }
                                        @if !r.read_only {
                                            form.inline method="post" action="/router/crosspoint" data-router-patch {
                                                input type="hidden" name="csrf" value=(csrf);
                                                input type="hidden" name="source" value=(encode(&k.source));
                                                input type="hidden" name="destination" value=(encode(&k.destination));
                                                input type="hidden" name="on" value="false";
                                                input type="hidden" name="back" value=(format!("{back}#{anchor}"));
                                                button.btn.sm.danger type="submit" aria-label=(format!("Remove {words}")) { "Remove" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    @if !r.read_only {
                        @let free: Vec<&&tessera_control_types::PortStatus> = sources.iter().filter(|s| s.port != d.port && !feeding.iter().any(|x| x.crosspoint.source == s.port)).collect();
                        @if !free.is_empty() {
                            @let fid = format!("add-{}", port_id(&d.port));
                            form.inline method="post" action="/router/crosspoint" data-router-patch {
                                input type="hidden" name="csrf" value=(csrf);
                                input type="hidden" name="destination" value=(encode(&d.port));
                                input type="hidden" name="on" value="true";
                                input type="hidden" name="back" value=(format!("{back}#{anchor}"));
                                label.sr for=(fid) { "Source to add to " (ports::name(r, d)) }
                                select id=(fid) name="source" {
                                    @for s in &free {
                                        @let name = ports::name(r, s);
                                        @let place = ports::place_on(&s.port, status);
                                        option value=(encode(&s.port)) { (name) @if name != place && !name.starts_with(&place) { " (" (place) ")" } }
                                    }
                                }
                                button.btn.sm type="submit" { "Add source" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The page's query: `taps=1` shows every destination as a source.
#[derive(Debug, Deserialize, Default)]
pub struct GridQuery {
    #[serde(default)]
    taps: String,
}

/// `GET /router`.
pub async fn show(
    State(st): State<Shared>,
    headers: HeaderMap,
    Query(q): Query<GridQuery>,
) -> Response {
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let all_taps = q.taps == "1";
    let body = match snapshot(&st).await {
        Err(e) => html! { div.grid { section.card { h2 { "Router" } (error_box(&e)) } } },
        Ok(snap) => render(&snap, all_taps, &s.csrf),
    };
    page(
        &st,
        &token,
        &s,
        &headers,
        view("Router", "/router").every(LIVE_EVERY),
        &body,
    )
}

fn render(snap: &Snapshot, all_taps: bool, csrf: &str) -> Markup {
    let r = &snap.router;
    let status = snap.status.as_ref();
    let mut sources: Vec<&tessera_control_types::PortStatus> = r
        .ports
        .iter()
        .filter(|p| !p.port.is_destination())
        .collect();
    sources.sort_by_key(|p| source_order(&p.port));
    let mut dests: Vec<&tessera_control_types::PortStatus> =
        r.ports.iter().filter(|p| p.port.is_destination()).collect();
    dests.sort_by_key(|p| dest_order(&p.port));
    let taps: Vec<&tessera_control_types::PortStatus> = dests
        .iter()
        .copied()
        .filter(|d| all_taps || r.crosspoints.iter().any(|x| x.crosspoint.source == d.port))
        .collect();
    let back = if all_taps {
        "/router?taps=1"
    } else {
        "/router"
    };
    let mut all_sources = sources.clone();
    all_sources.extend(taps.iter().copied());
    html! {
        div.grid.router {
            section.card.router-card id="grid" {
                div.surface-head {
                    h2 { "Router" span.n { (if r.saved { "saved grid" } else { "default grid" }) } }
                    @if r.saved && !r.read_only {
                        form.inline method="post" action="/router/reset" {
                            input type="hidden" name="csrf" value=(csrf);
                            button.btn.sm.danger type="submit" { "Reset to default\u{2026}" }
                        }
                    }
                }
                @if r.read_only {
                    p.flash.bad role="status" { "This tesserad cannot change the host router yet: the grid shows what it applies, read-only. Channel levels, monitoring and bridges still change on their own pages." }
                } @else if !r.saved {
                    p.muted { "Nothing is saved: the default grid applies (each network input to the host output of the same number, each app's playback to the network output of the same number, local monitoring, configured routes). The first change saves this grid, then changes it." }
                }
                p.muted {
                    "Network inputs (RX) are audio from the network; network outputs (TX) are what this device sends to it. App playback is what local applications play through the tessera ALSA or PipeWire plugin; host outputs are what this host plays (and what local applications capture). A destination sums every source patched to it. Sound cards stay off the network until one of their inputs is patched to a network output. Click a port to set its level, mute or label."
                }
                // The page's script posts patches in place and says how
                // they went here.
                p.note.router-note role="status" aria-live="polite" data-live-local {}
                @if dests.is_empty() || all_sources.is_empty() {
                    p.muted { "The router has no ports yet: the daemon reports no channels." }
                } @else {
                    (matrix(r, &sources, &dests, status, &taps, back, csrf))
                    (list(r, &all_sources, &dests, status, back, csrf))
                }
                ul.legend aria-label="Crosspoint origins" {
                    li { span.xp.on data-origin="saved" { "0" } span { "saved" } }
                    li { span.xp.on data-origin="default" { "0" } span { "default" } }
                    li { span.xp.on data-origin="configured" { "0" } span { "configured route" } }
                    li { span.xp.on data-origin="monitor" { "0" } span { "local monitoring" } }
                    li { span.xp.on.bridge { "B" } span { "card bridge (change it on the " a href="/audio" { "Audio page" } ")" } }
                    li { span.xp.on.inactive { "0" } span { "inactive (a port is missing)" } }
                    li { span.xp.on.muted { "M" } span { "muted" } }
                }
                p {
                    @if all_taps { a href="/router" { "Show only the taps in use" } }
                    @else { a href="/router?taps=1" { "Show every destination as a source (taps)" } }
                }
                // The editors open here in place (the page's script); the
                // refresh never touches it.
                dialog.router-dialog id="router-dialog" aria-labelledby="router-dialog-title" data-live-local {
                    div.dialog-head {
                        h2 id="router-dialog-title" { "Edit" }
                        button.btn.sm type="button" data-router-close { "Close" }
                    }
                    p.note.dialog-note role="status" aria-live="polite" data-live-local {}
                    div data-router-dialog-content {}
                }
            }
            @if !r.read_only {
                @if let Some(status) = &snap.status { (patch_card(r, status, csrf)) }
            }
        }
    }
}

/// "Patch to network" and "Feed from the network" per card.
fn patch_card(r: &RouterReport, status: &Status, csrf: &str) -> Markup {
    let Some(dev) = status.devices.first() else {
        return html! {};
    };
    let mut cards: Vec<(String, bool)> = Vec::new();
    for p in &r.ports {
        let (card, input) = match &p.port {
            RouterPort::CardIn { card, .. } => (card, true),
            RouterPort::CardOut { card, .. } => (card, false),
            _ => continue,
        };
        if !cards.iter().any(|(c, i)| c == card && *i == input) {
            cards.push((card.clone(), input));
        }
    }
    html! {
        section.card id="patch" {
            h2 { "Patch cards to the network" span.n { "a card stays local until patched" } }
            @if cards.is_empty() {
                p.muted { "No card ports: open a card for local routing on the " a href="/audio" { "Audio page" } " first." }
            }
            @for (card, input) in &cards {
                (patch_form(r, status, dev, card, *input, csrf))
            }
        }
    }
}

/// One card's patch form: its inputs (or outputs) not on the network yet,
/// each with the label its new channel gets.
fn patch_form(
    r: &RouterReport,
    status: &Status,
    dev: &DeviceStatus,
    card: &str,
    input: bool,
    csrf: &str,
) -> Markup {
    let free: Vec<&tessera_control_types::PortStatus> = r
        .ports
        .iter()
        .filter(|p| p.available)
        .filter(|p| match &p.port {
            RouterPort::CardIn { card: c, .. } if input => c == card,
            RouterPort::CardOut { card: c, .. } if !input => c == card,
            _ => false,
        })
        .filter(|p| {
            !r.crosspoints.iter().any(|x| {
                if input {
                    x.crosspoint.source == p.port
                        && matches!(x.crosspoint.destination, RouterPort::NetworkTx { .. })
                } else {
                    x.crosspoint.destination == p.port
                        && matches!(x.crosspoint.source, RouterPort::NetworkRx { .. })
                }
            })
        })
        .collect();
    let short = ports::card_short(card);
    let (have, word) = if input {
        (dev.tx_channels.len(), "TX")
    } else {
        (dev.rx_channels.len(), "RX")
    };
    html! {
        details.edit.patch {
            summary {
                @if input { "Send " (short) " inputs to the network" } @else { "Feed " (short) " outputs from the network" }
            }
            @if free.is_empty() {
                p.muted { @if input { "Every input of this card is on the network already." } @else { "Every output of this card is fed from the network already." } }
            } @else {
                form.fields method="post" action="/router/patch" {
                    input type="hidden" name="csrf" value=(csrf);
                    input type="hidden" name="direction" value=(if input { "in" } else { "out" });
                    @if status.devices.len() == 1 {
                        input type="hidden" name="device" value=(dev.key.0);
                    } @else {
                        label { "Device"
                            select name="device" { @for d in &status.devices { option value=(d.key.0) { (d.name) } } }
                        }
                    }
                    fieldset.labels {
                        legend { @if input { "Inputs to send, and their TX channel labels" } @else { "Outputs to feed, and their RX channel labels" } }
                        @for p in &free {
                            @let v = encode(&p.port);
                            @let fid = format!("patch-{}", port_id(&p.port));
                            div.patch-row {
                                @let name = ports::name(r, p);
                                label.check { input type="checkbox" name="port" value=(v); span { bdi { (name) } } }
                                label.sr for=(fid) { "Label of the new channel for " (name) }
                                input id=(fid) type="text" name=(format!("label:{v}")) value=(ports::short(Some(r), &p.port)) maxlength="31" size="14";
                            }
                        }
                    }
                    p.muted {
                        (ports::device_subject(&status.devices, &dev.key)) " has " (have) " " (word) " channel(s); each one ticked adds one more (the device restarts; you confirm first). "
                        @if input { "Other devices on the network then see them as channels of this device." }
                        @else { "Route network audio to the new RX channels on the " a href="/routing" { "Network routing page" } "." }
                    }
                    button.btn.primary type="submit" { @if input { "Patch to network" } @else { "Feed from the network" } }
                }
            }
        }
    }
}

// ---- editors -----------------------------------------------------------------

/// The editors' query.
#[derive(Debug, Deserialize, Default)]
pub struct EditorQuery {
    /// A crosspoint ([`ports::encode_xp`]).
    #[serde(default)]
    x: String,
    /// A port ([`ports::encode`]).
    #[serde(default)]
    p: String,
}

/// A level fader of the editors: posts to the level handlers.
#[allow(clippy::too_many_arguments)]
fn level_form(
    action: &str,
    field: (&str, String),
    gain: Gain,
    what: &str,
    owner: Option<&str>,
    disabled: bool,
    back: &str,
    id: &str,
    csrf: &str,
) -> Markup {
    let v = slider_value(gain);
    let owned_id = format!("{id}-owner");
    html! {
        form.gain.locked[owner.is_some()] method="post" action=(action) data-live-slider="gain" {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name=(field.0) value=(field.1);
            input type="hidden" name="back" value=(back);
            label for=(id) { (what) }
            input id=(id) type="range" name="db" min=(SLIDER_OFF_DB) max=(SLIDER_MAX_DB) step="0.5" value=(v)
                class=(fill_class(v, SLIDER_OFF_DB, SLIDER_MAX_DB)) aria-valuetext=(slider_text(gain))
                aria-describedby=[owner.map(|_| owned_id.as_str())]
                disabled[owner.is_some() || disabled];
            output.readout data-readout for=(id) { (slider_text(gain)) }
            span.note data-live-local {}
            @if let Some(o) = owner {
                span.owned id=(owned_id) { "Locked: set by the " a href=(super::surfaces::surface_anchor(o)) { bdi { (o) } " fader" } }
            } @else if !disabled {
                button.btn.sm.nojs type="submit" { "Set" }
            }
        }
    }
}

/// A mute toggle of the editors.
fn mute_form(
    action: &str,
    field: (&str, String),
    muted: bool,
    what: &str,
    back: &str,
    csrf: &str,
) -> Markup {
    html! {
        div.press {
            form.inline method="post" action=(action) data-live-post {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name=(field.0) value=(field.1);
                input type="hidden" name="back" value=(back);
                input type="hidden" name="muted" value=(if muted { "false" } else { "true" });
                button.mute.on[muted] type="submit" aria-pressed=(if muted { "true" } else { "false" }) aria-label=(format!("Mute {what}")) {
                    (if muted { "Muted" } else { "Mute" })
                }
            }
            span.note data-live-local {}
        }
    }
}

/// `GET /router/crosspoint?x=…`: one crosspoint: gain, mute, remove.
pub async fn crosspoint_editor(
    State(st): State<Shared>,
    headers: HeaderMap,
    Query(q): Query<EditorQuery>,
) -> Response {
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let key = ports::decode_xp(&q.x);
    let body = match (key, snapshot(&st).await) {
        (None, _) => error_box("No such crosspoint."),
        (_, Err(e)) => error_box(&e),
        (Some(k), Ok(snap)) => crosspoint_body(&snap.router, &k, &s.csrf),
    };
    page(
        &st,
        &token,
        &s,
        &headers,
        view("Crosspoint", "/router").every(LIVE_EVERY),
        &body,
    )
}

/// A port in an editor's sentence: its name, linked to its editor, and
/// where it is when the name says something else.
fn port_ref(r: &RouterReport, p: &RouterPort) -> Markup {
    let name = ports::short(Some(r), p);
    let place = ports::place(p);
    html! {
        a href=(port_path(p)) data-router-open { bdi { (name) } }
        @if name != place { " (" (place) ")" }
    }
}

fn crosspoint_body(r: &RouterReport, k: &CrosspointKey, csrf: &str) -> Markup {
    let words = ports::xp_words(Some(r), k);
    let back = crosspoint_path(k);
    let v = encode_xp(k);
    html! {
        div.grid { section.card.editor id="crosspoint" {
            h2 { (words) }
            @match ports::crosspoint(r, k) {
                None => {
                    p { "Not patched." }
                    @if !r.read_only {
                        form.inline method="post" action="/router/crosspoint" {
                            input type="hidden" name="csrf" value=(csrf);
                            input type="hidden" name="source" value=(encode(&k.source));
                            input type="hidden" name="destination" value=(encode(&k.destination));
                            input type="hidden" name="on" value="true";
                            input type="hidden" name="back" value=(back);
                            button.btn.primary type="submit" { "Patch" }
                        }
                    }
                }
                Some(x) => {
                    @let (_, origin) = ports::origin_words(&x.origin);
                    @let bridge = ports::is_bridge(&x.origin);
                    p { "From " (port_ref(r, &k.source)) " to " (port_ref(r, &k.destination)) ". Origin: " (origin) "." }
                    @if !x.active { p.flash.bad { "Inactive: " (x.problem.as_deref().unwrap_or("a port is missing")) ". It applies again when the port is back." } }
                    @if bridge {
                        @if let tessera_control_types::CrosspointOrigin::Bridge { name } = &x.origin {
                            p.muted { "A card bridge implies this crosspoint: its level is the ports' levels; change or convert the bridge on the " a href=(format!("/audio#bridge-{}", path_segment(name))) { "Audio page" } "." }
                        }
                    } @else {
                        div.editor-controls {
                            (level_form("/levels/crosspoint/gain", ("xp", v.clone()), x.crosspoint.gain, "Gain", x.gain_owner.as_deref(), r.read_only, &back, "xp-gain", csrf))
                            @if !r.read_only { (mute_form("/levels/crosspoint/mute", ("xp", v.clone()), x.crosspoint.muted, &words, &back, csrf)) }
                        }
                        @if !r.read_only {
                            form.inline method="post" action="/router/crosspoint" data-router-patch="remove" {
                                input type="hidden" name="csrf" value=(csrf);
                                input type="hidden" name="source" value=(encode(&k.source));
                                input type="hidden" name="destination" value=(encode(&k.destination));
                                input type="hidden" name="on" value="false";
                                input type="hidden" name="back" value="/router";
                                button.btn.sm.danger type="submit" { "Remove crosspoint" }
                            }
                            span.note data-live-local {}
                        }
                    }
                }
            }
            p { a href=(format!("/router#{}", xp_id(k))) data-router-back { "Back to the router" } }
        } }
    }
}

/// `GET /router/port?p=…`: one port: level, mute, label, its crosspoints,
/// and for a card "Patch to network".
pub async fn port_editor(
    State(st): State<Shared>,
    headers: HeaderMap,
    Query(q): Query<EditorQuery>,
) -> Response {
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let port = decode(&q.p);
    let body = match (port, snapshot(&st).await) {
        (None, _) => error_box("No such port."),
        (_, Err(e)) => error_box(&e),
        (Some(p), Ok(snap)) => port_body(&snap, &p, &s.csrf),
    };
    page(
        &st,
        &token,
        &s,
        &headers,
        view("Router port", "/router").every(LIVE_EVERY),
        &body,
    )
}

fn port_body(snap: &Snapshot, p: &RouterPort, csrf: &str) -> Markup {
    let r = &snap.router;
    let back = port_path(p);
    let v = encode(p);
    let st = ports::status(r, p);
    let label = ports::label(Some(r), p);
    let place = ports::place_on(p, snap.status.as_ref());
    let into: Vec<&CrosspointStatus> = r
        .crosspoints
        .iter()
        .filter(|x| &x.crosspoint.destination == p)
        .collect();
    let from: Vec<&CrosspointStatus> = r
        .crosspoints
        .iter()
        .filter(|x| &x.crosspoint.source == p)
        .collect();
    html! {
        div.grid { section.card.editor id="port" {
            // A name and its place read as two things, also as plain text
            // (the Router dialog's title): `Speaker R — Host output 2`.
            h2 { bdi { (label) } @if label != place { " " span.n { "\u{2014} " (place) } } }
            @match st {
                None => { p.muted { "The router does not list this port." } }
                Some(st) => {
                    @if !st.available { p.flash.bad { "Not available now (the device has fewer channels, or the card's bridge is off). Its saved level and crosspoints are kept." } }
                    div.editor-controls {
                        (level_form("/levels/port/gain", ("port", v.clone()), st.gain, "Level", st.gain_owner.as_deref(), false, &back, "port-gain", csrf))
                        (mute_form("/levels/port/mute", ("port", v.clone()), st.muted, &label, &back, csrf))
                    }
                    div.press {
                        form.inline method="post" action="/router/label" data-live-post {
                            input type="hidden" name="csrf" value=(csrf);
                            input type="hidden" name="port" value=(v);
                            label for="port-label" { "Label" }
                            input id="port-label" type="text" name="label" value=(if ports::named(st) { st.label.as_str() } else { "" }) placeholder=(place) maxlength="31" size="18" aria-describedby="port-label-help";
                            button.btn.sm type="submit" { "Rename" }
                        }
                        span.note data-live-local {}
                    }
                    p.muted id="port-label-help" { "Empty: its default name again (" (place) ")." }
                }
            }
            @if p.is_destination() {
                h3 { "Fed by" }
                @if into.is_empty() { p.muted { "Nothing." } }
                ul { @for x in &into { li { a href=(crosspoint_path(&x.crosspoint.key())) data-router-open { bdi { (ports::label(Some(r), &x.crosspoint.source)) } } " " span.muted { (xp_state_words(x)) } } } }
            }
            h3 { "Feeds" }
            @if from.is_empty() { p.muted { "Nothing." } }
            ul { @for x in &from { li { a href=(crosspoint_path(&x.crosspoint.key())) data-router-open { bdi { (ports::label(Some(r), &x.crosspoint.destination)) } } " " span.muted { (xp_state_words(x)) } } } }
            @if !r.read_only {
                @if let (Some(status), Some(card)) = (&snap.status, p.card()) {
                    @if let Some(dev) = status.devices.first() {
                        (patch_form(r, status, dev, card, matches!(p, RouterPort::CardIn { .. }), csrf))
                    }
                }
            }
            p { a href=(format!("/router#{}", port_id(p))) data-router-back { "Back to the router" } }
        } }
    }
}

// ---- changing ------------------------------------------------------------------

fn field<'a>(form: &'a [(String, String)], name: &str) -> &'a str {
    form.iter()
        .find(|(k, _)| k == name)
        .map_or("", |(_, v)| v.as_str())
}

async fn send(st: &Shared, session: &str, cmd: Command) -> Result<(), (StatusCode, String)> {
    let kind = cmd.kind();
    let r = call(st, RequestBody::Command(cmd)).await;
    tracing::info!(session = %session, command = kind, ok = r.is_ok(), "page command");
    r.map(|_| ()).map_err(|e| refusal(&e))
}

/// `POST /router/crosspoint`: `source`, `destination`, `on` (`true`:
/// patch at unity, `false`: remove).
pub async fn set_crosspoint(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, field(&form, "csrf")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let to = back_to(field(&form, "back")).unwrap_or_else(|| "/router".into());
    let on = field(&form, "on") != "false";
    let r: Result<String, (StatusCode, String)> = async {
        let (Some(source), Some(destination)) = (
            decode(field(&form, "source")),
            decode(field(&form, "destination")),
        ) else {
            return Err((
                StatusCode::BAD_REQUEST,
                "Name a source and a destination port.".into(),
            ));
        };
        if !destination.is_destination() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("{} is not a destination.", ports::place(&destination)),
            ));
        }
        let k = CrosspointKey::new(source.clone(), destination.clone());
        // The names as the page shows them, when the router can be read.
        let report = ports::router_of(&st).await.ok();
        let words = ports::xp_words(report.as_ref(), &k);
        if on {
            send(
                &st,
                &s.id,
                Command::SetCrosspoint {
                    crosspoint: Crosspoint::new(source, destination),
                },
            )
            .await?;
            Ok(format!("Patched: {words}."))
        } else {
            send(
                &st,
                &s.id,
                Command::RemoveCrosspoint {
                    source,
                    destination,
                },
            )
            .await?;
            Ok(format!("Removed: {words}."))
        }
    }
    .await;
    super::answer(&st, &token, &headers, &to, r, serde_json::json!({}))
}

/// The command that names a port (`label`: empty for its default name
/// again): a network channel with `SetChannelLabel`, a local port or host
/// output with `SetPortLabel` (the grid is not touched).
fn label_command(port: &RouterPort, text: &str) -> Command {
    let label = (!text.is_empty()).then(|| text.to_owned());
    match port {
        RouterPort::NetworkRx { device, channel } | RouterPort::NetworkTx { device, channel } => {
            Command::SetChannelLabel {
                device: device.clone(),
                direction: if matches!(port, RouterPort::NetworkRx { .. }) {
                    Direction::Rx
                } else {
                    Direction::Tx
                },
                channel: *channel,
                label,
            }
        }
        _ => Command::SetPortLabel {
            port: port.clone(),
            label,
        },
    }
}

/// `POST /router/label`: `port`, `label` (empty: the default name again);
/// see [`label_command`]. Posted in place, the answer is JSON.
pub async fn label(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, field(&form, "csrf")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let port = decode(field(&form, "port"));
    let to = port
        .as_ref()
        .map_or_else(|| "/router".to_owned(), port_path);
    let text = field(&form, "label").trim().to_owned();
    let r: Result<String, (StatusCode, String)> = async {
        let Some(port) = port else {
            return Err((StatusCode::BAD_REQUEST, "No such port.".into()));
        };
        // The daemon checks the rest (ASCII, no `=`, `.` or `@`).
        if text.len() > 31 || text.chars().any(char::is_control) {
            return Err((
                StatusCode::BAD_REQUEST,
                "A label is at most 31 characters, without control characters.".into(),
            ));
        }
        send(&st, &s.id, label_command(&port, &text)).await?;
        let place = ports::place(&port);
        Ok(if text.is_empty() {
            format!("{place} has its default name again.")
        } else {
            format!("{place} is now \u{201c}{text}\u{201d}.")
        })
    }
    .await;
    super::answer(&st, &token, &headers, &to, r, serde_json::json!({}))
}

/// `POST /router/reset`: asks first, then `ResetRouter`.
pub async fn reset(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, field(&form, "csrf")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if field(&form, "confirm") != "yes" {
        let q = Question {
            title: "Reset the router",
            text: html! {
                p { "Forget the saved grid? The default grid applies again: each RX channel to the host output, each app output to its TX channel, local monitoring and configured routes." }
                p.muted { "Port labels and levels stay. Card inputs patched to the network are no longer sent." }
            },
            action: "/router/reset".into(),
            hidden: Vec::new(),
            back_to: "/router".into(),
            active: "/router",
        };
        return ask(&st, &headers, &s, &q);
    }
    let r = send(&st, &s.id, Command::ResetRouter)
        .await
        .map(|()| "The router is back to its default grid.".to_owned());
    back(&st, &token, "/router", r.map_err(|(_, m)| m))
}

/// The commands "Patch to network" (card inputs) or "Feed from the
/// network" (card outputs) needs, in order: more channels (the daemon asks
/// for confirmation: the device restarts), their labels, then per port the
/// crosspoint, and for a new TX channel the removal of the default
/// app → TX crosspoint the new channel gets.
fn patch_commands(
    form: &[(String, String)],
    status: &Status,
) -> Result<(Vec<Command>, String), String> {
    let input = match field(form, "direction") {
        "in" => true,
        "out" => false,
        _ => return Err("Choose inputs or outputs.".into()),
    };
    let device = field(form, "device");
    let Some(dev) = status.devices.iter().find(|d| d.key.0 == device) else {
        return Err(format!("There is no device `{device}`."));
    };
    let chosen: Vec<RouterPort> = form
        .iter()
        .filter(|(k, _)| k == "port")
        .filter_map(|(_, v)| decode(v))
        .filter(|p| {
            if input {
                matches!(p, RouterPort::CardIn { .. })
            } else {
                matches!(p, RouterPort::CardOut { .. })
            }
        })
        .collect();
    if chosen.is_empty() {
        return Err("Tick at least one card channel.".into());
    }
    let have = if input {
        dev.tx_channels.len()
    } else {
        dev.rx_channels.len()
    };
    let total = have + chosen.len();
    let total16 = u16::try_from(total).map_err(|_| "Too many channels.".to_owned())?;
    let (rx, tx) = if input {
        (
            u16::try_from(dev.rx_channels.len()).unwrap_or(u16::MAX),
            total16,
        )
    } else {
        (
            total16,
            u16::try_from(dev.tx_channels.len()).unwrap_or(u16::MAX),
        )
    };
    let mut cmds = vec![Command::SetDeviceChannels {
        device: dev.key.clone(),
        rx,
        tx,
        dry_run: false,
    }];
    let mut names = Vec::new();
    for (i, p) in chosen.iter().enumerate() {
        let n = u16::try_from(have + i + 1).unwrap_or(u16::MAX);
        let Some(channel) = Channel::new(n) else {
            continue;
        };
        let label = field(form, &format!("label:{}", encode(p)))
            .trim()
            .to_owned();
        if !label.is_empty() {
            cmds.push(Command::SetChannelLabel {
                device: dev.key.clone(),
                direction: if input { Direction::Tx } else { Direction::Rx },
                channel,
                label: Some(label.clone()),
            });
        }
        names.push(format!(
            "{} \u{2192} {} {n}",
            ports::place(p),
            if input { "TX" } else { "RX" }
        ));
    }
    for (i, p) in chosen.iter().enumerate() {
        let n = u16::try_from(have + i + 1).unwrap_or(u16::MAX);
        let Some(channel) = Channel::new(n) else {
            continue;
        };
        if input {
            let tx = RouterPort::NetworkTx {
                device: dev.key.clone(),
                channel,
            };
            cmds.push(Command::SetCrosspoint {
                crosspoint: Crosspoint::new(p.clone(), tx.clone()),
            });
            cmds.push(Command::RemoveCrosspoint {
                source: RouterPort::AppOut {
                    device: dev.key.clone(),
                    channel,
                },
                destination: tx,
            });
        } else {
            let rx = RouterPort::NetworkRx {
                device: dev.key.clone(),
                channel,
            };
            cmds.push(Command::SetCrosspoint {
                crosspoint: Crosspoint::new(rx, p.clone()),
            });
        }
    }
    let msg = if input {
        format!("On the network now: {}.", names.join(", "))
    } else {
        format!(
            "Fed from the network now: {}. Route network audio to the new RX channels.",
            names.join(", ")
        )
    };
    Ok((cmds, msg))
}

/// `POST /router/patch`: "Patch to network" / "Feed from the network".
pub async fn patch(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, field(&form, "csrf")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let prepared = match status_of(&st).await {
        Ok(status) => patch_commands(&form, &status),
        Err(e) => Err(e),
    };
    match prepared {
        Err(m) => back(&st, &token, "/router#patch", Err(m)),
        Ok((cmds, msg)) => {
            let outcome = super::drive(&st, &s, cmds).await;
            super::settle(&st, &token, &s, &headers, "/router", |_| msg, outcome)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_router_pages_are_places_to_return_to() {
        for ok in [
            "/router",
            "/router#x-1",
            "/router?taps=1#p-a",
            "/router/crosspoint?x=card_in%2F1%2Fa",
            "/router/port?p=a%2F1",
        ] {
            assert_eq!(back_to(ok).as_deref(), Some(ok), "{ok}");
        }
        for bad in [
            "",
            "/routerx",
            "/router//x",
            "/router/other",
            "/router?x=<a>",
            "https://x/router",
            "/router/port?p=a b",
        ] {
            assert_eq!(back_to(bad), None, "{bad}");
        }
    }

    #[test]
    fn cell_gains_are_short() {
        assert_eq!(cell_gain(Gain::MilliDb(0)), "0");
        assert_eq!(cell_gain(Gain::MilliDb(-6000)), "\u{2212}6");
        assert_eq!(cell_gain(Gain::MilliDb(3500)), "+3.5");
        assert_eq!(cell_gain(Gain::Off), "off");
    }
}
