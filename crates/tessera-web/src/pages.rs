//! Server-rendered pages (no JavaScript). Every page needs a session; every
//! form that changes something carries the session's CSRF token and answers
//! with a redirect (post/redirect/get) and a one-shot message.

use std::num::NonZeroU16;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::Form;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use maud::{DOCTYPE, Markup, html};
use serde::Deserialize;
use tessera_control_types::{
    Channel, ClockRole, Command, CommandError, DeviceKey, DeviceStatus, Direction, Encoding,
    Impact, ImpactToken, LatencyUs, Metrics, PrimarySource, Query, Reply, RequestBody,
    RestartScope, SampleRate, Status, SubscriptionState,
};
use tesseractl::client::ClientError;
use tesseractl::output;

use crate::auth::{Session, Stored, same, verify_password};
use crate::signal::{self, Sample};
use crate::theme::{self, Theme};
use crate::{Flash, Shared, call, clear_cookie, explain, peer, session_cookie, session_of};

const RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
const BITS: [u8; 3] = [16, 24, 32];

// ---- plumbing --------------------------------------------------------------

fn to_login() -> Response {
    Redirect::to("/login").into_response()
}

/// The session, or a redirect to the login page.
#[allow(clippy::result_large_err)]
pub(crate) fn need(st: &Shared, headers: &HeaderMap) -> Result<(String, Session), Response> {
    session_of(st, headers).ok_or_else(to_login)
}

/// Check the form's CSRF token; on success run nothing, on failure answer.
#[allow(clippy::result_large_err)]
pub(crate) fn need_post(
    st: &Shared,
    headers: &HeaderMap,
    csrf: &str,
) -> Result<(String, Session), Response> {
    let (t, s) = need(st, headers)?;
    if !same(csrf, &s.csrf) {
        return Err((
            StatusCode::FORBIDDEN,
            "stale or missing form token; reload the page",
        )
            .into_response());
    }
    Ok((t, s))
}

pub(crate) async fn status_of(st: &Shared) -> Result<Status, String> {
    match call(st, RequestBody::Query(Query::Status)).await {
        Ok(Reply::Status(s)) => {
            st.set_host(host_label(&s));
            Ok(s)
        }
        Ok(other) => Err(format!("unexpected reply: {other:?}")),
        Err(e) => Err(explain(&e)),
    }
}

/// What the top bar calls this device: the device name and the primary
/// interface's address, whichever are known.
fn host_label(s: &Status) -> Option<String> {
    let name = s.devices.first().map(|d| d.name.clone());
    let addr = s.network.as_ref().and_then(|n| {
        n.interfaces
            .iter()
            .find(|i| Some(&i.name) == n.primary.as_ref())
            .and_then(|i| i.ipv4.clone())
    });
    match (name, addr) {
        (Some(n), Some(a)) => Some(format!("{n} · {a}")),
        (Some(n), None) => Some(n),
        (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

pub(crate) async fn metrics_of(st: &Shared) -> Option<Metrics> {
    match call(st, RequestBody::Query(Query::Metrics)).await {
        Ok(Reply::Metrics(m)) => Some(m),
        _ => None,
    }
}

/// What came of a list of commands.
enum Outcome {
    /// All were applied.
    Done(usize),
    /// The daemon asks for confirmation of the first of `pending` (a change
    /// that restarts something) and has applied nothing of it. `applied`
    /// commands before it were applied.
    Confirm {
        impact: Box<Impact>,
        pending: Vec<Command>,
        applied: usize,
    },
    /// The first refusal, explained.
    Failed(String),
}

/// Send commands in order; stop at the first refusal or at the first that
/// needs confirmation.
async fn drive(st: &Shared, s: &Session, cmds: Vec<Command>) -> Outcome {
    let mut applied = 0;
    let mut rest = cmds;
    while let Some(c) = rest.first().cloned() {
        let kind = c.kind();
        let r = call(st, RequestBody::Command(c)).await;
        tracing::info!(session = %s.id, command = kind, ok = r.is_ok(), "page command");
        match r {
            Ok(_) => {
                applied += 1;
                rest.remove(0);
            }
            Err(ClientError::Command(CommandError::ConfirmationRequired { impact })) => {
                return Outcome::Confirm {
                    impact,
                    pending: rest,
                    applied,
                };
            }
            Err(e) => return Outcome::Failed(explain(&e)),
        }
    }
    Outcome::Done(applied)
}

/// Answer a form: redirect with a message, or ask for confirmation.
fn settle(
    st: &Shared,
    token: &str,
    s: &Session,
    headers: &HeaderMap,
    to: &str,
    ok: impl FnOnce(usize) -> String,
    outcome: Outcome,
) -> Response {
    match outcome {
        Outcome::Done(n) => back(st, token, to, Ok(ok(n))),
        Outcome::Failed(m) => back(st, token, to, Err(m)),
        Outcome::Confirm {
            impact,
            pending,
            applied,
        } => confirm_page(
            &Chrome::of(st, headers),
            s,
            &impact,
            &pending,
            applied,
            false,
            StatusCode::OK,
        ),
    }
}

fn back(st: &Shared, token: &str, to: &str, r: Result<String, String>) -> Response {
    match r {
        Ok(m) => st.set_flash(token, true, m),
        Err(m) => st.set_flash(token, false, m),
    }
    Redirect::to(to).into_response()
}

pub(crate) fn hms(t: SystemTime) -> String {
    let s = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

pub(crate) fn khz(hz: u32) -> String {
    let k = f64::from(hz) / 1000.0;
    if hz % 1000 == 0 {
        format!("{k:.0} kHz")
    } else {
        format!("{k:.1} kHz")
    }
}

pub(crate) fn ms(us: u32) -> String {
    format!("{:.2} ms", f64::from(us) / 1000.0)
}

fn sub_class(s: &SubscriptionState) -> &'static str {
    match s {
        SubscriptionState::Receiving => "ok",
        SubscriptionState::InProgress => "warn",
        _ => "bad",
    }
}

// ---- the frame of every page ---------------------------------------------------

/// The pill navigation: path and label, in order. The current page is the
/// one whose path equals the page's `active`. Appliance mode prepends a
/// "Speaker" entry and moves the overview ([`crate::appliance::nav`]).
const NAV: [(&str, &str); 7] = [
    ("/", "Overview"),
    ("/devices", "Device"),
    ("/network", "Network"),
    ("/clock", "Clock"),
    ("/metrics", "Metrics"),
    ("/events", "Events"),
    ("/config", "Config"),
];

/// Pages that answer `GET` besides those in [`NAV`]: where a `POST /theme`
/// may send the browser back to (the appliance pages exist only in appliance
/// mode; elsewhere they are 404, which the theme form never links to).
pub(crate) const EXTRA_GET_PAGES: &[&str] = &["/login", "/speaker", "/device", "/overview"];

/// Does `path` (without query or fragment) name a page that answers `GET`?
#[must_use]
pub fn is_get_page(path: &str) -> bool {
    NAV.iter().any(|(p, _)| *p == path) || EXTRA_GET_PAGES.contains(&path)
}

/// What the frame of a page needs from the request: the theme and the
/// device's name for the host chip.
#[derive(Debug, Clone, Default)]
pub struct Chrome {
    /// The colour theme of this request (its cookie).
    pub theme: Theme,
    /// "name · address" of this device, when a status was seen.
    pub host: Option<String>,
    /// The navigation entries (path, label): [`NAV`], with "Speaker" first
    /// and the overview at `/device` in appliance mode ([`crate::appliance::nav`]).
    pub nav: Vec<(&'static str, &'static str)>,
}

impl Chrome {
    /// The frame for this request.
    #[must_use]
    pub fn of(st: &Shared, headers: &HeaderMap) -> Self {
        Self {
            theme: Theme::of(headers),
            host: st.host(),
            nav: crate::appliance::nav(st, &NAV),
        }
    }
}

/// A page: its title, which navigation entry it is and how often it
/// refreshes itself.
#[derive(Debug, Clone, Copy)]
pub(crate) struct View<'a> {
    title: &'a str,
    active: &'a str,
    refresh: Option<u32>,
}

/// A page titled `title` under the navigation entry `active`.
pub(crate) const fn view<'a>(title: &'a str, active: &'a str) -> View<'a> {
    View {
        title,
        active,
        refresh: None,
    }
}

impl View<'_> {
    /// Update live values in place every `secs` seconds.
    pub(crate) const fn every(mut self, secs: u32) -> Self {
        self.refresh = Some(secs);
        self
    }
}

fn top_bar(chrome: &Chrome, active: &str, s: Option<&Session>) -> Markup {
    html! {
        header.top {
            a.brand href="/" { span.mark { i {} i {} i {} i {} } "tessera" }
            // The device's name and address are for operators only.
            @if let Some(s) = s {
                @if let Some(h) = &chrome.host { span.host { (h) } }
                nav.nav {
                    @for (href, label) in &chrome.nav {
                        a.cur[*href == active] href=(href) { (label) }
                    }
                }
                form.seg method="post" action="/theme" {
                    input type="hidden" name="csrf" value=(s.csrf);
                    @for t in Theme::ALL {
                        button.cur[t == chrome.theme] type="submit" name="theme" value=(t.name())
                            aria-pressed=(if t == chrome.theme { "true" } else { "false" }) { (t.label()) }
                    }
                }
                form.logout method="post" action="/logout" {
                    input type="hidden" name="csrf" value=(s.csrf);
                    button.ghost type="submit" { "Log out" }
                }
            }
        }
    }
}

fn footer(refresh: Option<u32>) -> Markup {
    html! {
        footer.foot {
            span { "tessera-web " (env!("CARGO_PKG_VERSION")) }
            @match refresh {
                Some(r) => { span data-live-status { "live updates every " (r) " s" } }
                None => { span { "no automatic refresh" } }
            }
            span data-live-clock { "UTC " (hms(SystemTime::now())) }
            a href="/api/v1/status" { "API" }
        }
    }
}

/// The whole document around `body`: the frame of every page.
pub(crate) fn layout(
    view: &View<'_>,
    chrome: &Chrome,
    s: Option<&Session>,
    flash: Option<Flash>,
    body: &Markup,
) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" data-theme=(chrome.theme.name()) {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                @if view.refresh.is_some() { script src="/live.js" defer {} }
                title { (view.title) " · tessera" }
                link rel="stylesheet" href=(theme::css_path());
            }
            body {
                div.app {
                    (top_bar(chrome, view.active, s))
                    main {
                        h1.sr { (view.title) }
                        @if let Some(f) = flash {
                            p class=(if f.ok { "flash ok" } else { "flash bad" }) { (f.text) }
                        }
                        div id="live-content" data-live-interval=[view.refresh] { (body) }
                    }
                    // The version is for operators only.
                    @if s.is_some() { (footer(view.refresh)) }
                }
            }
        }
    }
}

/// A page of a logged-in session, with the session's one-shot message.
pub(crate) fn page(
    st: &Shared,
    token: &str,
    s: &Session,
    headers: &HeaderMap,
    view: View<'_>,
    body: &Markup,
) -> Response {
    let flash = live_flash(st, token, headers);
    layout(&view, &Chrome::of(st, headers), Some(s), flash, body).into_response()
}

/// Background reads must not consume a message intended for a form redirect.
pub(crate) fn live_flash(st: &Shared, token: &str, headers: &HeaderMap) -> Option<Flash> {
    if headers.contains_key("x-tessera-live") {
        None
    } else {
        st.take_flash(token)
    }
}

/// An error shown in place of content.
pub(crate) fn error_box(e: &str) -> Markup {
    html! { p.flash.bad { (e) } }
}

// ---- login -----------------------------------------------------------------

/// Anything else.
pub async fn not_found(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let s = session_of(&st, &headers).map(|(_, s)| s);
    let body = html! {
        div.grid {
            section.card {
                h2 { "Not found" }
                p { "There is no such page." }
                p { a href="/" { "Back to the overview" } }
            }
        }
    };
    (
        StatusCode::NOT_FOUND,
        layout(
            &view("Not found", ""),
            &Chrome::of(&st, &headers),
            s.as_ref(),
            None,
            &body,
        ),
    )
        .into_response()
}

fn login_page(msg: Option<&str>, st: &Shared, headers: &HeaderMap) -> Markup {
    let stored = st.password.read();
    let body = html! {
        div.login-wrap {
            section.card.login-card {
                div.brand { span.mark { i {} i {} i {} i {} } "tessera" }
                @match &stored {
                    Stored::Missing => {
                        p.flash.bad {
                            "No password is set. On the device, run "
                            code { "sudo tessera-web set-password" }
                            " and reload this page."
                        }
                    }
                    Stored::Unreadable(m) => { p.flash.bad { (m) } }
                    Stored::Hash(_) => {
                        @if let Some(m) = msg { p.flash.bad { (m) } }
                        form.login method="post" action="/login" {
                            label { "Password" input type="password" name="password" autofocus required; }
                            button.btn.primary type="submit" { "Log in" }
                        }
                    }
                }
            }
        }
    };
    html! {
        (DOCTYPE)
        html lang="en" data-theme=(Theme::of(headers).name()) {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Log in · tessera" }
                link rel="stylesheet" href=(theme::css_path());
            }
            body { div.app { (body) } }
        }
    }
}

/// `GET /login`.
pub async fn login_form(State(st): State<Shared>, headers: HeaderMap) -> Response {
    if session_of(&st, &headers).is_some() {
        return Redirect::to("/").into_response();
    }
    login_page(None, &st, &headers).into_response()
}

/// The login form.
#[derive(Debug, Deserialize)]
pub struct LoginForm {
    password: String,
}

/// `POST /login`.
pub async fn login(
    State(st): State<Shared>,
    ext: axum::http::Extensions,
    headers: HeaderMap,
    Form(f): Form<LoginForm>,
) -> Response {
    let addr = peer(&ext);
    let now = Instant::now();
    if !st.limiter.allowed(addr, now) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            login_page(
                Some("Too many failed attempts. Wait a minute."),
                &st,
                &headers,
            ),
        )
            .into_response();
    }
    let Stored::Hash(h) = st.password.read() else {
        return login_page(None, &st, &headers).into_response();
    };
    let pw = f.password;
    let ok = tokio::task::spawn_blocking(move || verify_password(&pw, &h))
        .await
        .unwrap_or(false);
    if !ok {
        st.limiter.failed(addr, now);
        tracing::warn!(%addr, "failed login");
        return (
            StatusCode::UNAUTHORIZED,
            login_page(Some("Wrong password."), &st, &headers),
        )
            .into_response();
    }
    let (token, s) = st.sessions.create(now);
    tracing::info!(session = %s.id, %addr, "login");
    (
        [(header::SET_COOKIE, session_cookie(&token, st.cookie_secure))],
        Redirect::to("/"),
    )
        .into_response()
}

/// A form with only the token.
#[derive(Debug, Deserialize)]
pub struct TokenForm {
    csrf: String,
}

/// `POST /logout`.
pub async fn logout(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<TokenForm>,
) -> Response {
    let (token, _) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    st.sessions.remove(&token);
    (
        [(header::SET_COOKIE, clear_cookie(st.cookie_secure))],
        Redirect::to("/login"),
    )
        .into_response()
}

// ---- overview ----------------------------------------------------------------

fn devices_table(s: &Status) -> Markup {
    html! {
        div.table { table {
            thead { tr { th { "Device" } th { "Name" } th { "Rate" } th { "Bits" } th { "Latency" } th { "RX" } th { "TX" } th { "Subscriptions" } } }
            tbody {
                @for d in &s.devices {
                    @let rec = d.subscriptions.iter().filter(|x| matches!(x.state, SubscriptionState::Receiving)).count();
                    tr {
                        td { (d.key.0) }
                        td { (d.name) }
                        td { (khz(d.sample_rate.0)) }
                        td { (d.encoding.0) }
                        td { (ms(d.latency.0)) }
                        td { (d.rx_channels.len()) }
                        td { (d.tx_channels.len()) }
                        td { (rec) " of " (d.subscriptions.len()) " receiving" }
                    }
                }
            }
        } }
    }
}

fn clock_summary(s: &Status) -> Markup {
    html! {
        @if let Some(c) = &s.clock_settings {
            dl.kv {
                dt { "Role" } dd { (format!("{:?}", c.role).to_lowercase()) }
                dt { "Priority" } dd { (c.priority) }
                @if !c.locked.is_empty() { dt { "Locked" } dd { (c.locked.join(", ")) } }
            }
        }
        @if s.clocks.is_empty() { p.muted { "No clock is running." } } @else {
            div.table { table {
                thead { tr { th { "Interface" } th { "State" } th { "Offset" } th { "Rate" } th { "Leader" } th { "Timestamps" } } }
                tbody {
                    @for c in &s.clocks {
                        tr {
                            td { (c.interface) }
                            td { (format!("{:?}", c.state).to_lowercase()) }
                            td { (c.offset_ns.map_or("-".into(), |o| format!("{o} ns"))) }
                            td { (c.rate_ppb) " ppb" }
                            td { (c.grandmaster.clone().unwrap_or_else(|| "-".into())) }
                            td { (format!("{:?}", c.timestamping).to_lowercase()) }
                        }
                    }
                }
            } }
        }
    }
}

/// `GET /`.
pub async fn overview(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let status = status_of(&st).await;
    let mut m = metrics_of(&st).await;
    let now = Instant::now();
    let prev = st
        .flow_sample
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .filter(|p| p.usable_at(now));
    let prev = match (prev, &m) {
        (Some(p), _) => Some(p),
        // No recent sample: take a second one shortly after the first.
        (None, Some(first)) => {
            let p = Sample::of(first, now);
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            m = metrics_of(&st).await;
            Some(p)
        }
        (None, None) => None,
    };
    let act = m.as_ref().map_or_else(Default::default, |m| {
        let cur = Sample::of(m, Instant::now());
        let a = signal::activity(prev.as_ref(), m, &cur);
        if let Ok(mut g) = st.flow_sample.lock() {
            *g = Some(cur);
        }
        a
    });
    let body = match &status {
        Err(e) => error_box(e),
        Ok(stt) => html! {
            div.grid {
                section.card {
                    h2 { "Signal path" span.n { "highlighted while packets move" } }
                    (signal::render(stt, m.as_ref(), act))
                }
                section.card {
                    h2 { "Devices" span.n { "tessera " (stt.version) } }
                    (devices_table(stt))
                }
                section.card.c6 {
                    h2 { "Network" }
                    @if let Some(n) = &stt.network {
                        dl.kv { dt { "Primary" } dd { (output::render_primary(n)) } }
                    } @else { p.muted { "Not reported yet." } }
                    p { a href="/network" { "Interfaces and selection" } }
                }
                section.card.c6 {
                    h2 { "Clock" }
                    (clock_summary(stt))
                }
                @if !stt.warnings.is_empty() {
                    section.card.warnings {
                        h2 { "Warnings" }
                        ul { @for w in &stt.warnings { li { (w) } } }
                    }
                }
            }
        },
    };
    page(
        &st,
        &token,
        &s,
        &headers,
        view("Overview", crate::appliance::overview_path(&st)).every(5),
        &body,
    )
}

// ---- network -------------------------------------------------------------------

/// `GET /network`.
pub async fn network(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Some(response) = crate::network_profiles::page(&st, &token, &s, &headers).await {
        return response;
    }
    let body = match status_of(&st).await {
        Err(e) => error_box(&e),
        Ok(stt) => match &stt.network {
            None => html! { p { "The daemon has not reported its interfaces yet." } },
            Some(n) => html! { div.grid {
                section.card {
                    h2 { "Primary interface" }
                    dl.kv { dt { "Primary" } dd { (output::render_primary(n)) } }
                    p.muted {
                        "The primary interface carries discovery, control, audio and the clock. "
                        "Automatic: the first wired interface with a link and an address. "
                        "A selection is kept across restarts; if its interface is missing the "
                        "device falls back and switches back when it returns."
                    }
                    @if n.source == PrimarySource::Fallback { p.flash.bad { "The selected interface is not usable now; see below." } }
                }
                section.card {
                    h2 { "Interfaces" }
                    form method="post" action="/network/primary" {
                        input type="hidden" name="csrf" value=(s.csrf);
                        div.table { table {
                            thead { tr { th {} th { "Interface" } th { "MAC" } th { "IPv4" } th { "Link" } th { "Type" } th { "Usable" } } }
                            tbody {
                                @for i in &n.interfaces {
                                    tr class=(if n.primary.as_deref() == Some(i.name.as_str()) { "current" } else { "" }) {
                                        td { input type="radio" name="interface" value=(i.name) checked[n.selected.as_deref() == Some(i.name.as_str())] disabled[n.locked || !i.usable]; }
                                        td { (i.name) @if n.primary.as_deref() == Some(i.name.as_str()) { " (primary)" } }
                                        td { (i.mac.clone().unwrap_or_else(|| "-".into())) }
                                        td { (i.ipv4.clone().unwrap_or_else(|| "-".into())) }
                                        td { (if i.carrier { "up" } else { "down" }) }
                                        td { (if i.wireless { "Wi-Fi" } else if i.is_virtual { "virtual" } else { "wired" }) }
                                        td { @if i.usable { "yes" } @else { "no: " (i.reason.clone().unwrap_or_default()) } }
                                    }
                                }
                                tr {
                                    td { input type="radio" name="interface" value="default" checked[n.selected.is_none()] disabled[n.locked]; }
                                    td colspan="6" { "Default: the configured or automatic choice" }
                                }
                            }
                        } }
                        @if n.locked {
                            p.muted { "The primary interface is locked by the configuration (" code { "[network] locked = [\"primary\"]" } ")." }
                        } @else {
                            button.btn.primary type="submit" { "Use the selected interface" }
                            p.muted.mt { "Changing the primary restarts the clock on the new interface and moves the device's flows; subscriptions are kept." }
                        }
                    }
                    @if !n.allow_wireless {
                        p.muted { "Wi-Fi interfaces need " code { "[network] allow_wireless = true" } " in the configuration." }
                    }
                }
            } },
        },
    };
    page(
        &st,
        &token,
        &s,
        &headers,
        view("Network", "/network"),
        &body,
    )
}

/// The primary-interface form.
#[derive(Debug, Deserialize)]
pub struct PrimaryForm {
    csrf: String,
    interface: Option<String>,
}

/// `POST /network/primary`.
pub async fn set_primary(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<PrimaryForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(choice) = f.interface else {
        return back(&st, &token, "/network", Err("Choose an interface.".into()));
    };
    let interface = tesseractl::cli::primary_arg(&choice);
    let msg = interface.as_ref().map_or_else(
        || "Selection cleared: the configured or automatic interface is used.".to_owned(),
        |i| format!("Primary interface selected: {i}."),
    );
    let o = drive(&st, &s, vec![Command::SetPrimaryInterface { interface }]).await;
    settle(&st, &token, &s, &headers, "/network", |_| msg, o)
}

// ---- devices -----------------------------------------------------------------

fn device_card(d: &DeviceStatus, csrf: &str) -> Markup {
    let key = &d.key.0;
    let locked = |k: &str| d.locked.iter().any(|l| l == k);
    let mut rates = RATES.to_vec();
    if !rates.contains(&d.sample_rate.0) {
        rates.push(d.sample_rate.0);
        rates.sort_unstable();
    }
    html! {
        section.card {
            h2 { (d.name) span.n { (key) } }
            form.fields method="post" action=(dev_path(key, "settings")) {
                input type="hidden" name="csrf" value=(csrf);
                label { "Name" input type="text" name="name" value=(d.name) maxlength="31" disabled[locked("name")]; }
                label { "Sample rate"
                    select name="rate" disabled[locked("sample_rate")] {
                        @for r in &rates { option value=(r) selected[*r == d.sample_rate.0] { (khz(*r)) } }
                    }
                }
                label { "Encoding"
                    select name="bits" disabled[locked("encoding")] {
                        @for b in BITS { option value=(b) selected[b == d.encoding.0] { (b) " bit" } }
                    }
                }
                label { "Latency (µs)" input type="number" name="latency_us" min="1" value=(d.latency.0) disabled[locked("latency")]; }
                button.btn.primary type="submit" { "Apply" }
                @if !d.locked.is_empty() { p.muted { "Locked by the configuration: " (d.locked.join(", ")) } }
            }
            p.muted { "A new rate or encoding rebuilds the flows; audio clients re-attach." }

            h3 { "Channels" }
            (channels_form(
                key,
                (locked("rx_channels"), locked("tx_channels")),
                csrf,
                &d.rx_channels.len().to_string(),
                &d.tx_channels.len().to_string(),
            ))

            h3 { "Subscriptions" }
            @if d.subscriptions.is_empty() { p.muted { "None." } } @else {
                div.table { table {
                    thead { tr { th { "RX channel" } th { "From" } th { "State" } th { "Latency" } th {} } }
                    tbody {
                        @for x in &d.subscriptions {
                            tr {
                                td { (x.rx_channel.get()) " " span.muted { (d.rx_channels.iter().find(|c| c.channel == x.rx_channel).map(|c| c.label.clone()).unwrap_or_default()) } }
                                td { (x.tx_channel) " @ " (x.tx_device) }
                                td { span class=(sub_class(&x.state)) { (output::sub_state(&x.state)) } }
                                td { (x.effective_latency.map_or("-".into(), |l| ms(l.0))) }
                                td {
                                    form.inline method="post" action=(dev_path(key, "unsubscribe")) {
                                        input type="hidden" name="csrf" value=(csrf);
                                        input type="hidden" name="rx_channel" value=(x.rx_channel.get());
                                        button.btn.sm.danger type="submit" { "Remove" }
                                    }
                                }
                            }
                        }
                    }
                } }
            }
            form.fields method="post" action=(dev_path(key, "subscribe")) {
                input type="hidden" name="csrf" value=(csrf);
                label { "RX channel"
                    select name="rx_channel" {
                        @for c in &d.rx_channels { option value=(c.channel.get()) { (c.channel.get()) " · " (c.label) } }
                    }
                }
                label { "TX channel" input type="text" name="tx_channel" required placeholder="channel name"; }
                label { "TX device" input type="text" name="tx_device" required placeholder="device name"; }
                button.btn.primary type="submit" { "Subscribe" }
            }

            h3 { "Channel labels" }
            div.cols {
                @for (dir, title, chans, lock) in [("rx", "Receive", &d.rx_channels, "rx_channel_labels"), ("tx", "Transmit", &d.tx_channels, "tx_channel_labels")] {
                    div {
                        h4 { (title) }
                        table {
                            tbody {
                                @for c in chans {
                                    tr {
                                        td { (c.channel.get()) }
                                        td {
                                            form.inline method="post" action=(dev_path(key, "label")) {
                                                input type="hidden" name="csrf" value=(csrf);
                                                input type="hidden" name="direction" value=(dir);
                                                input type="hidden" name="channel" value=(c.channel.get());
                                                input type="text" name="label" value=(c.label) maxlength="31" disabled[locked(lock)];
                                                button.btn.sm type="submit" disabled[locked(lock)] { "Set" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The channel counts form: submitting it shows what the change does and
/// asks before anything is applied.
fn channels_form(
    key: &str,
    (rx_locked, tx_locked): (bool, bool),
    csrf: &str,
    rx: &str,
    tx: &str,
) -> Markup {
    html! {
        form.fields method="post" action=(dev_path(key, "channels")) {
            input type="hidden" name="csrf" value=(csrf);
            label { "Receive channels" input type="number" name="rx" min="0" max="65535" value=(rx) required readonly[rx_locked]; }
            label { "Transmit channels" input type="number" name="tx" min="0" max="65535" value=(tx) required readonly[tx_locked]; }
            button.btn type="submit" disabled[rx_locked && tx_locked] { "Review the change" }
            p.muted {
                "Changing the channel counts restarts this device: its flows are rebuilt and its audio clients are detached. "
                "The next page shows exactly what happens and asks before anything is applied."
                @if rx_locked || tx_locked { " Counts locked by the configuration cannot be changed." }
            }
        }
    }
}

/// `GET /devices`.
pub async fn devices(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let body = match status_of(&st).await {
        Err(e) => error_box(&e),
        Ok(stt) => html! { div.grid { @for d in &stt.devices { (device_card(d, &s.csrf)) } } },
    };
    page(&st, &token, &s, &headers, view("Device", "/devices"), &body)
}

/// The device settings form.
#[derive(Debug, Deserialize)]
pub struct SettingsForm {
    csrf: String,
    name: Option<String>,
    rate: Option<u32>,
    bits: Option<u8>,
    latency_us: Option<u32>,
}

/// `POST /devices/{key}/settings`: only the values that differ are sent.
pub async fn device_settings(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Form(f): Form<SettingsForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let cur = match status_of(&st).await {
        Ok(stt) => stt.devices.into_iter().find(|d| d.key.0 == key),
        Err(e) => return back(&st, &token, "/devices", Err(e)),
    };
    let Some(d) = cur else {
        return back(&st, &token, "/devices", Err(format!("No device `{key}`.")));
    };
    let dev = DeviceKey(key);
    let mut cmds = Vec::new();
    if let Some(n) = f.name.filter(|n| *n != d.name) {
        cmds.push(Command::SetDeviceName {
            device: dev.clone(),
            name: n,
        });
    }
    if let Some(r) = f.rate.filter(|r| *r != d.sample_rate.0) {
        cmds.push(Command::SetSampleRate {
            device: dev.clone(),
            rate: SampleRate(r),
        });
    }
    if let Some(b) = f.bits.filter(|b| *b != d.encoding.0) {
        cmds.push(Command::SetEncoding {
            device: dev.clone(),
            encoding: Encoding(b),
        });
    }
    if let Some(l) = f.latency_us.filter(|l| *l != d.latency.0) {
        cmds.push(Command::SetLatency {
            device: dev.clone(),
            latency: LatencyUs(l),
        });
    }
    if cmds.is_empty() {
        return back(&st, &token, "/devices", Ok("Nothing changed.".into()));
    }
    let o = drive(&st, &s, cmds).await;
    settle(
        &st,
        &token,
        &s,
        &headers,
        "/devices",
        |n| format!("{n} setting(s) applied."),
        o,
    )
}

/// The channel counts form. Missing fields are refused after the token check
/// (403 without a valid token, then 400 on the page).
#[derive(Debug, Deserialize)]
pub struct ChannelsForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    rx: String,
    #[serde(default)]
    tx: String,
}

/// `POST /devices/{key}/channels`: a dry run, never a change. A change that
/// restarts something is answered with the confirmation page; errors (a
/// locked count, an invalid or over-budget value) are shown on a page with
/// the form again.
pub async fn device_channels(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Form(f): Form<ChannelsForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let chrome = Chrome::of(&st, &headers);
    let fail = |code: StatusCode, msg: String| {
        channels_error(&st, &token, &s, &chrome, &key, (&f.rx, &f.tx), (code, msg))
    };
    let parse = |v: &str| v.trim().parse::<u16>().ok();
    let (Some(rx), Some(tx)) = (parse(&f.rx), parse(&f.tx)) else {
        return fail(
            StatusCode::BAD_REQUEST,
            "Channel counts are whole numbers from 0 to 65535.".into(),
        )
        .await;
    };
    let device = DeviceKey(key.clone());
    let probe = Command::SetDeviceChannels {
        device: device.clone(),
        rx,
        tx,
        dry_run: true,
    };
    let r = call(&st, RequestBody::Command(probe)).await;
    tracing::info!(session = %s.id, command = "set_device_channels", dry_run = true, ok = r.is_ok(), "page command");
    match r {
        Ok(Reply::Impact(impact)) if impact.needs_confirmation() => {
            let apply = Command::SetDeviceChannels {
                device,
                rx,
                tx,
                dry_run: false,
            };
            confirm_page(&chrome, &s, &impact, &[apply], 0, false, StatusCode::OK)
        }
        // The daemon sees nothing to restart. That is "nothing to do" only
        // if the device really has these counts; otherwise send the change
        // itself and let the daemon decide.
        Ok(Reply::Impact(_)) => {
            let now = status_of(&st).await.ok().and_then(|stt| {
                stt.devices
                    .into_iter()
                    .find(|d| d.key == device)
                    .map(|d| (d.rx_channels.len(), d.tx_channels.len()))
            });
            if now == Some((usize::from(rx), usize::from(tx))) {
                return back(
                    &st,
                    &token,
                    "/devices",
                    Ok(format!(
                        "`{key}` already has {rx} receive and {tx} transmit channels."
                    )),
                );
            }
            let apply = Command::SetDeviceChannels {
                device,
                rx,
                tx,
                dry_run: false,
            };
            let msg = applied_message(&apply);
            let o = drive(&st, &s, vec![apply]).await;
            settle(&st, &token, &s, &headers, "/devices", |_| msg, o)
        }
        Ok(other) => {
            fail(
                StatusCode::BAD_GATEWAY,
                format!("The daemon answered a dry run with {other:?}."),
            )
            .await
        }
        Err(ClientError::Command(e)) => {
            fail(command_error_status(&e), explain(&ClientError::Command(e))).await
        }
        Err(e) => fail(StatusCode::BAD_GATEWAY, explain(&e)).await,
    }
}

/// The HTTP status that goes with a refusal of the daemon, for pages that
/// show the refusal inline.
fn command_error_status(e: &CommandError) -> StatusCode {
    match e {
        CommandError::Invalid { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        CommandError::Locked { .. } => StatusCode::CONFLICT,
        CommandError::NotFound { .. } => StatusCode::NOT_FOUND,
        CommandError::PermissionDenied => StatusCode::FORBIDDEN,
        CommandError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::BAD_GATEWAY,
    }
}

/// The channel counts page with an error above the form (the values typed
/// stay in the fields).
async fn channels_error(
    st: &Shared,
    token: &str,
    s: &Session,
    chrome: &Chrome,
    key: &str,
    (rx, tx): (&str, &str),
    (code, msg): (StatusCode, String),
) -> Response {
    // The same form as on the devices page, with its locked counts.
    let locked = status_of(st).await.ok().map_or((false, false), |stt| {
        stt.devices
            .iter()
            .find(|d| d.key.0 == key)
            .map_or((false, false), |d| {
                let l = |k: &str| d.locked.iter().any(|x| x == k);
                (l("rx_channels"), l("tx_channels"))
            })
    });
    let body = html! {
        div.grid { section.card {
            h2 { "Channels" span.n { (key) } }
            (error_box(&msg))
            (channels_form(key, locked, &s.csrf, rx, tx))
            p { a href="/devices" { "Back to the devices" } }
        } }
    };
    let flash = st.take_flash(token);
    let title = format!("Channels of {key}");
    (
        code,
        layout(&view(&title, "/devices"), chrome, Some(s), flash, &body),
    )
        .into_response()
}

/// A device key as one URL path segment (percent-encoded).
fn dev_path(key: &str, rest: &str) -> String {
    let mut out = String::from("/devices/");
    for b in key.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out.push('/');
    out.push_str(rest);
    out
}

/// Where a command's page is.
fn landing(c: &Command) -> &'static str {
    match c {
        Command::SetPrimaryInterface { .. } => "/network",
        Command::SetClockRole { .. } | Command::SetClockPriority { .. } => "/clock",
        Command::ReloadConfig => "/config",
        _ => "/devices",
    }
}

/// What to tell after a confirmed command was applied.
fn applied_message(c: &Command) -> String {
    match c {
        Command::SetDeviceChannels { device, rx, tx, .. } => format!(
            "`{}` now has {rx} receive and {tx} transmit channels. The device restarts; audio clients re-attach.",
            device.0
        ),
        Command::SetPrimaryInterface { interface: Some(i) } => {
            format!("Primary interface selected: {i}.")
        }
        Command::SetPrimaryInterface { interface: None } => {
            "Selection cleared: the configured or automatic interface is used.".into()
        }
        Command::ReloadConfig => "Configuration reloaded.".into(),
        _ => "The change was applied.".into(),
    }
}

/// The warning above the impact: what applying does to the running system.
fn restart_warning(impact: &Impact) -> Markup {
    html! {
        @match impact.restart.scope {
            RestartScope::Daemon => {
                p.flash.bad {
                    strong { "Applying this restarts the whole service." }
                    " Every device, the control socket and the clock restart. This portal loses its connection to the daemon for a moment, and every audio client must re-attach."
                }
            }
            RestartScope::Device => {
                p.flash.bad {
                    strong { "Applying this restarts the device" }
                    @if !impact.restart.devices.is_empty() {
                        " "
                        @for (i, d) in impact.restart.devices.iter().enumerate() {
                            @if i > 0 { ", " }
                            code { (d.0) }
                        }
                    }
                    ". Its flows are rebuilt, its audio clients are detached and must re-attach, and audio on its channels is interrupted."
                }
            }
            _ => {}
        }
    }
}

/// The confirmation page: the impact of the first of `pending`, a button
/// that sends it confirmed with the impact's token, and a way back. Nothing
/// has been applied of `pending`; `changed` says the impact is a new one
/// because the situation changed after the previous page.
fn confirm_page(
    chrome: &Chrome,
    s: &Session,
    impact: &Impact,
    pending: &[Command],
    applied: usize,
    changed: bool,
    code: StatusCode,
) -> Response {
    let cancel = pending.first().map_or("/devices", landing);
    let v = view("Confirm the change", cancel);
    // The digest is what makes "confirm what was shown" safe; without one
    // (a daemon that does not seal its impacts) nothing is offered.
    let Some(token) = impact.token.as_ref().map(ImpactToken::as_str) else {
        let body = html! {
            div.grid { section.card {
                h2 { "Confirm the change" }
                (error_box("The daemon sent no impact token, so this change cannot be confirmed safely here. Nothing was applied."))
                p { a href=(cancel) { "Back" } }
            } }
        };
        return (
            StatusCode::BAD_GATEWAY,
            layout(&v, chrome, Some(s), None, &body),
        )
            .into_response();
    };
    let commands = serde_json::to_string(pending).unwrap_or_default();
    let text = impact.describe();
    let body = html! {
        div.grid { section.card {
            h2 { "Confirm the change" span.n { "nothing is applied before you confirm" } }
            @if changed {
                p.flash.bad { "The situation changed since the previous page, so nothing was applied. Check what happens now." }
            }
            (restart_warning(impact))
            ul { @for line in text.lines() { li { (line) } } }
            @if applied > 0 {
                p.muted { (applied) " change(s) before this one were already applied." }
            }
            form.confirm method="post" action="/confirm" {
                input type="hidden" name="csrf" value=(s.csrf);
                input type="hidden" name="token" value=(token);
                input type="hidden" name="commands" value=(commands);
                input type="hidden" name="applied" value=(applied);
                button.btn.primary type="submit" { "Confirm and apply" }
                " "
                a href=(cancel) { "Cancel" }
            }
            p.muted { "Cancel changes nothing." }
        } }
    };
    (code, layout(&v, chrome, Some(s), None, &body)).into_response()
}

/// The confirmation form. Missing fields are refused after the token check:
/// 403 without a valid CSRF token, then 400.
#[derive(Debug, Deserialize)]
pub struct ConfirmForm {
    #[serde(default)]
    csrf: String,
    /// The impact token that was shown.
    #[serde(default)]
    token: String,
    /// The commands still to send, as a JSON list; the first is confirmed.
    #[serde(default)]
    commands: String,
    /// How many changes of this series were already applied; shown on the
    /// page only, never used to decide anything.
    #[serde(default)]
    applied: String,
}

/// `POST /confirm`: send the first pending command as confirmed, with the
/// token the user saw. If the impact changed meanwhile the daemon applies
/// nothing and the new impact is shown for another decision.
pub async fn confirm(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<ConfirmForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let mut cmds: Vec<Command> = serde_json::from_str(&f.commands).unwrap_or_default();
    if cmds.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "no command to confirm; start again",
        )
            .into_response();
    }
    // The pages only ever offer the real command; a dry run is no change
    // and must not be reported as one.
    if cmds
        .iter()
        .any(|c| matches!(c, Command::SetDeviceChannels { dry_run: true, .. }))
    {
        return (StatusCode::BAD_REQUEST, "a dry run cannot be confirmed").into_response();
    }
    let shown: usize = f.applied.trim().parse().unwrap_or(0);
    if f.token.is_empty() {
        return (StatusCode::BAD_REQUEST, "no impact token; start again").into_response();
    }
    let first = cmds.remove(0);
    let to = landing(&first);
    let expect = Some(ImpactToken::new(f.token.clone()));
    let kind = first.kind();
    let r = call(&st, RequestBody::confirmed(first.clone(), expect)).await;
    tracing::info!(session = %s.id, command = kind, ok = r.is_ok(), "page command, confirmed");
    let chrome = Chrome::of(&st, &headers);
    match r {
        Ok(_) => {
            let msg = applied_message(&first);
            if cmds.is_empty() {
                return back(&st, &token, to, Ok(msg));
            }
            match drive(&st, &s, cmds).await {
                Outcome::Done(n) => back(
                    &st,
                    &token,
                    to,
                    Ok(format!("{msg} {n} more change(s) applied.")),
                ),
                Outcome::Failed(m) => back(
                    &st,
                    &token,
                    to,
                    Err(format!("{msg} The next change failed: {m}")),
                ),
                Outcome::Confirm {
                    impact,
                    pending,
                    applied,
                } => confirm_page(
                    &chrome,
                    &s,
                    &impact,
                    &pending,
                    shown + 1 + applied,
                    false,
                    StatusCode::OK,
                ),
            }
        }
        Err(ClientError::Command(CommandError::ConfirmationRequired { impact })) => {
            let mut pending = vec![first];
            pending.extend(cmds);
            confirm_page(
                &chrome,
                &s,
                &impact,
                &pending,
                shown,
                true,
                StatusCode::CONFLICT,
            )
        }
        Err(e) => back(&st, &token, to, Err(explain(&e))),
    }
}

/// The channel label form.
#[derive(Debug, Deserialize)]
pub struct LabelForm {
    csrf: String,
    direction: String,
    channel: u16,
    label: String,
}

/// `POST /devices/{key}/label`. An empty label restores the default.
pub async fn device_label(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Form(f): Form<LabelForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let direction = if f.direction == "tx" {
        Direction::Tx
    } else {
        Direction::Rx
    };
    let Some(channel) = NonZeroU16::new(f.channel).and_then(|c| Channel::new(c.get())) else {
        return back(
            &st,
            &token,
            "/devices",
            Err("Channel numbers start at 1.".into()),
        );
    };
    let label = Some(f.label.trim().to_owned()).filter(|l| !l.is_empty());
    let o = drive(
        &st,
        &s,
        vec![Command::SetChannelLabel {
            device: DeviceKey(key),
            direction,
            channel,
            label,
        }],
    )
    .await;
    settle(
        &st,
        &token,
        &s,
        &headers,
        "/devices",
        |_| "Label set.".into(),
        o,
    )
}

/// The subscribe form.
#[derive(Debug, Deserialize)]
pub struct SubscribeForm {
    csrf: String,
    rx_channel: u16,
    tx_channel: String,
    tx_device: String,
}

/// `POST /devices/{key}/subscribe`.
pub async fn subscribe(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Form(f): Form<SubscribeForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(rx_channel) = Channel::new(f.rx_channel) else {
        return back(
            &st,
            &token,
            "/devices",
            Err("Channel numbers start at 1.".into()),
        );
    };
    let cmd = Command::Subscribe {
        device: DeviceKey(key),
        rx_channel,
        tx_channel: f.tx_channel.trim().to_owned(),
        tx_device: f.tx_device.trim().to_owned(),
    };
    let o = drive(&st, &s, vec![cmd]).await;
    settle(
        &st,
        &token,
        &s,
        &headers,
        "/devices",
        |_| "Subscription added.".into(),
        o,
    )
}

/// The unsubscribe form.
#[derive(Debug, Deserialize)]
pub struct UnsubscribeForm {
    csrf: String,
    rx_channel: u16,
}

/// `POST /devices/{key}/unsubscribe`.
pub async fn unsubscribe(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Form(f): Form<UnsubscribeForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(rx_channel) = Channel::new(f.rx_channel) else {
        return back(
            &st,
            &token,
            "/devices",
            Err("Channel numbers start at 1.".into()),
        );
    };
    let o = drive(
        &st,
        &s,
        vec![Command::Unsubscribe {
            device: DeviceKey(key),
            rx_channel,
        }],
    )
    .await;
    settle(
        &st,
        &token,
        &s,
        &headers,
        "/devices",
        |_| "Subscription removed.".into(),
        o,
    )
}

// ---- clock -----------------------------------------------------------------------

/// `GET /clock`.
pub async fn clock(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let body = match status_of(&st).await {
        Err(e) => error_box(&e),
        Ok(stt) => {
            let c = stt.clock_settings.clone();
            let locked = |k: &str| c.as_ref().is_some_and(|c| c.locked.iter().any(|l| l == k));
            html! { div.grid {
                section.card.c4 {
                    h2 { "Settings" }
                    @if let Some(c) = &c {
                        form.fields method="post" action="/clock" {
                            input type="hidden" name="csrf" value=(s.csrf);
                            label { "Role"
                                select name="role" disabled[locked("role")] {
                                    @for (v, t) in [("follower", "Follower"), ("leader", "Leader"), ("auto", "Auto (leader-capable)")] {
                                        option value=(v) selected[format!("{:?}", c.role).to_lowercase() == v] { (t) }
                                    }
                                }
                            }
                            label { "Priority (lower wins)" input type="number" name="priority" min="0" max="255" value=(c.priority) disabled[locked("priority")]; }
                            button.btn.primary type="submit" { "Apply" }
                        }
                        p.muted.mt { "A priority below 64 marks this device as a preferred leader." }
                    } @else { p.muted { "Not reported by this daemon." } }
                }
                section.card.c8 {
                    h2 { "Clocks" }
                    (clock_summary(&stt))
                }
            } }
        }
    };
    page(
        &st,
        &token,
        &s,
        &headers,
        view("Clock", "/clock").every(10),
        &body,
    )
}

/// The clock form.
#[derive(Debug, Deserialize)]
pub struct ClockForm {
    csrf: String,
    role: Option<String>,
    priority: Option<u8>,
}

/// `POST /clock`.
pub async fn clock_settings(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<ClockForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let cur = status_of(&st).await.ok().and_then(|x| x.clock_settings);
    let mut cmds = Vec::new();
    if let Some(r) = f.role {
        let role = match r.as_str() {
            "leader" => ClockRole::Leader,
            "auto" => ClockRole::Auto,
            _ => ClockRole::Follower,
        };
        if cur.as_ref().is_none_or(|c| c.role != role) {
            cmds.push(Command::SetClockRole { role });
        }
    }
    if let Some(p) = f.priority {
        if cur.as_ref().is_none_or(|c| c.priority != p) {
            cmds.push(Command::SetClockPriority { priority: p });
        }
    }
    if cmds.is_empty() {
        return back(&st, &token, "/clock", Ok("Nothing changed.".into()));
    }
    let o = drive(&st, &s, cmds).await;
    settle(
        &st,
        &token,
        &s,
        &headers,
        "/clock",
        |_| "Clock settings applied.".into(),
        o,
    )
}

// ---- metrics, events, configuration -------------------------------------------------

/// `GET /metrics`.
pub async fn metrics(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let body = match metrics_of(&st).await {
        None => error_box("The daemon did not answer."),
        Some(m) => html! { div.grid {
            section.card.c6 {
                h2 { "Daemon" }
                dl.kv {
                    dt { "Uptime" } dd { (m.daemon.uptime_ms / 1000) " s" }
                    dt { "Control connections" } dd { (m.daemon.control_connections) }
                    dt { "Requests" } dd { (m.daemon.requests_total) " (" (m.daemon.requests_denied) " denied, " (m.daemon.requests_rate_limited) " rate limited)" }
                    dt { "Audio clients" } dd { (m.audio.sessions) " now, " (m.audio.sessions_total) " total" }
                }
            }
            section.card.c6 {
                h2 { "Media engine" }
                dl.kv {
                    dt { "Running" } dd { (if m.engine.running > 0 { "yes" } else { "no" }) }
                    dt { "Flows" } dd { (m.engine.flows) }
                    dt { "RX / TX datagrams" } dd { (m.engine.rx_datagrams) " / " (m.engine.tx_datagrams) }
                    dt { "Max wake lateness" } dd { (m.engine.max_wake_lateness_us) " µs" }
                    dt { "Max tick work" } dd { (m.engine.max_tick_work_us) " µs" }
                    dt { "Real-time degraded" } dd { (m.engine.rt_degraded) }
                }
            }
            section.card {
                h2 { "Flows" }
                @if m.flows.is_empty() { p.muted { "No flows." } } @else {
                    div.table { table {
                        thead { tr { th { "Flow" } th { "Device" } th { "Dir" } th { "Packets" } th { "Late" } th { "Gaps" } th { "Max lateness" } th { "TX underruns" } } }
                        tbody {
                            @for f in &m.flows {
                                tr {
                                    td { (f.id) }
                                    td { (f.device.0) }
                                    td { (if f.direction == Direction::Rx { "RX" } else { "TX" }) }
                                    td { (f.packets) }
                                    td { (f.rx.as_ref().map_or(0, |r| r.late_packets)) }
                                    td { (f.rx.as_ref().map_or(0, |r| r.gap_events)) }
                                    td { (f.rx.as_ref().map_or("-".into(), |r| format!("{} µs", r.max_lateness_us))) }
                                    td { (f.tx.as_ref().map_or("-".into(), |t| t.underrun_samples.to_string())) }
                                }
                            }
                        }
                    } }
                }
            }
        } },
    };
    page(
        &st,
        &token,
        &s,
        &headers,
        view("Metrics", "/metrics").every(5),
        &body,
    )
}

/// `GET /events`.
pub async fn events(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let log = st.hub.recent();
    let body = html! { div.grid { section.card {
        h2 { "Events" span.n { "the latest " (log.len()) " since the portal started, UTC, newest first" } }
        @if log.is_empty() { p.muted { "No events yet." } } @else {
            div.table { table {
                tbody {
                    @for l in log.iter().rev() {
                        tr {
                            td.time { (hms(l.at)) }
                            td { @match &l.event {
                                Some(e) => { (output::render_event(&tesseractl::sanitize::cleaned(e))) }
                                None => { span.muted { "(connection to the daemon renewed; events may be missing)" } }
                            } }
                        }
                    }
                }
            } }
        }
    } } };
    page(
        &st,
        &token,
        &s,
        &headers,
        view("Events", "/events").every(5),
        &body,
    )
}

/// `GET /config`.
pub async fn config(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let text = match call(&st, RequestBody::Query(Query::EffectiveConfig)).await {
        Ok(Reply::EffectiveConfig(t)) => Ok(t),
        Ok(other) => Err(format!("unexpected reply: {other:?}")),
        Err(e) => Err(explain(&e)),
    };
    let body = html! { div.grid {
        section.card {
            h2 { "Configuration in effect" span.n { "the files merged, with the values set at run time" } }
            p.muted { "Edit the files in " code { "/etc/tessera/" } ", then reload." }
            form method="post" action="/config/reload" {
                input type="hidden" name="csrf" value=(s.csrf);
                button.btn type="submit" { "Reload the configuration files" }
            }
            @match &text {
                Ok(t) => { pre { (t) } }
                Err(e) => { (error_box(e)) }
            }
        }
    } };
    page(
        &st,
        &token,
        &s,
        &headers,
        view("Configuration", "/config"),
        &body,
    )
}

/// `POST /config/reload`.
pub async fn reload(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<TokenForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let o = drive(&st, &s, vec![Command::ReloadConfig]).await;
    settle(
        &st,
        &token,
        &s,
        &headers,
        "/config",
        |_| "Configuration reloaded.".into(),
        o,
    )
}
