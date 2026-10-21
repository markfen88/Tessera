//! Standalone VLAN editor and token-scoped management recovery.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::Form;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use maud::{Markup, html};
use serde::Deserialize;
use tessera_state::network_profiles::{
    Command, Ipv4, NETWORK_API, Network, Profiles, Reply, Request,
};

use crate::Shared;
use crate::auth::Session;
use crate::pages::{Chrome, error_box, layout, need_post, view};

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
        tessera_network::request(
            std::path::Path::new(tessera_network::SOCKET),
            &Request {
                api: NETWORK_API,
                command,
            },
        )
    }
}

async fn exchange(st: &Shared, command: Command) -> Result<Reply, String> {
    let backend = Arc::clone(&st.network);
    let reply = tokio::task::spawn_blocking(move || backend.request(command))
        .await
        .map_err(|e| e.to_string())??;
    if reply.api != NETWORK_API {
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
        html! { section.card { h2 { "VLAN configuration unavailable" } (error_box(&error))
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
                        p.muted { "When moving management, clear the old network's gateway and DNS in the draft before applying." }
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
                    (network_form(&Network { id: String::new(), name: String::new(), vlan_id: None, ipv4: Ipv4::Dhcp }, &session.csrf, true))
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
                || f.name.len() > 64
                || f.name.chars().any(char::is_control)
                || p.networks
                    .iter()
                    .any(|n| n.id != id && n.vlan_id == vlan_id)
            {
                return Err("Use a unique VLAN and a valid network name.".into());
            }
            let row = Network {
                id: id.clone(),
                name: f.name.clone(),
                vlan_id,
                ipv4,
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
pub async fn preview(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<CsrfForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let result = async {
        let current = call(&st, Command::Status {})
            .await?
            .profiles
            .ok_or("Networking unavailable")?;
        let candidate = draft(&st, &token, &current);
        call(
            &st,
            Command::Preview {
                profiles: candidate.clone(),
            },
        )
        .await?;
        let removed: Vec<_> = current
            .networks
            .iter()
            .filter(|n| !candidate.networks.iter().any(|c| c.id == n.id))
            .map(|n| n.name.clone())
            .collect();
        Ok::<_, String>((candidate, removed))
    }
    .await;
    let body = match result {
        Err(e) => error_box(&e),
        Ok((p, removed)) => html! { section.card {
            h2 { "Apply network settings?" }
            p.flash.bad { "Network access and audio may be interrupted. Open the resulting management address and click Keep changes within 60 seconds. Otherwise the appliance restores its previous settings." }
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
pub async fn apply(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<CsrfForm>,
) -> Response {
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
            Command::Apply {
                profiles: draft(&st, &token, &current),
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
    let local = request
        .extensions()
        .get::<ConnectInfo<Connection>>()
        .and_then(|c| c.0.local)
        .and_then(|s| match s.ip() {
            std::net::IpAddr::V4(ip) => Some(ip),
            std::net::IpAddr::V6(ip) => ip.to_ipv4_mapped(),
        });
    let reply = call(
        &st,
        Command::Pending {
            token: token.clone(),
        },
    )
    .await;
    let body = match reply {
        Err(e) => {
            html! { section.card { (error_box(&e)) a.btn href="/network" { "Return to Network" } } }
        }
        Ok(reply) => html! { section.card {
            h2 { "Keep network changes?" }
            @if let Some(p) = reply.pending {
                p { (p.phase) " · automatic recovery in " (p.seconds) " seconds" }
                @if let Some(e) = p.error { (error_box(&e)) }
                @if let Some(ip) = p.management_address {
                    p { "Open the new management address: " a href=(management_url(ip, p.management_port, &token)) { (ip) ":" (p.management_port) } }
                }
                p { "Opening this page does not confirm the change. Keep changes works only through the new management address." }
                form method="post" action=(format!("/network/pending/{token}/keep")) {
                    button.btn.primary type="submit" disabled[p.phase != "ready" || local.is_none() || local != p.management_address] { "Keep changes" }
                }
                form method="post" action=(format!("/network/pending/{token}/revert")) {
                    button.btn type="submit" { "Revert" }
                }
            }
        } },
    };
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

/// Keep only through the actual new management socket, or explicitly revert.
pub async fn finish(
    State(st): State<Shared>,
    Path((token, action)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    let command = match action.as_str() {
        "revert" => Command::Revert { token },
        "keep" => {
            let local = request
                .extensions()
                .get::<ConnectInfo<Connection>>()
                .and_then(|c| c.0.local)
                .and_then(|s| match s.ip() {
                    std::net::IpAddr::V4(ip) => Some(ip),
                    std::net::IpAddr::V6(ip) => ip.to_ipv4_mapped(),
                });
            let Some(local_address) = local else {
                return (
                    StatusCode::FORBIDDEN,
                    "Cannot verify the management socket address.",
                )
                    .into_response();
            };
            Command::Confirm {
                token,
                local_address,
            }
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    match call(&st, command).await {
        Ok(_) => Redirect::to("/network").into_response(),
        Err(e) => (StatusCode::CONFLICT, e).into_response(),
    }
}
