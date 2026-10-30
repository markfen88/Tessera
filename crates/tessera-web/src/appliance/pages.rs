//! The Speaker page and its service form. Markup uses the component names of
//! the design system (`.card`, `.hero`, `.chip`, `.row`, `.switch`,
//! `.status`, `.icon`, `.kv`, `.pane`, `.note`); the styles are in
//! `style.css`, the frame (top bar, flash, footer) is [`crate::pages::layout`].
//!
//! The page is the device's control panel: the master card (volume and
//! signal boost) first, then the capture-rate notice (only while the EQ
//! engine cannot open the Tessera device) and the daemon's warnings, the
//! hero, the clock, the services, the sources, the signal path and the
//! devices of the overview, the EQ files and the network. The page's script (`live.js`) posts the sliders and
//! the service buttons in place and shows the result in each row's or
//! form's `.note`; the same forms work without it (Apply buttons, a
//! redirect and a one-shot message).
//!
//! The EQ engine's parts (the Playing/Idle ring, the active configuration
//! chip, the signal boost pane and the files card) and the Volume pane are
//! rendered by [`crate::appliance::camilla`], [`crate::appliance::files`]
//! and [`crate::appliance::volume`]; this page only places them.

use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use maud::{Markup, html};
use serde::Deserialize;
use tessera_control_types::{ClockState, Metrics, Status, SubscriptionState};
use tesseractl::output;

use crate::Shared;
use crate::appliance::camilla::{self, Snapshot};
use crate::appliance::capture_rate::{self, CaptureRate, FormatNotice};
use crate::appliance::host::{self, BLUETOOTH_UNIT, Bluetooth, HostInfo};
use crate::appliance::services::{self, Action, Row};
use crate::appliance::{Appliance, files, rate_follow, volume};
use crate::auth::Session;
use crate::config::{ServiceEntry, is_own_unit};
use crate::pages::{
    Chrome, devices_table, flow_activity, is_live, khz, layout, live_answer, ms, need, need_post,
    status_of, sub_class, view,
};
use crate::signal::{self, Activity};

/// Where the service form sends the operator back.
const SERVICES_ANCHOR: &str = "/speaker#services";
/// The page refreshes this often (seconds).
const REFRESH: u32 = 5;

/// `GET /` in appliance mode.
pub async fn root() -> Response {
    Redirect::to("/speaker").into_response()
}

/// `GET /services`: core services are visible without enabling Speaker mode.
pub async fn services_page(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let a = st.appliance.as_ref().unwrap_or(&st.service_manager);
    let rows = services::query_all(&*a.runner, &a.config).await;
    let body = html! { div.grid { (services_card(&rows, &s.csrf, "/services")) } };
    layout(
        &view("Services", "/services").every(REFRESH),
        &Chrome::of(&st, &headers),
        Some(&s),
        crate::pages::live_flash(&st, &token, &headers),
        &body,
    )
    .into_response()
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
    // The EQ engine (one websocket call, the state file and the two
    // directories), the flow counters (the signal path and the audio
    // session gauge) and the amplifier's volume. Capture is re-checked
    // against the current status each time.
    let eq = if a.config.camilladsp.enabled {
        Some(Snapshot::read(&a.config.camilladsp).await)
    } else {
        None
    };
    let capture_rate = match eq.as_ref() {
        Some(eq) => Some(capture_rate::read(&eq.active, &status, eq.stop_reason.as_deref()).await),
        None => None,
    };
    let format_notice = match eq.as_ref() {
        Some(eq) => capture_rate::format_notice(&eq.active, eq.format_failure.as_ref()).await,
        None => None,
    };
    let (metrics, act) = flow_activity(&st).await;
    let sessions = metrics.as_ref().map(|m| m.audio.sessions);
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
    // The EQ engine's GUI: `gui_url` with `{host}` as the browser reached
    // the portal (else the device's host name; else no button).
    let gui = a.config.camilladsp.gui_url.as_deref().and_then(|t| {
        camilla::gui_link(
            t,
            camilla::request_host(&headers)
                .as_deref()
                .or(Some(info.hostname.as_str())),
        )
    });
    let body = html! {
        div.grid {
            (master_card(a, vol.as_ref(), eq.as_ref(), gui.as_deref(), &s.csrf))
            @if let (Some(check), Some(eq)) = (capture_rate.as_ref(), eq.as_ref()) {
                (capture_rate_card(check, eq.active_label(), &s.csrf, a.config.rate_follow.enabled))
            }
            @if let (Some(notice), Some(eq)) = (format_notice.as_ref(), eq.as_ref()) {
                (format_card(notice, eq.active_label()))
            }
            @if let Ok(st) = &status {
                @if !st.warnings.is_empty() { (warnings_card(st)) }
            }
            (hero(&name, &status, &info, eq.as_ref(), sessions, gui.as_deref()))
            (clock_card(&status))
            (services_card(&rows, &s.csrf, "/speaker/services"))
            (sources_card(&status, bluetooth.as_ref()))
            (signal_card(&status, metrics.as_ref(), act))
            (devices_card(&status))
            @if let Some(eq) = eq.as_ref() {
                (files::card(&a.config.camilladsp, &eq.active, &eq.configs, &eq.coeffs, &s.csrf))
            }
            (network_card(&status, gui.as_deref()))
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

/// The capture-rate notice. While the rate follower runs (`follower`) it
/// offers no fix of its own: the follower sets the rates within seconds, and
/// two writers of the same keys must not fight.
fn capture_rate_card(
    check: &CaptureRate,
    config_label: &str,
    csrf: &str,
    follower: bool,
) -> Markup {
    let CaptureRate::Mismatch {
        device_name,
        device_rate,
        capture_rate,
        from_samplerate,
        samplerate,
        fixable,
        stop_reason,
    } = check
    else {
        return html! {};
    };
    html! {
        section.card id="capture-rate" {
            h2 { "EQ engine input " span.n { "rate mismatch" } }
            div.flash.bad role="alert" {
                "The EQ engine opens the network audio device at " b { (khz(*capture_rate)) } ", but the Tessera device "
                b { (device_name) } " runs at " b { (khz(*device_rate)) } ". The device accepts only its own rate, so the EQ engine "
                "cannot start and the speaker stays silent while network audio is still received."
            }
            @if let Some(r) = stop_reason { p.sub { "The EQ engine reported: " code { (r) } } }
            dl.kv {
                dt { "Tessera device" } dd { (device_name) " · " (khz(*device_rate)) }
                dt { "EQ capture rate" } dd { (khz(*capture_rate)) " · " @if *from_samplerate { "from samplerate" } @else { "capture_samplerate" } }
                dt { "EQ processing rate" } dd { (khz(*samplerate)) }
                dt { "Configuration" } dd { (config_label) }
            }
            @if follower {
                p.sub { "The rate follower sets the EQ engine's rates to the device's ("
                    code { "[appliance.rate_follow]" } "); this notice goes when it has." }
            } @else if *fixable {
                form method="post" action="/speaker/capture-rate" {
                    input type="hidden" name="csrf" value=(csrf);
                    button.btn.primary type="submit" { "Set capture rate to " (khz(*device_rate)) " and reload the EQ engine" }
                }
                p.sub { "Only " code { "capture_samplerate" } " changes. The file keeps its comments and order, and the EQ engine keeps processing at " (khz(*samplerate)) "." }
            } @else {
                p.muted { "This configuration has no resampler, so the EQ engine must also process at the device's rate. Set "
                    code { "samplerate: " (device_rate) } " (check that coefficient files match that rate), or add an AsyncSinc resampler and set "
                    code { "capture_samplerate: " (device_rate) } ". See the Raspberry Pi speaker guide." }
            }
        }
    }
}

/// The notice for a sample format the sound card (or the network audio
/// device) refuses. No fix to offer: the format is a choice of the
/// configuration, so the notice says where to change it.
fn format_card(notice: &FormatNotice, config_label: &str) -> Markup {
    let FormatNotice {
        failure,
        device,
        format,
    } = notice;
    let (id, title, side, who) = if failure.playback {
        (
            "playback-format",
            "EQ engine output",
            "playback",
            "sound card",
        )
    } else {
        (
            "capture-format",
            "EQ engine input",
            "capture",
            "Tessera device",
        )
    };
    html! {
        section.card id=(id) {
            h2 { (title) " " span.n { "format not supported" } }
            div.flash.bad role="alert" {
                "The EQ engine opens the " (side) " device"
                @if let Some(d) = device { " " b { (d) } }
                @if let Some(f) = format { " with the sample format " b { (f) } }
                ", but the " (who) " does not accept that format, so the EQ engine cannot start and the speaker stays silent"
                @if failure.playback { " while network audio is still received" }
                "."
            }
            p.sub { "The EQ engine reported: " code { (failure.reason) } }
            dl.kv {
                @if let Some(d) = device { dt { "Device" } dd { (d) } }
                @if let Some(f) = format { dt { "Format" } dd { (f) } }
                dt { "Configuration" } dd { (config_label) }
            }
            p.muted { "Set " code { "format" } " under " code { "devices" } ", " code { (side) } " in the EQ engine's GUI to one the "
                (who) " accepts. "
                @if failure.playback {
                    code { "S24_3_LE" } ", " code { "S24_4_LE" } " and " code { "S16_LE" }
                    " are the usual alternatives to " code { "S32_LE" } "; many USB DACs offer only " code { "S16_LE" }
                    " and " code { "S24_3_LE" } ". The EQ engine converts internally, so only the output format has to match the card."
                } @else {
                    "It offers " code { "S16_LE" } ", " code { "S24_4_LE" } ", " code { "S24_3_LE" } ", " code { "S32_LE" }
                    " and " code { "F32_LE" } " at its own rate (the EQ engine's names)."
                }
            }
        }
    }
}

/// The master card at the top: the Volume pane (with `[appliance.volume]`)
/// and the signal boost pane (with the EQ engine), side by side, under them
/// the rate the speaker follows (with `[appliance.rate_follow]`) and the
/// button to the engine's GUI (with `gui_url`). Nothing when none is
/// configured.
fn master_card(
    a: &Appliance,
    vol: Option<&Result<volume::Level, String>>,
    eq: Option<&Snapshot>,
    gui: Option<&str>,
    csrf: &str,
) -> Markup {
    let volume = a.config.volume.as_ref().zip(vol);
    let follow = a.config.rate_follow.enabled.then(|| {
        a.rate_follow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    });
    if volume.is_none() && eq.is_none() && gui.is_none() && follow.is_none() {
        return html! {};
    }
    html! {
        section.card.master id="master" {
            @if volume.is_some() || eq.is_some() {
                div.panes {
                    @if let Some((cfg, cur)) = volume { (volume::pane(cfg, cur, csrf)) }
                    @if let Some(eq) = eq { (camilla::boost_pane(&eq.active, csrf)) }
                }
            }
            @if let Some(r) = &follow { (rate_follow_line(r)) }
            @if let Some(url) = gui {
                div.actions.manage {
                    (camilla::gui_button(url, eq.map(|e| &e.playback), false))
                    span.muted { "filters, pipeline and devices of the EQ engine, in its own GUI" }
                }
            }
        }
    }
}

/// The rate follower's line: the rate the speaker follows and, while there
/// is one, the follower's last problem.
fn rate_follow_line(r: &rate_follow::Report) -> Markup {
    html! {
        div id="rate-follow" {
            p.sub {
                "Sample rate follows the network device: "
                @match r.rate {
                    Some(hz) => { b { (hz) " Hz" } }
                    None => { "not known yet" }
                }
            }
            @if let Some(e) = &r.error { p.flash.bad role="status" { (e) } }
        }
    }
}

fn warnings_card(s: &Status) -> Markup {
    html! {
        section.card.warnings id="warnings" {
            h2 { "Warnings" }
            ul { @for w in &s.warnings { li { (w) } } }
        }
    }
}

fn hero(
    name: &str,
    status: &Result<Status, String>,
    info: &HostInfo,
    eq: Option<&Snapshot>,
    sessions: Option<u64>,
    gui: Option<&str>,
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
                    @if let Some(url) = gui {
                        (camilla::gui_button(url, eq.map(|e| &e.playback), true))
                    }
                }
            }
        }
    }
}

/// The clock, compactly: role and priority, then one line per clock.
fn clock_card(status: &Result<Status, String>) -> Markup {
    html! {
        section.card.c4 id="clock" {
            h2 { "Clock" }
            @match status {
                Ok(s) => {
                    dl.kv {
                        @if let Some(c) = &s.clock_settings {
                            dt { "Role" } dd { (format!("{:?}", c.role).to_lowercase()) }
                            dt { "Priority" } dd { (c.priority) }
                        }
                        @for c in &s.clocks {
                            dt { (c.interface) }
                            dd {
                                span class=(clock_class(&c.state)) { (format!("{:?}", c.state).to_lowercase()) }
                                @if let Some(o) = c.offset_ns { " · " (o) " ns" }
                                @if let Some(g) = &c.grandmaster { " · leader " (g) }
                            }
                        }
                    }
                    @if s.clocks.is_empty() { p.muted { "No clock is running." } }
                    p { a href="/clock" { "Clock settings" } }
                }
                Err(_) => { p.muted { "The daemon is not reachable." } }
            }
        }
    }
}

fn clock_class(s: &ClockState) -> &'static str {
    if matches!(s, ClockState::Locked | ClockState::Leader) {
        "ok"
    } else {
        "warn"
    }
}

/// The overview's signal path, live like the rest of the page.
fn signal_card(status: &Result<Status, String>, m: Option<&Metrics>, act: Activity) -> Markup {
    html! {
        section.card id="signal" {
            h2 { "Signal path " span.n { "highlighted while packets move" } }
            @match status {
                Ok(s) => { (signal::render(s, m, act)) }
                Err(_) => { p.muted { "The daemon is not reachable." } }
            }
        }
    }
}

/// The overview's devices table and, under it, every subscription with
/// its state.
fn devices_card(status: &Result<Status, String>) -> Markup {
    html! {
        section.card id="devices" {
            h2 { "Devices " @if let Ok(s) = status { span.n { "tessera " (s.version) } } }
            @match status {
                Ok(s) if s.devices.is_empty() => { p.muted { "No device." } }
                Ok(s) => {
                    (devices_table(s))
                    @let subs: Vec<_> = s.devices.iter().flat_map(|d| d.subscriptions.iter().map(move |x| (d, x))).collect();
                    @if subs.is_empty() {
                        p.muted { "No subscriptions." }
                    } @else {
                        dl.kv {
                            @for (d, x) in &subs {
                                dt { (d.name) " ch " (x.rx_channel.get()) }
                                dd {
                                    (x.tx_channel) " @ " (x.tx_device) " · "
                                    span class=(sub_class(&x.state)) { (output::sub_state(&x.state)) }
                                }
                            }
                        }
                    }
                    p { a href="/devices" { "Device settings and subscriptions" } }
                }
                Err(e) => { p.muted { (e) } }
            }
        }
    }
}

fn services_card(rows: &[Row], csrf: &str, target: &str) -> Markup {
    html! {
        section.card.c6 id="services" {
            h2 { "Services " span.n { "enable keeps a service running across reboots" } }
            @if rows.is_empty() {
                p.muted { "No services are configured: add " code { "[[appliance.service]]" } " entries to " code { "web.toml" } "." }
            } @else {
                div.rows {
                    @for r in rows { (service_row(r, csrf, target)) }
                }
            }
        }
    }
}

/// One row. An essential entry's disable and restart forms carry the
/// confirmation text in `data-confirm`, so the script asks in the row
/// before it posts (`confirm=yes`); without the script the server shows
/// the confirmation page, and the server asks in both cases.
fn service_row(r: &Row, csrf: &str, target: &str) -> Markup {
    let e = &r.entry;
    let key = e.key();
    let on = r.on();
    let blocked = r.blocked();
    let (action, verb) = if on {
        (Action::Disable, "Disable")
    } else {
        (Action::Enable, "Enable")
    };
    let asks = |action: Action| -> Option<String> {
        (e.essential && action.asks_when_essential()).then(|| confirm_text(e, action))
    };
    html! {
        div.row data-live-key=(key) {
            @if r.static_only() {
                // A static unit has no boot state to switch.
                span.muted title="static: started by what needs it, no switch" { "static" }
            } @else {
                form method="post" action=(target) data-confirm=[asks(action)] {
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
                form method="post" action=(target) data-confirm=[asks(Action::Restart)] {
                    input type="hidden" name="csrf" value=(csrf);
                    input type="hidden" name="unit" value=(key);
                    input type="hidden" name="action" value="restart";
                    button.icon type="submit" title="Restart" aria-label=(format!("Restart {}", e.label)) { "↻" }
                }
            } @else {
                span {}
            }
            // The script's result line (and its confirmation); empty
            // otherwise and never touched by the background refresh.
            span.note role="status" aria-live="polite" data-live-local {}
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

fn sources_card(status: &Result<Status, String>, bluetooth: Option<&Bluetooth>) -> Markup {
    html! {
        section.card.c6 id="sources" {
            h2 { "Sources" }
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
                @if let Some(u) = gui_url { a.btn.sm href=(u) target="_blank" rel="noopener noreferrer" { "EQ engine (CamillaDSP)" } }
                a.btn.sm href="/device" { "Overview page" }
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
/// that. Posted in place (`X-Tessera-Live`) the answer is JSON instead:
/// the result, or 409 with `confirm: true` for an essential entry asked
/// without `confirm=yes`.
pub async fn services(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<ServiceForm>,
) -> Response {
    service_action(st, headers, f, false).await
}

/// `POST /services`: the same allowlist, authentication and confirmations.
pub async fn services_post(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<ServiceForm>,
) -> Response {
    service_action(st, headers, f, true).await
}

async fn service_action(
    st: Shared,
    headers: HeaderMap,
    f: ServiceForm,
    standalone: bool,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let a = if standalone {
        st.appliance.as_ref().unwrap_or(&st.service_manager)
    } else if let Some(a) = st.appliance.as_ref() {
        a
    } else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let target = if standalone {
        "/services"
    } else {
        "/speaker/services"
    };
    let anchor = if standalone {
        "/services#services"
    } else {
        SERVICES_ANCHOR
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
    let live = is_live(&headers);
    if entry.essential && action.asks_when_essential() && f.confirm != "yes" {
        // The script asks in the row before posting; a post without the
        // answer is refused the same way, whatever asked.
        if live {
            return live_answer(
                StatusCode::CONFLICT,
                false,
                &confirm_text(entry, action),
                serde_json::json!({ "confirm": true }),
            );
        }
        return confirm_page(&st, &headers, &s, entry, action, target, anchor);
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
    let (ok, text) = match r {
        Ok(()) if own => (
            true,
            format!(
                "{} {} requested; the portal does not wait for its own unit, so the outcome is not known.",
                entry.label,
                action.name()
            ),
        ),
        Ok(()) => (true, format!("{} {}.", entry.label, action.done())),
        Err(m) => (false, m),
    };
    if live {
        return live_answer(
            StatusCode::OK,
            ok,
            &text,
            serde_json::json!({ "unit": entry.key(), "action": action.name() }),
        );
    }
    st.set_flash(&token, ok, text);
    Redirect::to(anchor).into_response()
}

/// The words of a confirmation: the gerund, what the action does and the
/// button's verb.
fn confirm_words(action: Action) -> (&'static str, &'static str, &'static str) {
    match action {
        Action::Disable => (
            "Disabling",
            "stops it now and keeps it stopped after a reboot",
            "Disable",
        ),
        Action::Restart => ("Restarting", "interrupts it for a moment", "Restart"),
        Action::Enable => ("Enabling", "starts it", "Enable"),
    }
}

/// The confirmation text for `action` on the essential entry `e`, as the
/// confirmation page and the row's `data-confirm` say it.
#[must_use]
pub fn confirm_text(e: &ServiceEntry, action: Action) -> String {
    let (gerund, does, _) = confirm_words(action);
    format!(
        "{gerund} {} {does}. {}",
        e.label,
        essential_warning(e, action)
    )
}

/// The confirmation page for disabling or restarting an essential entry.
fn confirm_page(
    st: &Shared,
    headers: &HeaderMap,
    s: &Session,
    e: &ServiceEntry,
    action: Action,
    target: &str,
    anchor: &str,
) -> Response {
    let (gerund, does, button) = confirm_words(action);
    let body = html! {
        section.card {
            p.flash.bad {
                strong { (gerund) " " (e.label) " " (does) "." }
                " " (essential_warning(e, action))
            }
            p { "Units: " code { (e.canonical_units().join(" ")) } }
            form method="post" action=(target) {
                input type="hidden" name="csrf" value=(s.csrf);
                input type="hidden" name="unit" value=(e.key());
                input type="hidden" name="action" value=(action.name());
                input type="hidden" name="confirm" value="yes";
                button.btn.danger type="submit" { (button) " " (e.label) }
                " "
                a href=(anchor) { "Cancel" }
            }
            p.muted { "Cancel changes nothing." }
        }
    };
    (
        StatusCode::OK,
        layout(
            &view(
                "Confirm the change",
                if target == "/services" {
                    "/services"
                } else {
                    "/speaker"
                },
            ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appliance::services::{
        FakeRunner, HelperVerb, Output, SYSTEMCTL, argv, helper_unit,
    };
    use crate::appliance::testing::{form_headers, state};
    use crate::config::ApplianceConfig;
    use axum::body::to_bytes;

    fn essential(label: &str, unit: &str) -> ServiceEntry {
        ServiceEntry {
            label: label.into(),
            description: unit.into(),
            units: vec![unit.into()],
            essential: true,
        }
    }

    fn cfg() -> ApplianceConfig {
        ApplianceConfig {
            enabled: true,
            services: vec![essential("Device discovery", "avahi-daemon")],
            ..ApplianceConfig::default()
        }
    }

    fn form(csrf: &str, action: &str, confirm: &str) -> ServiceForm {
        ServiceForm {
            csrf: csrf.into(),
            unit: "avahi-daemon".into(),
            action: action.into(),
            confirm: confirm.into(),
        }
    }

    async fn body_json(r: Response) -> serde_json::Value {
        let b = to_bytes(r.into_body(), 1 << 16).await.unwrap();
        serde_json::from_slice(&b).unwrap()
    }

    #[test]
    fn confirmation_text_names_the_entry_and_the_reason() {
        let e = essential("Device discovery", "avahi-daemon");
        assert_eq!(
            confirm_text(&e, Action::Restart),
            "Restarting Device discovery interrupts it for a moment. It is marked essential in the configuration."
        );
        let d = essential("Network audio", "tesserad");
        assert!(confirm_text(&d, Action::Disable).starts_with(
            "Disabling Network audio stops it now and keeps it stopped after a reboot. That is the network audio daemon"
        ));
        let w = essential("This portal", "tessera-web");
        assert!(confirm_text(&w, Action::Restart).contains("That is this portal"));
    }

    #[tokio::test]
    async fn a_live_post_needs_the_session_and_the_token_and_asks_before_acting() {
        let runner = std::sync::Arc::new(FakeRunner::default());
        let (st, token, s) = state(cfg(), runner.clone());
        // No session: to the login page, nothing run.
        let r = services(
            State(st.clone()),
            form_headers(None, true),
            Form(form(&s.csrf, "restart", "")),
        )
        .await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(r.headers()["location"], "/login");
        // A wrong token: refused.
        let r = services(
            State(st.clone()),
            form_headers(Some(&token), true),
            Form(form("not-the-token", "restart", "")),
        )
        .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        // Essential, asked in place without the answer: 409 and the text to
        // ask with; still nothing run.
        let r = services(
            State(st.clone()),
            form_headers(Some(&token), true),
            Form(form(&s.csrf, "restart", "")),
        )
        .await;
        assert_eq!(r.status(), StatusCode::CONFLICT);
        let j = body_json(r).await;
        assert_eq!(j["ok"], false);
        assert_eq!(j["confirm"], true);
        assert!(
            j["text"]
                .as_str()
                .unwrap()
                .starts_with("Restarting Device discovery")
        );
        assert!(runner.calls().is_empty());
        // The same without the live header is the confirmation page.
        let r = services(
            State(st.clone()),
            form_headers(Some(&token), false),
            Form(form(&s.csrf, "restart", "")),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()["content-type"], "text/html; charset=utf-8");
        assert!(runner.calls().is_empty());
        // Confirmed: the helper is started and the JSON says what was done.
        let helper = helper_unit(HelperVerb::Restart, "avahi-daemon.service");
        runner.script(&[SYSTEMCTL, "start", &helper], Output::ok(""));
        let r = services(
            State(st.clone()),
            form_headers(Some(&token), true),
            Form(form(&s.csrf, "restart", "yes")),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let j = body_json(r).await;
        assert_eq!(j["ok"], true);
        assert_eq!(j["text"], "Device discovery restarted.");
        assert_eq!(j["unit"], "avahi-daemon");
        assert_eq!(runner.calls(), [argv(&[SYSTEMCTL, "start", &helper])]);
        // In place, nothing is left for a redirect to show.
        assert!(st.take_flash(&token).is_none());
    }

    #[test]
    fn format_card_names_the_device_and_format_and_offers_no_button() {
        use crate::appliance::capture_rate::FormatFailure;
        let notice = |playback, device: Option<&str>, format: Option<&str>| FormatNotice {
            failure: FormatFailure {
                playback,
                reason: "Playback error: 'snd_pcm_hw_params_set_format' <Invalid argument (22)>"
                    .into(),
            },
            device: device.map(str::to_owned),
            format: format.map(str::to_owned),
        };
        let out = format_card(
            &notice(true, Some("hw:CARD=DAC,DEV=0"), Some("S32_LE")),
            "speaker.yml",
        )
        .into_string();
        assert!(out.contains("id=\"playback-format\""), "{out}");
        assert!(out.contains("role=\"alert\""));
        assert!(out.contains("EQ engine output"));
        assert!(out.contains("<b>hw:CARD=DAC,DEV=0</b>") && out.contains("<b>S32_LE</b>"));
        assert!(out.contains("the sound card does not accept that format"));
        assert!(out.contains("while network audio is still received"));
        assert!(
            out.contains("&lt;Invalid argument (22)&gt;"),
            "escaped: {out}"
        );
        assert!(out.contains("speaker.yml") && out.contains("S16_LE"));
        assert!(
            !out.contains("<form") && !out.contains("<button"),
            "no fix to offer"
        );
        // The capture side, and a configuration that could not be read.
        let out = format_card(&notice(false, None, None), "speaker.yml").into_string();
        assert!(out.contains("id=\"capture-format\"") && out.contains("EQ engine input"));
        assert!(out.contains("the Tessera device does not accept that format"));
        assert!(
            !out.contains("<b>")
                && !out.contains("<dt>Device</dt>")
                && !out.contains("<dt>Format</dt>"),
            "nothing to name: {out}"
        );
        assert!(out.contains("F32_LE") && !out.contains("USB DACs"));
        assert!(!out.contains("while network audio"));
    }

    #[test]
    fn capture_rate_card_only_shows_a_mismatch_and_offers_only_safe_fixes() {
        for check in [CaptureRate::Ok, CaptureRate::Unchecked("not YAML".into())] {
            assert!(
                capture_rate_card(&check, "speaker.yml", "token", false)
                    .into_string()
                    .is_empty()
            );
        }
        for fixable in [true, false] {
            let check = CaptureRate::Mismatch {
                device_name: "Kitchen <speaker>".into(),
                device_rate: 48_000,
                capture_rate: 96_000,
                from_samplerate: true,
                samplerate: 96_000,
                fixable,
                stop_reason: Some("Capture error <detail>".into()),
            };
            let out = capture_rate_card(&check, "speaker.yml", "token", false).into_string();
            assert!(out.contains("id=\"capture-rate\""));
            assert!(out.contains("role=\"alert\""));
            assert!(out.contains("Kitchen &lt;speaker&gt;"));
            assert!(out.contains("48 kHz") && out.contains("96 kHz"));
            assert!(out.contains("from samplerate"));
            assert!(out.contains("speaker.yml"));
            assert!(out.contains("Capture error &lt;detail&gt;"));
            assert_eq!(out.contains("<form"), fixable);
            assert_eq!(out.contains("action=\"/speaker/capture-rate\""), fixable);
            assert_eq!(out.contains("name=\"csrf\" value=\"token\""), fixable);
            assert_eq!(out.contains("coefficient files"), !fixable);
        }
        let check = CaptureRate::Mismatch {
            device_name: "Kitchen".into(),
            device_rate: 48_000,
            capture_rate: 44_100,
            from_samplerate: false,
            samplerate: 48_000,
            fixable: true,
            stop_reason: None,
        };
        let out = capture_rate_card(&check, "speaker.yml", "token", false).into_string();
        assert!(out.contains("44.1 kHz · capture_samplerate"));
        assert!(!out.contains("from samplerate"));
        assert!(!out.contains("The EQ engine reported:"));
        // With the rate follower the notice stays, the fix does not: the
        // follower owns those keys.
        let out = capture_rate_card(&check, "speaker.yml", "token", true).into_string();
        assert!(out.contains("id=\"capture-rate\"") && out.contains("44.1 kHz"));
        assert!(!out.contains("<form") && !out.contains("/speaker/capture-rate"));
        assert!(out.contains("[appliance.rate_follow]"), "{out}");
    }

    #[test]
    fn the_rate_follow_line_names_the_rate_and_the_last_problem() {
        let out = rate_follow_line(&rate_follow::Report {
            rate: Some(96_000),
            error: None,
        })
        .into_string();
        assert!(
            out.contains("Sample rate follows the network device: <b>96000 Hz</b>"),
            "{out}"
        );
        assert!(!out.contains("flash"));
        let out = rate_follow_line(&rate_follow::Report {
            rate: None,
            error: Some("the daemon has 2 devices <x>".into()),
        })
        .into_string();
        assert!(out.contains("not known yet"), "{out}");
        assert!(
            out.contains("class=\"flash bad\"") && out.contains("2 devices &lt;x&gt;"),
            "{out}"
        );
    }

    #[test]
    fn the_master_card_shows_the_follower_only_when_it_is_enabled() {
        let runner: std::sync::Arc<dyn services::Runner> =
            std::sync::Arc::new(FakeRunner::default());
        let mut c = cfg();
        let a = Appliance::new(c.clone(), runner.clone());
        assert!(
            master_card(&a, None, None, None, "t")
                .into_string()
                .is_empty()
        );
        c.rate_follow.enabled = true;
        let a = Appliance::new(c, runner);
        *a.rate_follow.lock().unwrap() = rate_follow::Report {
            rate: Some(48_000),
            error: None,
        };
        let out = master_card(&a, None, None, None, "t").into_string();
        assert!(
            out.contains("id=\"master\"") && out.contains("48000 Hz"),
            "{out}"
        );
    }
}
