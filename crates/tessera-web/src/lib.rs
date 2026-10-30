//! `tessera-web`: the web portal of a tessera device (ADR 0013).
//!
//! A client of the daemon's `control.sock`, like `tesseractl`: it translates
//! HTTP to the typed commands and queries of `tessera-control-types` and
//! keeps audio operations within that command model. Standalone OS networking
//! uses a separate typed, privileged helper. Two surfaces: server-rendered pages with
//! optional live updates and a JSON API under `/api/v1/`. Every route except the login
//! page needs a session; every state-changing request needs the session's
//! CSRF token.
//!
//! The library exists so the binary and its tests share code; it is not a
//! public API.
#![warn(missing_docs)]

pub mod api;
pub mod appliance;
pub mod auth;
pub mod backend;
pub mod config;
mod live;
pub mod network_profiles;
pub mod pages;
pub mod routing_peers;
pub mod signal;
pub mod theme;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::Router;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tessera_control_types::{CommandError, Reply, RequestBody};
use tesseractl::client::ClientError;

use crate::auth::{COOKIE, LoginLimiter, PasswordFile, Session, Sessions};
use crate::backend::{Backend, EventHub};

/// Everything the handlers share.
#[derive(Debug)]
pub struct AppState {
    /// The daemon.
    pub backend: Arc<dyn Backend>,
    /// Routing-only receiver-host API federation.
    pub routing_peers: routing_peers::Federation,
    /// Recent events and API subscribers.
    pub hub: EventHub,
    /// Logged-in clients.
    pub sessions: Sessions,
    /// Failed-login limits.
    pub limiter: LoginLimiter,
    /// The operator password.
    pub password: PasswordFile,
    /// Mark the cookie `Secure`.
    pub cookie_secure: bool,
    /// The previous flow counters, to tell which paths carry audio now.
    pub flow_sample: Mutex<Option<signal::Sample>>,
    /// One-shot messages for the next page, per session token.
    pub flash: Mutex<HashMap<String, Flash>>,
    /// What the top bar calls this device (name · primary address), from the
    /// last status seen; pages that do not ask the daemon show the last one.
    pub host: Mutex<Option<String>>,
    /// Appliance mode, when `[appliance] enabled = true`.
    pub appliance: Option<Arc<appliance::Appliance>>,
    /// Core service controls, also available outside appliance mode.
    pub service_manager: Arc<appliance::Appliance>,
    /// Standalone host networking, kept outside the audio daemon's privileges.
    pub network: Arc<dyn network_profiles::Backend>,
    /// Per-session browser drafts; no host changes until explicitly applied.
    pub network_drafts: Mutex<HashMap<String, tessera_state::network_profiles::Profiles>>,
    /// Wrong network confirmation tokens per client, separate from login limits.
    pub confirm_limiter: network_profiles::ConfirmLimiter,
}

/// A message shown once on the next page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flash {
    /// Success or failure.
    pub ok: bool,
    /// The text.
    pub text: String,
}

/// Shared state handle.
pub type Shared = Arc<AppState>;

impl AppState {
    /// State for `backend` with the given password file and session timeout.
    #[must_use]
    pub fn new(
        backend: Arc<dyn Backend>,
        password: PasswordFile,
        session_idle: std::time::Duration,
        cookie_secure: bool,
    ) -> Self {
        Self {
            backend,
            routing_peers: Default::default(),
            hub: EventHub::new(200),
            sessions: Sessions::new(session_idle, 64),
            limiter: LoginLimiter::new(std::time::Duration::from_secs(60), 5, 30),
            password,
            cookie_secure,
            flow_sample: Mutex::new(None),
            flash: Mutex::new(HashMap::new()),
            host: Mutex::new(None),
            appliance: None,
            service_manager: Arc::new(appliance::Appliance::new(
                config::WebConfig::default().services_config(),
                Arc::new(appliance::services::ProcessRunner),
            )),
            network: Arc::new(network_profiles::SocketBackend),
            network_drafts: Mutex::new(HashMap::new()),
            confirm_limiter: network_profiles::ConfirmLimiter::default(),
        }
    }

    /// Configure explicit peers without changing daemon authority.
    pub fn with_routing_peers(mut self, peers: routing_peers::Federation) -> Self {
        self.routing_peers = peers;
        self
    }

    /// Turn appliance mode on (the Speaker page, the service switches).
    #[must_use]
    pub fn with_appliance(mut self, a: appliance::Appliance) -> Self {
        self.appliance = Some(Arc::new(a));
        self
    }

    /// Configure the Services page independently of the Speaker landing page.
    #[must_use]
    pub fn with_services(mut self, a: appliance::Appliance) -> Self {
        self.service_manager = Arc::new(a);
        self
    }

    /// Remember how the top bar names this device.
    pub fn set_host(&self, label: Option<String>) {
        if let Ok(mut h) = self.host.lock() {
            *h = label;
        }
    }

    /// The device's name for the top bar, if any status was seen.
    #[must_use]
    pub fn host(&self) -> Option<String> {
        self.host.lock().ok().and_then(|h| h.clone())
    }

    /// Remember a message for the next page of this session.
    pub fn set_flash(&self, token: &str, ok: bool, text: impl Into<String>) {
        if let Ok(mut m) = self.flash.lock() {
            m.insert(
                token.to_owned(),
                Flash {
                    ok,
                    text: text.into(),
                },
            );
        }
    }

    /// Take the message for this session, if any.
    pub fn take_flash(&self, token: &str) -> Option<Flash> {
        self.flash.lock().ok()?.remove(token)
    }
}

/// The application: pages, API, static files and the security layers.
pub fn router(state: Shared) -> Router {
    // Appliance mode owns `/` (a redirect to `/speaker`) and adds its pages;
    // otherwise `/` is the overview and none of its paths exist.
    let root = match &state.appliance {
        Some(a) => appliance::routes(&a.config),
        None => Router::new().route("/", get(pages::overview)),
    };
    root.route(
        "/services",
        get(appliance::pages::services_page).post(appliance::pages::services_post),
    )
    .route("/login", get(pages::login_form).post(pages::login))
    .route("/logout", post(pages::logout))
    .route("/network", get(pages::network))
    .route("/network/primary", post(pages::set_primary))
    .route("/network/profiles/edit", post(network_profiles::edit))
    .route("/network/profiles/preview", post(network_profiles::preview))
    .route("/network/profiles/apply", post(network_profiles::apply))
    .route("/network/pending/{token}", get(network_profiles::pending))
    .route(
        "/network/pending/{token}/{action}",
        post(network_profiles::finish),
    )
    .merge(network_profiles::token_routes())
    .route("/devices", get(pages::devices))
    .route("/routing", get(pages::routing::receivers))
    .route("/routing/review", get(pages::routing::review))
    .route("/routing/recovery", get(pages::routing::inspect))
    .route(
        "/routing/carrier-recovery",
        get(pages::routing::inspect_carriers),
    )
    .route(
        "/routing/peer-operation",
        get(pages::routing::peer_operation).post(pages::routing::retire_peer_operation),
    )
    .route("/routing/apply", post(pages::routing::apply))
    .route("/devices/{key}/settings", post(pages::device_settings))
    .route("/devices/{key}/channels", post(pages::device_channels))
    .route("/confirm", post(pages::confirm))
    .route("/devices/{key}/label", post(pages::device_label))
    .route("/devices/{key}/lock", post(pages::device_lock))
    .route("/devices/{key}/unlock", post(pages::device_unlock))
    .route("/devices/{key}/subscribe", post(pages::subscribe))
    .route(
        "/devices/{key}/mapping",
        get(pages::mapping::picker).post(pages::mapping::save),
    )
    .route("/devices/{key}/unsubscribe", post(pages::unsubscribe))
    .route("/clock", get(pages::clock).post(pages::clock_settings))
    .route("/metrics", get(pages::metrics))
    .route("/events", get(pages::events))
    .route("/config", get(pages::config))
    .route("/config/reload", post(pages::reload))
    .route("/theme", post(theme::set_theme))
    .route("/assets/{hash}/style.css", get(theme::stylesheet_immutable))
    .route("/style.css", get(theme::stylesheet))
    .route("/live.js", get(live::script))
    .route("/static/style.css", get(theme::stylesheet))
    .nest("/api/v1", api::routes())
    .fallback(pages::not_found)
    .layer(middleware::from_fn(same_origin))
    .layer(middleware::from_fn(security_headers))
    .with_state(state)
}

/// Headers on every response.
async fn security_headers(req: Request, next: Next) -> Response {
    let mut r = next.run(req).await;
    let h = r.headers_mut();
    let set = |h: &mut HeaderMap, k: &'static str, v: &'static str| {
        h.insert(k, HeaderValue::from_static(v));
    };
    set(
        h,
        "content-security-policy",
        "default-src 'none'; style-src 'self'; script-src 'self'; connect-src 'self'; img-src 'self'; form-action 'self'; \
         frame-ancestors 'none'; base-uri 'none'",
    );
    set(h, "x-frame-options", "DENY");
    set(h, "x-content-type-options", "nosniff");
    // Not `no-referrer`: with it, browsers send `Origin: null` on form posts,
    // which the origin check below must refuse.
    set(h, "referrer-policy", "same-origin");
    // The content-hashed stylesheet sets its own (immutable) caching.
    if !h.contains_key(header::CACHE_CONTROL) {
        set(h, "cache-control", "no-store");
    }
    r
}

/// A state-changing request whose `Origin` names another site is refused
/// (with `SameSite=Strict` cookies and CSRF tokens, a second line).
async fn same_origin(req: Request, next: Next) -> Response {
    if !network_profiles::is_token_alias(req.uri().path())
        && !matches!(*req.method(), Method::GET | Method::HEAD)
    {
        if let Some(origin) = req.headers().get(header::ORIGIN) {
            let host = req
                .headers()
                .get(header::HOST)
                .and_then(|h| h.to_str().ok());
            let origin_host = origin
                .to_str()
                .ok()
                .and_then(|o| o.split_once("://"))
                .map(|(_, rest)| rest.trim_end_matches('/'));
            if host.is_none() || origin_host != host {
                if req.uri().path().starts_with("/api/")
                    || network_profiles::wants_json(req.headers())
                {
                    return api::error(StatusCode::FORBIDDEN, "cross-origin request refused");
                }
                return (StatusCode::FORBIDDEN, "cross-origin request refused").into_response();
            }
        }
    }
    next.run(req).await
}

/// The value of cookie `name` from the `Cookie` header.
#[must_use]
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_owned())
}

/// The session token from the `Cookie` header.
#[must_use]
pub fn cookie_token(headers: &HeaderMap) -> Option<String> {
    cookie_value(headers, COOKIE)
}

/// The session of this request, if it has a valid one: `(token, session)`.
#[must_use]
pub fn session_of(st: &AppState, headers: &HeaderMap) -> Option<(String, Session)> {
    let token = cookie_token(headers)?;
    let s = st.sessions.get(&token, Instant::now())?;
    Some((token, s))
}

/// The `Set-Cookie` value that starts a session.
#[must_use]
pub fn session_cookie(token: &str, secure: bool) -> String {
    format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict{}",
        if secure { "; Secure" } else { "" }
    )
}

/// The `Set-Cookie` value that ends a session.
#[must_use]
pub fn clear_cookie(secure: bool) -> String {
    format!(
        "{COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",
        if secure { "; Secure" } else { "" }
    )
}

/// The client's address (loopback when the server did not record it).
#[must_use]
pub fn peer(ext: &axum::http::Extensions) -> IpAddr {
    if let Some(ConnectInfo(c)) = ext.get::<ConnectInfo<network_profiles::Connection>>() {
        return c.remote.ip();
    }
    ext.get::<ConnectInfo<SocketAddr>>()
        .map_or(IpAddr::from([127, 0, 0, 1]), |c| c.0.ip())
}

/// Ask the daemon, on the blocking pool.
pub async fn call(st: &AppState, body: RequestBody) -> Result<Reply, ClientError> {
    if let Some(reply) = st.routing_peers.dispatch(st, &body).await {
        return reply;
    }
    local_call(st, body).await
}

/// Ask only the local daemon; machine requests cannot recursively federate.
pub(crate) async fn local_call(st: &AppState, body: RequestBody) -> Result<Reply, ClientError> {
    let b = Arc::clone(&st.backend);
    tokio::task::spawn_blocking(move || b.request(body))
        .await
        .unwrap_or_else(|e| Err(ClientError::Protocol(e.to_string())))
}

/// What to tell an operator about a failed request.
#[must_use]
pub fn explain(e: &ClientError) -> String {
    match e {
        ClientError::Command(CommandError::PermissionDenied) => {
            "The portal may only read: add the user `tessera-web` to the daemon's admin \
             group (`[daemon] admin_group`, default `tessera-admin`) and restart tessera-web."
                .into()
        }
        ClientError::Connect { .. } => format!("The daemon is not reachable: {e}"),
        other => other.to_string(),
    }
}
