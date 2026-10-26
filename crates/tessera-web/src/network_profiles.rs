//! Standalone VLAN editor and token-scoped management recovery.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, FromRequest, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Form, Json};
use maud::{Markup, html};
use serde::Deserialize;
use tessera_state::network_profiles::{
    Command, ConfirmPolicy, ConfirmationError, Document, Ipv4, MAX_DNS_SEARCH, NETWORK_API,
    NETWORK_DOCUMENT_API, Network, Profiles, Reply, Request, SessionPath, valid_dns_search_domain,
};

use crate::Shared;
use crate::auth::Session;
use crate::pages::{Chrome, error_box, layout, need_post, view};

/// Refusals of the helper that the administrator settles by naming the wired
/// port in `[network] interface` (the typed atoms of `Host::adopt`).
const PORT_REFUSALS: &[&str] = &[
    "no_wired_interface",
    "ambiguous_management_port",
    "wired_port_required",
    "unsupported_link_kind",
    "attached_interfaces",
    "unknown_backend",
];

/// NE-7.6/U15: one failure-only rolling budget for all token routes.
#[derive(Debug, Default)]
pub struct ConfirmLimiter(Mutex<HashMap<IpAddr, Vec<Instant>>>);

impl ConfirmLimiter {
    fn allowed(&self, addr: IpAddr, now: Instant) -> bool {
        let Ok(mut clients) = self.0.lock() else {
            return false;
        };
        Self::expire(&mut clients, now);
        let addr = normalized(addr);
        match clients.get(&addr) {
            Some(hits) => hits.len() < 5,
            // Bound memory without evicting another client's live failures.
            None => clients.len() < 4096,
        }
    }

    fn failed(&self, addr: IpAddr, now: Instant, kind: Option<&ConfirmationError>) {
        if kind.is_none() || matches!(kind, Some(ConfirmationError::TokenMismatch)) {
            if let Ok(mut clients) = self.0.lock() {
                Self::expire(&mut clients, now);
                let addr = normalized(addr);
                if clients.contains_key(&addr) || clients.len() < 4096 {
                    let hits = clients.entry(addr).or_default();
                    if hits.len() < 5 {
                        hits.push(now);
                    }
                }
            }
        }
    }

    fn expire(clients: &mut HashMap<IpAddr, Vec<Instant>>, now: Instant) {
        clients.retain(|_, hits| {
            hits.retain(|hit| now.saturating_duration_since(*hit) < Duration::from_secs(60));
            !hits.is_empty()
        });
    }
}

fn normalized(addr: IpAddr) -> IpAddr {
    match addr {
        IpAddr::V6(ip) => ip.to_ipv4_mapped().map_or(addr, IpAddr::V4),
        _ => addr,
    }
}

#[derive(Debug)]
struct Failure {
    kind: Option<ConfirmationError>,
    message: String,
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self {
            kind: None,
            message,
        }
    }
}

impl Failure {
    fn code(&self) -> StatusCode {
        match &self.kind {
            Some(ConfirmationError::NotPending | ConfirmationError::Expired) => StatusCode::GONE,
            Some(ConfirmationError::StillApplying | ConfirmationError::WrongAddress) => {
                StatusCode::CONFLICT
            }
            Some(ConfirmationError::TokenMismatch) => StatusCode::FORBIDDEN,
            Some(ConfirmationError::RateLimited) => StatusCode::TOO_MANY_REQUESTS,
            Some(ConfirmationError::Invalid { .. }) => StatusCode::BAD_REQUEST,
            Some(ConfirmationError::Unsupported { .. }) => StatusCode::NOT_IMPLEMENTED,
            Some(ConfirmationError::Locked { .. }) => StatusCode::CONFLICT,
            Some(ConfirmationError::RevisionConflict { .. }) => StatusCode::CONFLICT,
            // A recovery that did not verify yet is a state of the transaction, not a fault of
            // this request: the helper keeps the journal and retries (HW-3, D15).
            Some(ConfirmationError::ApplyFailed { step, .. }) if step == "recover" => {
                StatusCode::CONFLICT
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn page_message(&self) -> &str {
        match &self.kind {
            Some(ConfirmationError::NotPending) => "No network change is waiting for confirmation.",
            Some(ConfirmationError::Expired) => {
                "The network confirmation window has expired. The appliance restores its previous settings."
            }
            Some(ConfirmationError::StillApplying) => {
                "The network change is still being applied or recovered. Try again in a moment."
            }
            Some(ConfirmationError::TokenMismatch) => {
                "The confirmation token does not match the pending network change."
            }
            Some(ConfirmationError::WrongAddress) => {
                "Open the new management address directly to keep these settings."
            }
            Some(ConfirmationError::RateLimited) => {
                "Too many confirmation token mismatches; wait a minute."
            }
            _ => &self.message,
        }
    }

    fn response(&self, st: &Shared, headers: &HeaderMap) -> Response {
        if wants_json(headers) {
            let kind = self.kind.as_ref().map_or_else(
                || serde_json::json!({ "internal": { "message": self.message } }),
                |kind| serde_json::json!(kind),
            );
            return (
                self.code(),
                Json(serde_json::json!({ "error": kind, "message": self.message })),
            )
                .into_response();
        }
        let body = html! { section.card {
            (error_box(self.page_message()))
            a.btn href="/network" { "Return to Network" }
        } };
        (
            self.code(),
            layout(
                &view("Keep network changes", "/network"),
                &Chrome::of(st, headers),
                None,
                None,
                &body,
            ),
        )
            .into_response()
    }
}

pub(crate) fn wants_json(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|v| v.trim().starts_with("application/json"))
        })
}

/// Actual socket endpoints supplied by the HTTP listener.
#[derive(Debug, Clone, Copy)]
pub struct Connection {
    /// Peer, used for login rate limits.
    pub remote: SocketAddr,
    /// Destination, used for management confirmation, never a forwarded header.
    pub local: Option<SocketAddr>,
}

impl
    axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, tokio::net::TcpListener>>
    for Connection
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        Self {
            remote: *stream.remote_addr(),
            local: stream.io().local_addr().ok(),
        }
    }
}

/// Injectable helper boundary; HTTP tests need neither root nor a listener.
pub trait Backend: std::fmt::Debug + Send + Sync {
    /// Execute a typed networking command.
    fn request(&self, command: Command) -> Result<Reply, String>;
}

/// Production client of the root-owned Unix socket.
#[derive(Debug)]
pub struct SocketBackend;
impl Backend for SocketBackend {
    fn request(&self, command: Command) -> Result<Reply, String> {
        let policy = if matches!(
            &command,
            Command::Apply { .. }
                | Command::Preview { .. }
                | Command::ApplyFrom { .. }
                | Command::PreviewFrom { .. }
        ) {
            tessera_network::request(
                std::path::Path::new(tessera_network::SOCKET),
                &Request {
                    api: NETWORK_DOCUMENT_API,
                    command: Command::Status {},
                },
            )?
            .confirm_policy
        } else {
            ConfirmPolicy::default()
        };
        let command = match command {
            Command::Preview { profiles } => Command::PreviewDocument {
                document: Document::from_profiles(profiles, policy),
                session: None,
            },
            Command::Apply { profiles } => Command::ApplyDocument {
                document: Document::from_profiles(profiles, policy),
                session: None,
            },
            Command::PreviewFrom { profiles, session } => Command::PreviewDocument {
                document: Document::from_profiles(profiles, policy),
                session: Some(session),
            },
            Command::ApplyFrom { profiles, session } => Command::ApplyDocument {
                document: Document::from_profiles(profiles, policy),
                session: Some(session),
            },
            command => command,
        };
        let mut reply = tessera_network::request(
            std::path::Path::new(tessera_network::SOCKET),
            &Request {
                api: NETWORK_DOCUMENT_API,
                command,
            },
        )?;
        // The server-rendered editor uses Rust u64 throughout. Keep the API-2
        // document for JSON clients and adapt only its in-memory draft view.
        reply.profiles = reply.document.clone().map(Document::into_profiles);
        Ok(reply)
    }
}

async fn exchange(st: &Shared, command: Command) -> Result<Reply, String> {
    let backend = Arc::clone(&st.network);
    let reply = tokio::task::spawn_blocking(move || backend.request(command))
        .await
        .map_err(|e| e.to_string())??;
    if reply.api != NETWORK_API && reply.api != NETWORK_DOCUMENT_API {
        return Err("Incompatible networking packages; install matching versions.".into());
    }
    Ok(reply)
}

async fn call(st: &Shared, command: Command) -> Result<Reply, String> {
    let reply = exchange(st, command).await?;
    if let Some(error) = &reply.error {
        return Err(error.clone());
    }
    Ok(reply)
}

async fn token_call(st: &Shared, command: Command) -> Result<Reply, Failure> {
    let reply = exchange(st, command).await?;
    if let Some(message) = &reply.error {
        return Err(Failure {
            kind: reply.error_kind,
            message: message.clone(),
        });
    }
    Ok(reply)
}

fn draft(st: &Shared, token: &str, current: &Profiles) -> Profiles {
    st.network_drafts
        .lock()
        .ok()
        .and_then(|d| d.get(token).cloned())
        .unwrap_or_else(|| current.clone())
}

/// Prefer the protected editor when available; retain legacy interface controls
/// on systems without the optional standalone helper.
pub(crate) async fn page(
    st: &Shared,
    token: &str,
    session: &Session,
    headers: &HeaderMap,
) -> Option<Response> {
    let reply = exchange(st, Command::Status {}).await.ok()?;
    let body = if let Some(error) = reply.error {
        let choose_port = matches!(&reply.error_kind, Some(ConfirmationError::Unsupported { what })
            if PORT_REFUSALS.contains(&what.as_str()));
        html! { section.card { h2 { "VLAN configuration unavailable" } (error_box(&error))
            @if choose_port {
                p { "To choose the wired management port yourself, set " code { "interface = \"<port>\"" } " under " code { "[network]" } " in " code { "/etc/tessera/web.toml" } " or in a file in " code { "/etc/tessera/web.d/" } "." }
            }
            p { "The current network settings have not been changed." }
        } }
    } else if reply.externally_managed {
        html! { section.card { h2 { "Networking managed by NDIMon" }
            p { "Configure VLAN networks and management in NDIMon's Network settings. Select the audio network in its Tessera plugin." }
            p.muted { "Standalone controls are unavailable while NDIMon owns this appliance's networking." }
        } }
    } else {
        let current = reply.profiles?;
        let candidate = draft(st, token, &current);
        html! {
            @if let Some(pending) = reply.pending {
                section.card { h2 { "Network change pending" }
                    p { (pending.phase) " · " (pending.seconds) " seconds remaining. Use the confirmation page opened when you applied the change." }
                    @if let Some(error) = pending.error { (error_box(&error)) }
                }
            } @else {
                section.card { h2 { "VLAN networks" }
                    p { "Wired port: " code { (current.interface) } ". Configure the switch trunk before applying tagged networks. An empty VLAN ID means untagged." }
                    p.muted { "Edits stay in your draft until Apply network settings. Management provides the web interface, default gateway and DNS. Audio carries discovery, controller traffic, audio and PTP." }
                    @if current != candidate { p.flash { "You have unapplied network changes." } }
                    @if current.revision != candidate.revision { (error_box("This draft is stale. Discard it and reload the current settings.")) }
                }
                section.card { h2 { "Assignments" }
                    form.fields method="post" action="/network/profiles/edit" {
                        input type="hidden" name="csrf" value=(session.csrf);
                        input type="hidden" name="action" value="assign";
                        @for (name, label, selected) in [("management", "Management", &candidate.management), ("audio", "Audio network", &candidate.audio)] {
                            label { (label) select name=(name) aria-label=(label) { @for network in &candidate.networks {
                                option value=(network.id) selected[network.id == *selected] { (network.name) }
                            } } }
                        }
                        button.btn type="submit" { "Save assignments to draft" }
                        p.muted { "When moving management, clear the old network's gateway, DNS and search domains in the draft before applying." }
                    }
                }
                @for network in &candidate.networks {
                    section.card { h2 { (network.name) }
                        p { "Used by: " @if network.id == candidate.management { "Management " } @if network.id == candidate.audio { "Audio" }
                            @if network.id != candidate.management && network.id != candidate.audio { "Unassigned" }
                        }
                        (network_form(network, &session.csrf, false))
                        form method="post" action="/network/profiles/edit" {
                            input type="hidden" name="csrf" value=(session.csrf);
                            input type="hidden" name="id" value=(network.id);
                            button.btn type="submit" name="action" value="delete" disabled[network.id == candidate.management || network.id == candidate.audio] { "Remove from draft" }
                            p.muted { "Assigned networks require a replacement first. Removal takes effect only after the final apply confirmation." }
                        }
                    }
                }
                details.card { summary { "Add network" }
                    (network_form(&Network { id: String::new(), name: String::new(), vlan_id: None, ipv4: Ipv4::Dhcp, dns_search: Vec::new() }, &session.csrf, true))
                }
                section.card { div.actions {
                    form method="post" action="/network/profiles/preview" {
                        input type="hidden" name="csrf" value=(session.csrf);
                        button.btn.primary type="submit" disabled[current == candidate || current.revision != candidate.revision] { "Apply network settings" }
                    }
                    form method="post" action="/network/profiles/edit" {
                        input type="hidden" name="csrf" value=(session.csrf);
                        button.btn type="submit" name="action" value="reset" { "Discard draft" }
                    }
                } }
            }
        }
    };
    let body = html! { div.grid.network-profiles { (body) } };
    Some(crate::pages::page(
        st,
        token,
        session,
        headers,
        view("Network", "/network"),
        &body,
    ))
}

fn network_form(n: &Network, csrf: &str, add: bool) -> Markup {
    let (mode, address, prefix, gateway, dns) = match &n.ipv4 {
        Ipv4::Dhcp => ("dhcp", String::new(), 24, String::new(), String::new()),
        Ipv4::Static {
            address,
            prefix,
            gateway,
            dns,
        } => (
            "static",
            address.to_string(),
            *prefix,
            gateway.map_or_else(String::new, |v| v.to_string()),
            dns.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        ),
    };
    let search = n.dns_search.join(", ");
    html! { form.fields method="post" action="/network/profiles/edit" {
        input type="hidden" name="csrf" value=(csrf);
        input type="hidden" name="action" value=(if add { "add" } else { "save" });
        input type="hidden" name="id" value=(n.id);
        label { "Name" input name="name" value=(n.name) maxlength="64" required; }
        label { "VLAN ID (empty = untagged)" input name="vlan_id" type="number" min="1" max="4094" value=(n.vlan_id.map_or_else(String::new, |v| v.to_string())); }
        label { "IPv4" select name="mode" aria-label="IPv4" {
            option value="dhcp" selected[mode == "dhcp"] { "DHCP" }
            option value="static" selected[mode == "static"] { "Static" }
        } }
        details { summary { "Static address settings" }
            label { "IPv4 address" input name="address" value=(address); }
            label { "Prefix length" input name="prefix" type="number" min="8" max="30" value=(prefix); }
            label { "Gateway (management only)" input name="gateway" value=(gateway); }
            label { "DNS servers (management only, separated by commas)" input name="dns" value=(dns); }
        }
        label { "Search domains (management only, comma separated, optional)" input name="dns_search" value=(search); }
        button.btn type="submit" { (if add { "Add to draft" } else { "Save to draft" }) }
    } }
}

/// All changes are drafts until a separate preview and apply.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct EditForm {
    csrf: String,
    action: String,
    id: String,
    name: String,
    vlan_id: String,
    mode: String,
    address: String,
    prefix: String,
    gateway: String,
    dns: String,
    dns_search: String,
    management: String,
    audio: String,
}

fn edit_draft(p: &mut Profiles, f: &EditForm) -> Result<(), String> {
    match f.action.as_str() {
        "assign" => {
            p.assigned(&f.management)?;
            p.assigned(&f.audio)?;
            p.management = f.management.clone();
            p.audio = f.audio.clone();
        }
        "delete" => {
            if f.id == p.management || f.id == p.audio {
                return Err("Select a replacement before removing an assigned network.".into());
            }
            p.networks.retain(|n| n.id != f.id);
        }
        "save" | "add" => {
            let vlan_id = if f.vlan_id.trim().is_empty() {
                None
            } else {
                Some(f.vlan_id.parse::<u16>().map_err(|_| "Invalid VLAN ID")?)
            };
            if vlan_id.is_some_and(|v| !(1..=4094).contains(&v)) {
                return Err("VLAN ID must be 1 to 4094.".into());
            }
            let ipv4 = match f.mode.as_str() {
                "dhcp" => Ipv4::Dhcp,
                "static" => Ipv4::Static {
                    address: f.address.parse().map_err(|_| "Invalid IPv4 address")?,
                    prefix: f.prefix.parse().map_err(|_| "Invalid prefix length")?,
                    gateway: if f.gateway.trim().is_empty() {
                        None
                    } else {
                        Some(f.gateway.parse().map_err(|_| "Invalid gateway")?)
                    },
                    dns: f
                        .dns
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(|s| s.parse::<Ipv4Addr>().map_err(|_| "Invalid DNS address"))
                        .collect::<Result<_, _>>()?,
                },
                _ => return Err("Choose DHCP or static addressing.".into()),
            };
            // Search domains apply to DHCP and static addressing alike.
            let dns_search: Vec<String> = f
                .dns_search
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            if dns_search.len() > MAX_DNS_SEARCH {
                return Err("Use at most six DNS search domains.".into());
            }
            if !dns_search.iter().all(|d| valid_dns_search_domain(d)) {
                return Err(
                    "Use DNS search domains made of letters, digits, hyphens and dots.".into(),
                );
            }
            let mut id = if f.action == "add" {
                vlan_id.map_or_else(|| "untagged".into(), |v| format!("vlan-{v}"))
            } else {
                f.id.clone()
            };
            if f.action == "add"
                && p.networks
                    .iter()
                    .any(|n| n.id == id && n.vlan_id != vlan_id)
            {
                id = format!("network-{}", crate::auth::random_hex(8));
            }
            if f.name.trim().is_empty()
                || f.name.chars().count() > 64
                || f.name.chars().any(char::is_control)
                || p.networks
                    .iter()
                    .any(|n| n.id != id && n.vlan_id == vlan_id)
            {
                return Err("Use a unique VLAN and a valid network name.".into());
            }
            let row = Network {
                id: id.clone(),
                name: f.name.trim().to_owned(),
                vlan_id,
                ipv4,
                dns_search,
            };
            if f.action == "add" {
                if p.networks.iter().any(|n| n.id == id) {
                    return Err("That network already exists.".into());
                }
                p.networks.push(row);
            } else {
                *p.networks
                    .iter_mut()
                    .find(|n| n.id == id)
                    .ok_or("Network no longer exists")? = row;
            }
        }
        _ => return Err("Unknown network draft action.".into()),
    }
    Ok(())
}

/// Save a session draft; this route never changes host networking.
pub async fn edit(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<EditForm>,
) -> Response {
    let (token, _) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let result: Result<(), String> = async {
        let reply = call(&st, Command::Status {}).await?;
        if reply.externally_managed || reply.pending.is_some() {
            return Err("Networking is externally managed or a change is pending.".into());
        }
        let current = reply.profiles.ok_or("Networking is unavailable")?;
        let mut drafts = st.network_drafts.lock().map_err(|_| "Draft unavailable")?;
        if f.action == "reset" {
            drafts.remove(&token);
            return Ok(());
        }
        if !drafts.contains_key(&token) && drafts.len() >= 64 {
            return Err("Too many active network drafts; discard an old draft first.".into());
        }
        let mut candidate = drafts.get(&token).cloned().unwrap_or(current.clone());
        if current.revision != candidate.revision {
            return Err("Discard this stale draft and reload the page.".into());
        }
        edit_draft(&mut candidate, &f)?;
        drafts.insert(token.clone(), candidate);
        Ok(())
    }
    .await;
    st.set_flash(
        &token,
        result.is_ok(),
        result
            .err()
            .unwrap_or_else(|| "Network draft saved. No settings have been applied.".into()),
    );
    Redirect::to("/network").into_response()
}

/// CSRF-protected review/apply form.
#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

/// Preview the complete candidate before a disruptive apply.
pub async fn preview(State(st): State<Shared>, request: axum::extract::Request) -> Response {
    let headers = request.headers().clone();
    let session = session_path(request.extensions());
    let Form(f) = match Form::<CsrfForm>::from_request(request, &st).await {
        Ok(form) => form,
        Err(error) => return error.into_response(),
    };
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let result = async {
        let status = call(&st, Command::Status {}).await?;
        let current = status.profiles.ok_or("Networking unavailable")?;
        let candidate = draft(&st, &token, &current);
        call(
            &st,
            Command::PreviewFrom {
                profiles: candidate.clone(),
                session,
            },
        )
        .await?;
        let removed: Vec<_> = current
            .networks
            .iter()
            .filter(|n| !candidate.networks.iter().any(|c| c.id == n.id))
            .map(|n| n.name.clone())
            .collect();
        Ok::<_, String>((candidate, removed, status.confirm_policy))
    }
    .await;
    let body = match result {
        Err(e) => error_box(&e),
        Ok((p, removed, policy)) => html! { section.card {
            h2 { "Apply network settings?" }
            p.flash.bad {
                @if policy == ConfirmPolicy::TokenOnly {
                    "Network access and audio may be interrupted. Use the confirmation link from any reachable address and click Keep changes within 60 seconds. Otherwise the appliance restores its previous settings."
                } @else {
                    "Network access and audio may be interrupted. Open the resulting management address and click Keep changes within 60 seconds. Otherwise the appliance restores its previous settings."
                }
            }
            @if !removed.is_empty() { p.flash.bad { "These networks will be removed: " (removed.join(", ")) "." } }
            dl.kv { dt { "Management" } dd { (p.assigned(&p.management).map(|n| n.name.as_str()).unwrap_or("unknown")) }
                dt { "Audio" } dd { (p.assigned(&p.audio).map(|n| n.name.as_str()).unwrap_or("unknown")) } }
            @for n in &p.networks { p { (n.name) " · " (p.ifname(n)) " · " (address_label(&n.ipv4)) } }
            form method="post" action="/network/profiles/apply" {
                input type="hidden" name="csrf" value=(s.csrf);
                button.btn.primary type="submit" { "Apply and start recovery timer" }
                " " a.btn href="/network" { "Cancel" }
            }
        } },
    };
    crate::pages::page(
        &st,
        &token,
        &s,
        &headers,
        view("Apply network settings", "/network"),
        &body,
    )
}

fn address_label(v: &Ipv4) -> String {
    match v {
        Ipv4::Dhcp => "DHCP".into(),
        Ipv4::Static {
            address, prefix, ..
        } => format!("{address}/{prefix}"),
    }
}

/// Stage a root-owned change; carry its token to the new management origin.
pub async fn apply(State(st): State<Shared>, request: axum::extract::Request) -> Response {
    let headers = request.headers().clone();
    let session = session_path(request.extensions());
    let Form(f) = match Form::<CsrfForm>::from_request(request, &st).await {
        Ok(form) => form,
        Err(error) => return error.into_response(),
    };
    let (token, _) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let result: Result<String, String> = async {
        let current = call(&st, Command::Status {})
            .await?
            .profiles
            .ok_or("Networking unavailable")?;
        let reply = call(
            &st,
            Command::ApplyFrom {
                profiles: draft(&st, &token, &current),
                session,
            },
        )
        .await?;
        reply
            .token
            .ok_or_else(|| "Network helper did not return a transaction token.".into())
    }
    .await;
    match result {
        Ok(pending) => {
            if let Ok(mut d) = st.network_drafts.lock() {
                d.remove(&token);
            }
            Redirect::to(&format!("/network/pending/{pending}")).into_response()
        }
        Err(e) => {
            st.set_flash(&token, false, e);
            Redirect::to("/network").into_response()
        }
    }
}

/// Token-scoped recovery page, usable without the previous login cookie.
pub async fn pending(
    State(st): State<Shared>,
    Path(token): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let headers = request.headers().clone();
    let reply = limited_token_call(
        &st,
        crate::peer(request.extensions()),
        Command::Pending {
            token: token.clone(),
        },
    )
    .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => return e.response(&st, &headers),
    };
    if wants_json(&headers) {
        return json_reply(reply);
    }
    let body = html! { section.card {
        h2 { "Keep network changes?" }
        @if let Some(p) = reply.pending {
            p { (p.phase) " · automatic recovery in " (p.seconds) " seconds" }
            @if let Some(e) = p.error { (error_box(&e)) }
            @if let Some(ip) = p.management_address {
                p { "Open the new management address: " a href=(management_url(ip, p.management_port, &token)) { (ip) ":" (p.management_port) } }
            }
            p {
                @if reply.confirm_policy == ConfirmPolicy::TokenOnly {
                    "Opening this page does not confirm the change. The change can be kept from any reachable address with this link."
                } @else {
                    "Opening this page does not confirm the change. Keep changes works only through the new management address."
                }
            }
            form method="post" action=(format!("/network/pending/{token}/keep")) {
                button.btn.primary type="submit" { "Keep changes" }
            }
            form method="post" action=(format!("/network/pending/{token}/revert")) {
                button.btn type="submit" { "Revert" }
            }
        }
    } };
    layout(
        &view("Keep network changes", "/network").every(1),
        &Chrome::of(&st, &headers),
        None,
        None,
        &body,
    )
    .into_response()
}

fn management_url(address: Ipv4Addr, port: u16, token: &str) -> String {
    format!("http://{address}:{port}/network/pending/{token}")
}

/// Keep under the helper's policy, or explicitly revert; neither needs a cookie.
pub async fn finish(
    State(st): State<Shared>,
    Path((token, action)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    let headers = request.headers().clone();
    let addr = crate::peer(request.extensions());
    let command = match action.as_str() {
        "revert" => Command::Revert { token },
        "keep" => {
            Command::Confirm {
                token,
                // An unknown/IPv6 destination cannot satisfy the strict policy.
                // Token-only confirmation does not depend on this address.
                local_address: local_ipv4(request.extensions()),
            }
        }
        _ if wants_json(&headers) => {
            return crate::api::error(StatusCode::NOT_FOUND, "Unknown network action.");
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    match limited_token_call(&st, addr, command).await {
        Ok(_) if wants_json(&headers) => Json(serde_json::json!({ "ok": true })).into_response(),
        Ok(_) => Redirect::to("/network").into_response(),
        Err(e) => e.response(&st, &headers),
    }
}

fn session_path(extensions: &axum::http::Extensions) -> SessionPath {
    SessionPath {
        peer: normalized(crate::peer(extensions)),
        local: extensions
            .get::<ConnectInfo<Connection>>()
            .and_then(|c| c.0.local)
            .map(|socket| normalized(socket.ip())),
    }
}

fn local_ipv4(extensions: &axum::http::Extensions) -> Ipv4Addr {
    match session_path(extensions).local {
        Some(IpAddr::V4(ip)) => ip,
        _ => Ipv4Addr::UNSPECIFIED,
    }
}

fn rate_limited() -> Failure {
    Failure {
        kind: Some(ConfirmationError::RateLimited),
        message: "Too many confirmation token mismatches; wait a minute.".into(),
    }
}

async fn limited_token_call(st: &Shared, addr: IpAddr, command: Command) -> Result<Reply, Failure> {
    let now = Instant::now();
    if !st.confirm_limiter.allowed(addr, now) {
        return Err(rate_limited());
    }
    let result = token_call(st, command).await;
    if let Err(error) = &result {
        st.confirm_limiter.failed(addr, now, error.kind.as_ref());
    }
    result
}

/// Only these fixed endpoints use a scoped body token instead of a session.
pub(crate) fn is_token_alias(path: &str) -> bool {
    matches!(
        path,
        "/api/net-pending" | "/api/net-confirm" | "/api/net-revert"
    )
}

pub(crate) fn token_routes() -> axum::Router<Shared> {
    axum::Router::new()
        .route("/api/net-pending", axum::routing::post(json_pending))
        .route("/api/net-confirm", axum::routing::post(json_confirm))
        .route("/api/net-revert", axum::routing::post(json_revert))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenBody {
    token: String,
    // NDIMon compatibility flag; this alias always targets profile changes.
    #[serde(default, rename = "profiles")]
    _profiles: Option<bool>,
}

#[derive(Clone, Copy)]
enum TokenAction {
    Pending,
    Confirm,
    Revert,
}

async fn json_pending(State(st): State<Shared>, request: axum::extract::Request) -> Response {
    json_token(st, request, TokenAction::Pending).await
}

async fn json_confirm(State(st): State<Shared>, request: axum::extract::Request) -> Response {
    json_token(st, request, TokenAction::Confirm).await
}

async fn json_revert(State(st): State<Shared>, request: axum::extract::Request) -> Response {
    json_token(st, request, TokenAction::Revert).await
}

async fn json_token(st: Shared, request: axum::extract::Request, action: TokenAction) -> Response {
    let mut headers = request.headers().clone();
    headers.insert(
        axum::http::header::ACCEPT,
        axum::http::HeaderValue::from_static("application/json"),
    );
    let addr = crate::peer(request.extensions());
    // NE-7.5: a blocked client cannot bypass the budget with malformed input.
    if !st.confirm_limiter.allowed(addr, Instant::now()) {
        return rate_limited().response(&st, &headers);
    }
    let local_address = local_ipv4(request.extensions());
    let parsed = match axum::body::to_bytes(request.into_body(), 4096).await {
        Ok(body) => serde_json::from_slice::<TokenBody>(&body).ok(),
        Err(_) => None,
    };
    let Some(body) = parsed else {
        return Failure {
            kind: Some(ConfirmationError::Invalid {
                field: "request".into(),
                reason: "invalid_json".into(),
            }),
            message: "Supply a bounded JSON object with a transaction token.".into(),
        }
        .response(&st, &headers);
    };
    if body.token.len() != 64
        || !body
            .token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Failure {
            kind: Some(ConfirmationError::Invalid {
                field: "token".into(),
                reason: "invalid_token".into(),
            }),
            message: "The transaction token must be 64 lowercase hexadecimal characters.".into(),
        }
        .response(&st, &headers);
    }
    let command = match action {
        TokenAction::Pending => Command::Pending { token: body.token },
        TokenAction::Confirm => Command::Confirm {
            token: body.token,
            local_address,
        },
        TokenAction::Revert => Command::Revert { token: body.token },
    };
    match limited_token_call(&st, addr, command).await {
        Ok(reply) if matches!(action, TokenAction::Pending) => json_reply(reply),
        Ok(_) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(error) => error.response(&st, &headers),
    }
}

fn json_reply(mut reply: Reply) -> Response {
    if reply.document.is_some() {
        reply.profiles = None;
    }
    Json(reply).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ne_7_6_failure_window_is_rolling_and_normalizes_mapped_ipv4() {
        let limiter = ConfirmLimiter::default();
        let start = Instant::now();
        let addr: IpAddr = "192.0.2.20".parse().unwrap();
        let mapped: IpAddr = "::ffff:192.0.2.20".parse().unwrap();
        for second in 0..5 {
            let now = start + Duration::from_secs(second);
            assert!(limiter.allowed(addr, now));
            limiter.failed(mapped, now, Some(&ConfirmationError::TokenMismatch));
        }
        assert!(!limiter.allowed(addr, start + Duration::from_secs(59)));
        assert!(limiter.allowed(
            "192.0.2.21".parse().unwrap(),
            start + Duration::from_secs(59)
        ));
        assert!(limiter.allowed(addr, start + Duration::from_secs(60)));
        limiter.failed(
            addr,
            start + Duration::from_secs(60),
            Some(&ConfirmationError::WrongAddress),
        );
        assert!(limiter.allowed(addr, start + Duration::from_secs(60)));
    }

    #[test]
    fn ne_7_6_client_memory_is_bounded_and_expires() {
        let limiter = ConfirmLimiter::default();
        let start = Instant::now();
        for n in 0..4096u32 {
            limiter.failed(IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + n)), start, None);
        }
        let other: IpAddr = "192.0.2.20".parse().unwrap();
        assert!(!limiter.allowed(other, start));
        assert!(limiter.allowed(other, start + Duration::from_secs(60)));
        assert!(limiter.0.lock().unwrap().is_empty());
    }
}
