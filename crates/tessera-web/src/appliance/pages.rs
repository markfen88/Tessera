//! The Speaker page and its service form. Markup uses the component names of
//! the design system (`.card`, `.hero`, `.chip`, `.row`, `.switch`,
//! `.status`, `.icon`, `.kv`); the styles are in `style.css`, the frame
//! (top bar, flash, footer) is [`crate::pages::layout`].
//!
//! The EQ engine's parts (the Playing/Idle ring, the active configuration
//! chip, the signal boost form and the files card) and the Volume card are
//! rendered by [`crate::appliance::camilla`], [`crate::appliance::files`]
//! and [`crate::appliance::volume`]; this page only places them.

use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use maud::{Markup, html};
use serde::Deserialize;
use tessera_control_types::{ClockState, Status, SubscriptionState};

use crate::Shared;
use crate::appliance::camilla::{self, Snapshot};
use crate::appliance::host::{self, BLUETOOTH_UNIT, Bluetooth, HostInfo};
use crate::appliance::services::{self, Action, Row};
use crate::appliance::{files, volume};
use crate::auth::Session;
use crate::config::{ServiceEntry, is_own_unit};
use crate::pages::{Chrome, khz, layout, metrics_of, ms, need, need_post, status_of, view};

/// Where the service form sends the operator back.
const SERVICES_ANCHOR: &str = "/speaker#services";
/// The page refreshes this often (seconds).
const REFRESH: u32 = 5;

/// `GET /` in appliance mode.
pub async fn root() -> Response {
    Redirect::to("/speaker").into_response()
}

/// `GET /speaker`.
pub async fn speaker(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(a) = st.appliance.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let status = status_of(&st).await;
    let rows = services::query_all(&*a.runner, &a.config).await;
    let info = HostInfo::read();
    // PR C: the EQ engine (one websocket call, the state file and the two
    // directories), the audio session gauge and the amplifier's volume.
    let eq = if a.config.camilladsp.enabled {
        Some(Snapshot::read(&a.config.camilladsp).await)
    } else {
        None
    };
    let sessions = metrics_of(&st).await.map(|m| m.audio.sessions);
    let vol = match a.config.volume.as_ref() {
        Some(v) => Some(volume::read(&*a.runner, v).await),
        None => None,
    };
    // `bluetoothctl` blocks for seconds while the service is down: ask only
    // when the `bluetooth` unit itself (in any entry) is active.
    let bluetooth_up = rows
        .iter()
        .flat_map(|r| r.units.iter())
        .any(|u| u.unit == BLUETOOTH_UNIT && u.active);
    let bluetooth = if bluetooth_up {
        Some(host::bluetooth_connected(&*a.runner).await)
    } else {
        None
    };
    let flash = crate::pages::live_flash(&st, &token, &headers);
    let name = a
        .config
        .hostname_label
        .clone()
        .unwrap_or_else(|| info.hostname.clone());
    let name = if name.is_empty() {
        "tessera".into()
    } else {
        name
    };
    let body = html! {
        div.grid {
            (hero(&name, &status, &info, eq.as_ref(), sessions))
            @if let (Some(v), Some(cur)) = (a.config.volume.as_ref(), vol.as_ref()) {
                (volume::card(v, cur, &s.csrf))
            }
            (services_card(&rows, &s.csrf))
            (sources_card(&status, bluetooth.as_ref(), eq.as_ref(), &s.csrf))
            @if let Some(eq) = eq.as_ref() {
                (files::card(&a.config.camilladsp, &eq.active, &eq.configs, &eq.coeffs, &s.csrf))
            }
            (network_card(&status, a.config.camilladsp.gui_url.as_deref()))
        }
    };
    layout(
        &view("Speaker", "/speaker").every(REFRESH),
        &Chrome::of(&st, &headers),
        Some(&s),
        flash,
        &body,
    )
    .into_response()
}

fn hero(
    name: &str,
    status: &Result<Status, String>,
    info: &HostInfo,
    eq: Option<&Snapshot>,
    sessions: Option<u64>,
) -> Markup {
    let clock = status.as_ref().ok().map(|s| {
        if s.clocks.is_empty() {
            None
        } else {
            Some(
                s.clocks
                    .iter()
                    .any(|c| matches!(c.state, ClockState::Locked | ClockState::Leader)),
            )
        }
    });
    html! {
        section.card.c8 id="hero" {
            div.hero {
                div.state {
                    // The pulsing ring and Playing/Idle come from the EQ
                    // engine's state; without the engine the name stands.
                    @if let Some(eq) = eq {
                        span class=(eq.playback.ring_class()) title=(eq.playback.detail()) {}
                    }
                    div {
                        @match eq {
                            Some(eq) => { b { (eq.playback.label()) } }
                            None => { b { (name) } }
                        }
                        @match status {
                            Ok(s) => {
                                @if let Some(d) = s.devices.first() {
                                    span {
                                        "network audio · " (d.name) " · " (d.rx_channels.len()) " ch · "
                                        (khz(d.sample_rate.0)) " · " (d.encoding.0) " bit · " (ms(d.latency.0))
                                        @if s.devices.len() > 1 { " · +" (s.devices.len() - 1) " more" }
                                    }
                                } @else {
                                    span { "network audio · no device" }
                                }
                            }
                            Err(e) => { span.bad { (e) } }
                        }
                        @if eq.is_some() {
                            @if let Ok(s) = status {
                                @let (rec, total) = receiving(s);
                                span.sub {
                                    (name) " · " (rec) " of " (total) " receiving"
                                    @if let Some(n) = sessions { " · " (n) " audio session" @if n != 1 { "s" } }
                                }
                            }
                        }
                    }
                }
                div.chips {
                    @match clock {
                        Some(Some(true)) => { span.chip { span.dot {} "clock " b { "locked" } } }
                        Some(Some(false)) => { span.chip { span.dot.off {} "clock " b { "unlocked" } } }
                        Some(None) => { span.chip { span.dot.na {} "clock " b { "none" } } }
                        None => { span.chip { span.dot.na {} "daemon " b { "unreachable" } } }
                    }
                    @if let Some(t) = info.temperature_c {
                        span.chip { b { (host::format_temperature(t)) } }
                    }
                    @if let Some(u) = info.uptime {
                        span.chip { "up " b { (host::format_uptime(u)) } }
                    }
                    @if let Some(eq) = eq {
                        span.chip { "EQ " b { (eq.active_label()) } }
                    }
                }
            }
        }
    }
}

fn services_card(rows: &[Row], csrf: &str) -> Markup {
    html! {
        section.card.c6 id="services" {
            h2 { "Services " span.n { "enable keeps a service running across reboots" } }
            @if rows.is_empty() {
                p.muted { "No services are configured: add " code { "[[appliance.service]]" } " entries to " code { "web.toml" } "." }
            } @else {
                div.rows {
                    @for r in rows { (service_row(r, csrf)) }
                }
            }
        }
    }
}

fn service_row(r: &Row, csrf: &str) -> Markup {
    let e = &r.entry;
    let key = e.key();
    let on = r.on();
    let blocked = r.blocked();
    let (action, verb) = if on {
        (Action::Disable, "Disable")
    } else {
        (Action::Enable, "Enable")
    };
    html! {
        div.row data-live-key=(key) {
            @if r.static_only() {
                // A static unit has no boot state to switch.
                span.muted title="static: started by what needs it, no switch" { "static" }
            } @else {
                form method="post" action="/speaker/services" {
                    input type="hidden" name="csrf" value=(csrf);
                    input type="hidden" name="unit" value=(key);
                    input type="hidden" name="action" value=(action.name());
                    button type="submit"
                        class=(if on { "switch on" } else { "switch" })
                        role="switch"
                        aria-checked=(if on { "true" } else { "false" })
                        aria-label=(format!("{verb} {}", e.label))
                        title=[blocked.as_deref()]
                        disabled[blocked.is_some()] {}
                }
            }
            div.name { (e.label) small { (e.description) } }
            @if r.shows_status() {
                @if r.active() { span.status.on { "active" } } @else { span.status.off { "stopped" } }
            } @else if let Some(why) = &blocked {
                span.muted { (why) }
            } @else {
                span {}
            }
            @if r.restartable() {
                form method="post" action="/speaker/services" {
                    input type="hidden" name="csrf" value=(csrf);
                    input type="hidden" name="unit" value=(key);
                    input type="hidden" name="action" value="restart";
                    button.icon type="submit" title="Restart" aria-label=(format!("Restart {}", e.label)) { "↻" }
                }
            } @else {
                span {}
            }
        }
    }
}

/// Subscriptions receiving, and all of them, over every device.
fn receiving(s: &Status) -> (usize, usize) {
    let all = s.devices.iter().flat_map(|d| d.subscriptions.iter());
    let total = all.clone().count();
    let rec = all
        .filter(|x| matches!(x.state, SubscriptionState::Receiving))
        .count();
    (rec, total)
}

fn sources_card(
    status: &Result<Status, String>,
    bluetooth: Option<&Bluetooth>,
    eq: Option<&Snapshot>,
    csrf: &str,
) -> Markup {
    html! {
        section.card.c6 id="sources" {
            @if let Some(eq) = eq {
                (camilla::boost_form(&eq.active, csrf))
                h2.links { "Sources" }
            } @else {
                h2 { "Sources" }
            }
            dl.kv {
                @if let Some(bt) = bluetooth {
                    dt { "Bluetooth" }
                    dd {
                        @match bt {
                            Bluetooth::Unavailable => { "unavailable" }
                            Bluetooth::Connected(devs) if devs.is_empty() => { "nothing connected" }
                            Bluetooth::Connected(devs) => {
                                @for (i, d) in devs.iter().enumerate() {
                                    @if i > 0 { ", " }
                                    (d)
                                }
                                " · connected"
                            }
                        }
                    }
                }
                @if let Ok(s) = status {
                    @for d in &s.devices {
                        @let rec = d.subscriptions.iter().filter(|x| matches!(x.state, SubscriptionState::Receiving)).count();
                        dt { "Network audio" }
                        dd { (rec) " of " (d.subscriptions.len()) " receiving" }
                    }
                }
            }
        }
    }
}

fn network_card(status: &Result<Status, String>, gui_url: Option<&str>) -> Markup {
    html! {
        section.card.c4 id="network" {
            h2 { "Network" }
            @match status {
                Ok(s) => {
                    @if let Some(n) = &s.network {
                        dl.kv {
                            @for i in &n.interfaces {
                                dt { (i.name) }
                                dd {
                                    @if let Some(ip) = &i.ipv4 { (ip) }
                                    @else if i.carrier { "no address" }
                                    @else { "down" }
                                    @if i.wireless { " · Wi-Fi" }
                                }
                            }
                            dt { "primary" }
                            dd { (n.primary.clone().unwrap_or_else(|| "none".into())) }
                            dt { "PTP leader" }
                            dd { (ptp_leader(s)) }
                        }
                    } @else {
                        p.muted { "Not reported yet." }
                    }
                }
                Err(_) => { p.muted { "The daemon is not reachable." } }
            }
            h2.links { "Links" }
            div.actions {
                @if let Some(u) = gui_url { a.btn.sm href=(u) { "CamillaDSP GUI" } }
                a.btn.sm href="/device" { "Device page" }
            }
        }
    }
}

fn ptp_leader(s: &Status) -> String {
    if let Some(c) = s.clocks.iter().find(|c| c.grandmaster.is_some()) {
        return c.grandmaster.clone().unwrap_or_default();
    }
    if s.clocks.iter().any(|c| c.state == ClockState::Leader) {
        return "this device".into();
    }
    if s.clocks.is_empty() {
        "no clock".into()
    } else {
        "not yet known".into()
    }
}

// ---- the service form -------------------------------------------------------------

/// The service form. Missing fields are refused after the token check (403
/// without a valid token, then 400).
#[derive(Debug, Deserialize)]
pub struct ServiceForm {
    #[serde(default)]
    csrf: String,
    /// [`ServiceEntry::key`] of the row.
    #[serde(default)]
    unit: String,
    /// `enable`, `disable` or `restart`.
    #[serde(default)]
    action: String,
    /// `yes` from the confirmation page of an essential entry.
    #[serde(default)]
    confirm: String,
}

/// `POST /speaker/services`: one action on one allowlisted entry, then a
/// redirect to the Services card with a message. Disabling or restarting an
/// essential entry first shows a confirmation page whose form carries
/// `confirm=yes` (and the session's CSRF token again); nothing runs before
/// that.
pub async fn services(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<ServiceForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(a) = st.appliance.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // The allowlist decides before anything is run. The refused value is
    // untrusted input: logged quoted, never raw.
    let Some(entry) = services::allowlisted(&a.config, &f.unit) else {
        tracing::warn!(session = %s.id, unit = ?f.unit, "appliance: refused a unit outside the allowlist");
        return (StatusCode::BAD_REQUEST, "unknown service").into_response();
    };
    let Some(action) = Action::parse(&f.action) else {
        tracing::warn!(session = %s.id, unit = %entry.key(), action = ?f.action, "appliance: refused an unknown action");
        return (StatusCode::BAD_REQUEST, "unknown action").into_response();
    };
    if entry.essential && action.asks_when_essential() && f.confirm != "yes" {
        return confirm_page(&st, &headers, &s, entry, action);
    }
    // The current state decides which units of a group a switch touches
    // (static ones are left alone); one more read, no privilege.
    let row = if action == Action::Restart {
        None
    } else {
        services::query_all(&*a.runner, &a.config)
            .await
            .into_iter()
            .find(|r| r.entry.key() == entry.key())
    };
    let r = services::perform(&*a.runner, entry, action, row.as_ref()).await;
    tracing::info!(
        session = %s.id,
        unit = %entry.key(),
        action = action.name(),
        ok = r.is_ok(),
        result = r.as_ref().err().map_or("ok", String::as_str),
        "appliance service action"
    );
    // Stopping the portal itself is not waited for: its outcome is not
    // known here (and this message is only seen if the portal comes back
    // before the browser reloads).
    let own = entry.units.iter().any(|u| action.stops_own_unit(u));
    match r {
        Ok(()) if own => st.set_flash(
            &token,
            true,
            format!(
                "{} {} requested; the portal does not wait for its own unit, so the outcome is not known.",
                entry.label,
                action.name()
            ),
        ),
        Ok(()) => st.set_flash(&token, true, format!("{} {}.", entry.label, action.done())),
        Err(m) => st.set_flash(&token, false, m),
    }
    Redirect::to(SERVICES_ANCHOR).into_response()
}

/// The confirmation page for disabling or restarting an essential entry.
fn confirm_page(
    st: &Shared,
    headers: &HeaderMap,
    s: &Session,
    e: &ServiceEntry,
    action: Action,
) -> Response {
    let (gerund, does, button) = match action {
        Action::Disable => (
            "Disabling",
            "stops it now and keeps it stopped after a reboot",
            "Disable",
        ),
        Action::Restart => ("Restarting", "interrupts it for a moment", "Restart"),
        Action::Enable => ("Enabling", "starts it", "Enable"),
    };
    let body = html! {
        section.card {
            p.flash.bad {
                strong { (gerund) " " (e.label) " " (does) "." }
                " " (essential_warning(e, action))
            }
            p { "Units: " code { (e.canonical_units().join(" ")) } }
            form method="post" action="/speaker/services" {
                input type="hidden" name="csrf" value=(s.csrf);
                input type="hidden" name="unit" value=(e.key());
                input type="hidden" name="action" value=(action.name());
                input type="hidden" name="confirm" value="yes";
                button.btn.danger type="submit" { (button) " " (e.label) }
                " "
                a href=(SERVICES_ANCHOR) { "Cancel" }
            }
            p.muted { "Cancel changes nothing." }
        }
    };
    (
        StatusCode::OK,
        layout(
            &view("Confirm the change", "/speaker"),
            &Chrome::of(st, headers),
            Some(s),
            None,
            &body,
        ),
    )
        .into_response()
}

fn essential_warning(e: &ServiceEntry, action: Action) -> &'static str {
    let own = e.units.iter().any(|u| is_own_unit(u));
    let daemon = e.has_unit("tesserad");
    match (own, daemon, action) {
        (true, _, Action::Disable) => {
            "That is this portal: the page you are on goes away, and only a shell on the device can bring it back."
        }
        (true, _, _) => {
            "That is this portal: the page you are on is interrupted and comes back without its message once the portal is up again."
        }
        (false, true, Action::Disable) => {
            "That is the network audio daemon: every page here loses its data and audio stops."
        }
        (false, true, _) => {
            "That is the network audio daemon: audio stops until it is back, and the pages lose their data meanwhile."
        }
        _ => "It is marked essential in the configuration.",
    }
}
