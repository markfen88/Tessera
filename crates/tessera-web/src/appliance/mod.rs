//! Appliance mode (`[appliance] enabled = true` in `web.toml`): the Speaker
//! landing page with one switch per configured service, the host's vitals
//! and the daemon's summary. Runtime configuration, not a cargo feature:
//! when it is off, nothing here is routed, the nav has no "Speaker" entry
//! and `/` is the overview as before.
//!
//! Privilege model: the portal stays unprivileged and never names a service
//! to PID 1. The three actions (enable, disable, restart) start one
//! instance of the root oneshot `tessera-web-unitfile@<verb>-<name>.service`
//! with plain `systemctl` as the portal's user; PID 1 asks polkit, and a
//! rules file generated from the same allowlist
//! (`tessera-web appliance polkit`, [`services::polkit_rules`]) answers yes
//! for exactly those instance units and the verb `start`. The helper
//! (`tessera-web appliance unitfile`, [`services::run_unitfile`]) re-reads
//! the configuration and refuses anything outside the allowlist before it
//! runs its one `systemctl`. Reads need no privilege. A unit that is not
//! allowlisted is refused by the portal before anything is run.

pub mod camilla;
pub mod capture_rate;
pub mod files;
pub mod host;
pub mod pages;
pub mod rate_follow;
pub mod services;
pub mod volume;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};

use crate::config::ApplianceConfig;
use crate::{AppState, Shared};

/// Appliance mode as configured, plus how host commands are run.
#[derive(Debug)]
pub struct Appliance {
    /// The `[appliance]` table (validated).
    pub config: ApplianceConfig,
    /// Runs `systemctl` and `bluetoothctl` (the real processes, or a
    /// scripted fake in tests).
    pub runner: Arc<dyn services::Runner>,
    /// One change to the EQ engine's files at a time: held across an
    /// upload, an activation (the check included), a deletion and a boost
    /// or capture-rate edit.
    pub eq_lock: Arc<tokio::sync::Mutex<()>>,
    /// The operator has posted a volume since start: the saved value that
    /// start sets again once the card is there must stand down
    /// ([`volume::restore`]).
    pub volume_touched: AtomicBool,
    /// What the rate follower last saw, for the Speaker page
    /// ([`rate_follow`]).
    pub rate_follow: Mutex<rate_follow::Report>,
}

impl Appliance {
    /// Appliance mode with `config`, running host commands through
    /// `runner`.
    #[must_use]
    pub fn new(config: ApplianceConfig, runner: Arc<dyn services::Runner>) -> Self {
        Self {
            config,
            runner,
            eq_lock: Arc::new(tokio::sync::Mutex::new(())),
            volume_touched: AtomicBool::new(false),
            rate_follow: Mutex::new(rate_follow::Report::default()),
        }
    }
}

/// The routes of appliance mode: `/` redirects to the Speaker page, the
/// overview is at `/device` (and `/overview`). Used only when it is on; off,
/// the router keeps `/` as the overview and none of these paths exist.
/// `cfg` sizes the upload route's body limit.
pub fn routes(cfg: &ApplianceConfig) -> Router<Shared> {
    Router::new()
        .route("/", get(pages::root))
        .route("/device", get(crate::pages::overview))
        .route("/overview", get(crate::pages::overview))
        .route("/speaker", get(pages::speaker))
        .route("/speaker/services", post(pages::services))
        // The EQ engine and the amplifier (answer 404 unless configured).
        .route("/speaker/volume", post(volume::post))
        .route("/speaker/volume/unmute", post(volume::post_unmute))
        .route("/speaker/boost", post(camilla::post_boost))
        .route("/speaker/capture-rate", post(camilla::post_capture_rate))
        // The upload route's body limit is `max_upload_bytes` plus the
        // multipart framing (the default 2 MiB would refuse coefficient
        // files); the handler counts the file bytes as well.
        .route(
            "/speaker/eq/upload",
            post(files::upload).layer(DefaultBodyLimit::max(files::body_limit(&cfg.camilladsp))),
        )
        .route("/speaker/eq/activate", post(files::activate))
        .route("/speaker/eq/delete", post(files::delete))
        .route("/speaker/eq/download/{kind}/{name}", get(files::download))
}

/// Where the overview page is: `/device` in appliance mode, else `/`.
#[must_use]
pub fn overview_path(st: &AppState) -> &'static str {
    if st.appliance.is_some() {
        "/device"
    } else {
        "/"
    }
}

/// The navigation entries `(href, label)`: the hook that puts "Speaker"
/// first and moves the overview to `/device` in appliance mode; otherwise
/// `base` unchanged.
#[must_use]
pub fn nav(
    st: &AppState,
    base: &[(&'static str, &'static str)],
) -> Vec<(&'static str, &'static str)> {
    let on = st.appliance.is_some();
    let mut v = Vec::with_capacity(base.len() + 1);
    if on {
        v.push(("/speaker", "Speaker"));
    }
    for (href, label) in base {
        let href = if on && *href == "/" {
            overview_path(st)
        } else {
            *href
        };
        v.push((href, *label));
    }
    v
}

/// What the handler tests share: a portal state with appliance mode on, a
/// daemon that is not there, a scripted runner and one session.
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use axum::http::{HeaderMap, HeaderValue, header};
    use tessera_control_types::{Reply, RequestBody};
    use tesseractl::client::ClientError;

    use super::{Appliance, services::Runner};
    use crate::auth::{COOKIE, PasswordFile, Session};
    use crate::backend::Backend;
    use crate::config::ApplianceConfig;
    use crate::{AppState, Shared};

    /// A daemon that never answers.
    #[derive(Debug)]
    struct NoDaemon;

    impl Backend for NoDaemon {
        fn request(&self, _: RequestBody) -> Result<Reply, ClientError> {
            Err(ClientError::Protocol("no daemon in tests".into()))
        }

        fn events(&self, _: &mut dyn FnMut(Reply) -> bool) -> Result<(), ClientError> {
            Err(ClientError::Protocol("no daemon in tests".into()))
        }
    }

    /// The state, a session's cookie token and the session (its CSRF token).
    pub(crate) fn state(
        cfg: ApplianceConfig,
        runner: Arc<dyn Runner>,
    ) -> (Shared, String, Session) {
        let st = AppState::new(
            Arc::new(NoDaemon),
            PasswordFile("/nonexistent/tessera-web-password".into()),
            Duration::from_secs(3600),
            false,
        )
        .with_appliance(Appliance::new(cfg, runner));
        let st = Arc::new(st);
        let (token, s) = st.sessions.create(Instant::now());
        (st, token, s)
    }

    /// Request headers: the session cookie, if any, and the live marker.
    pub(crate) fn form_headers(token: Option<&str>, live: bool) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(t) = token {
            h.insert(
                header::COOKIE,
                HeaderValue::from_str(&format!("{COOKIE}={t}")).unwrap(),
            );
        }
        if live {
            h.insert("x-tessera-live", HeaderValue::from_static("1"));
        }
        h
    }
}
