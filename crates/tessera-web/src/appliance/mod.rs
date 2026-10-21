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
pub mod files;
pub mod host;
pub mod pages;
pub mod services;
pub mod volume;

use std::sync::Arc;

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
    /// edit.
    pub eq_lock: Arc<tokio::sync::Mutex<()>>,
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
        .route("/speaker/boost", post(camilla::post_boost))
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
