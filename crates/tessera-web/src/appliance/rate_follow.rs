//! The rate follower (`[appliance.rate_follow]`, appliance mode, off by
//! default): on a speaker whose sources share one ALSA bus in front of the EQ
//! engine, the tessera device's sample rate is the only rate. It is changed
//! from a network controller, the portal or `tesseractl set-rate`; the bus,
//! its clients and the EQ engine follow it, and the engine never keeps a rate
//! of its own (an old configuration restored in its GUI included).
//!
//! The chain: `tesserad` bridge → ALSA `dmix` (the bus) → `snd-aloop` → EQ
//! engine capture → sound card. A `dmix` never converts rates and takes its
//! slave rate when its first client opens it; the ALSA snippet `rate_file`
//! (`tessera.rate <Hz>`) is where it reads that rate
//! (`rate { @func refer file "<rate_file>" name tessera.rate }`). A loopback
//! cable carries one rate, so the engine's capture must close before the bus
//! can reopen at another. The daemon's bridge restarts itself on a rate
//! change and retries its open while the bus runs at another rate; it needs
//! nothing from here.
//!
//! Every [`PERIOD`] (and once at start) a round reads the device's rate `R`
//! from the daemon's status, the rate file, the bus's live rate
//! (`bus_hw_params`), the active configuration file's rates and the running
//! engine's (`GetConfigJson`), and [`plan`] decides, purely:
//!
//! | Observed | Actions, in this order |
//! |----------|------------------------|
//! | the rate file does not hold `tessera.rate R` | write it (temporary file and rename, 0644) |
//! | the bus is open at a rate other than `R` | `Stop` the engine, restart every `restart` unit that is active, and reload the engine below even if nothing else asks for it |
//! | the active file's `samplerate`, or a set `capture_samplerate`, is not `R` | set them to `R` in the file (only those scalars change), then `Reload` |
//! | else, the bus was reopened | `Reload` (after `SetConfigFilePath` of the active file: the engine was stopped) |
//! | else, the running engine's rates are not `R` (a configuration applied in its GUI and not saved) | `SetConfigJson` with the running configuration, only its rates set to `R` |
//! | else, the follower acted for `R` and the engine stopped with an error since | `Reload` again |
//! | none of these | nothing: settled |
//!
//! Without `[appliance.camilladsp] enabled` only the first two rows apply,
//! without the engine. A file the follower cannot edit (outside
//! `configs_dir`, flow style, ...) is reported, not touched; the running
//! engine is still corrected.
//!
//! **Never hot.** [`Gate`] runs the actions at once for a new `R`; while the
//! state does not settle, the next attempt waits [`BACKOFF_MIN`], doubling
//! up to [`BACKOFF_MAX`] (a sound card that refuses the rate, a bus client
//! that is not in `restart`). A new `R` starts over. Actions are logged at
//! `info` when they first run for `R`, failures at `warn` when they change.
//!
//! The engine's files are changed with the appliance's file lock held, as
//! the signal boost and the capture-rate fix change them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;
use tessera_control_types::{Query, Reply, RequestBody, Status};

use crate::AppState;
use crate::appliance::camilla::{self, Active, Client, WsError};
use crate::appliance::{Appliance, capture_rate, files, services};
use crate::config::{ApplianceConfig, CamillaDspConfig};

/// A round runs this often.
pub const PERIOD: Duration = Duration::from_secs(2);
/// The first wait before acting again for the same rate.
pub const BACKOFF_MIN: Duration = Duration::from_secs(2);
/// The longest wait between attempts for the same rate.
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// Mode of the rate file: every ALSA client reads it.
pub const RATE_FILE_MODE: u32 = 0o644;
/// The timeout of the reads a round makes of the engine.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

// ---- the pure part ----------------------------------------------------------------------

/// The rate keys of an EQ configuration's `devices`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rates {
    /// `samplerate`: the processing rate (and the capture rate unless
    /// `capture_samplerate` is set).
    pub samplerate: u32,
    /// `capture_samplerate`; `None` when absent or null (it follows
    /// `samplerate` then).
    pub capture_samplerate: Option<u32>,
}

impl Rates {
    /// Both sides run at `rate`.
    #[must_use]
    pub fn at(self, rate: u32) -> bool {
        self.samplerate == rate && self.capture_samplerate.is_none_or(|c| c == rate)
    }
}

/// The active configuration file, as far as the follower may act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigFile {
    /// Nothing to compare: the engine has no active configuration (or it is
    /// unknown; the round reports why).
    Absent,
    /// Its rates; when they are not at the device's rate, the file can be
    /// edited.
    Rates(Rates),
    /// Not at the device's rate, or unreadable, and not editable: why.
    Unfixable(String),
}

/// One round's view of the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    /// The tessera device's sample rate.
    pub rate: u32,
    /// The rate file holds `tessera.rate <rate>`.
    pub rate_file: bool,
    /// The bus's live rate; `None` while it is closed (or not watched).
    pub bus: Option<u32>,
    /// `[appliance.camilladsp]` is enabled: the engine is part of the chain.
    pub eq: bool,
    /// The active configuration file.
    pub file: ConfigFile,
    /// The running engine's rates; `None` when it cannot be asked or runs
    /// no configuration.
    pub engine: Option<Rates>,
    /// The engine stopped with an error (asked only while retrying).
    pub engine_failed: bool,
}

/// What a round does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Write `tessera.rate <rate>` into the rate file.
    WriteRateFile,
    /// The engine's `Stop`: its capture closes, so the bus can reopen.
    StopEngine,
    /// Restart each `restart` unit that is active.
    RestartBusClients,
    /// Set the active file's rates to the device's.
    RewriteConfig,
    /// The engine's `Reload` (after `SetConfigFilePath` when it was stopped).
    Reload,
    /// `SetConfigJson`: the running configuration with its rates corrected.
    PatchEngine,
}

/// The decision of one round (the table in the module documentation).
/// `retrying`: the follower acted for this rate and the state has not
/// settled since.
#[must_use]
pub fn plan(o: &Observed, retrying: bool) -> Vec<Action> {
    let mut a = Vec::new();
    if !o.rate_file {
        a.push(Action::WriteRateFile);
    }
    let reopen = o.bus.is_some_and(|b| b != o.rate);
    if reopen {
        if o.eq {
            a.push(Action::StopEngine);
        }
        a.push(Action::RestartBusClients);
    }
    if !o.eq {
        return a;
    }
    match &o.file {
        ConfigFile::Rates(r) if !r.at(o.rate) => {
            a.push(Action::RewriteConfig);
            a.push(Action::Reload);
        }
        _ if reopen => a.push(Action::Reload),
        _ if o.engine.is_some_and(|e| !e.at(o.rate)) => a.push(Action::PatchEngine),
        _ if retrying && o.engine_failed => a.push(Action::Reload),
        _ => {}
    }
    a
}

/// The wait after the `attempt`-th attempt for one rate: [`BACKOFF_MIN`]
/// doubling up to [`BACKOFF_MAX`].
#[must_use]
pub fn backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(16);
    BACKOFF_MIN.saturating_mul(1 << shift).min(BACKOFF_MAX)
}

/// What [`Gate::turn`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Turn {
    /// Nothing to do: the chain is at the device's rate.
    Settled,
    /// Something to do, but the last attempt for this rate was too recent.
    Wait,
    /// Do these now.
    Act(Vec<Action>),
}

/// The back-off around [`plan`], per device rate.
#[derive(Debug, Default, Clone)]
pub struct Gate {
    rate: Option<u32>,
    attempts: u32,
    not_before: Option<Instant>,
}

impl Gate {
    /// The follower acted for `rate` and the state has not settled since.
    #[must_use]
    pub fn retrying(&self, rate: u32) -> bool {
        self.rate == Some(rate) && self.attempts > 0
    }

    /// Decide at `now`: a new rate starts over (acts at once); a settled
    /// state resets the count; otherwise the actions run only once the wait
    /// after the previous attempt has passed.
    pub fn turn(&mut self, now: Instant, o: &Observed) -> Turn {
        if self.rate != Some(o.rate) {
            *self = Self {
                rate: Some(o.rate),
                ..Self::default()
            };
        }
        let actions = plan(o, self.attempts > 0);
        if actions.is_empty() {
            self.attempts = 0;
            self.not_before = None;
            return Turn::Settled;
        }
        if self.not_before.is_some_and(|t| now < t) {
            return Turn::Wait;
        }
        self.attempts = self.attempts.saturating_add(1);
        self.not_before = Some(now + backoff(self.attempts));
        Turn::Act(actions)
    }
}

/// The rate file's content for `rate`.
#[must_use]
pub fn rate_text(rate: u32) -> String {
    format!("tessera.rate {rate}\n")
}

/// Does the rate file's `text` hold exactly `tessera.rate <rate>` (the final
/// newline may be missing: a tmpfiles seed has none)?
#[must_use]
pub fn holds(text: &str, rate: u32) -> bool {
    let want = rate_text(rate);
    text == want || text == want.trim_end()
}

/// The rate of a `/proc/asound/.../hw_params` text: the `rate:` line
/// (`rate: 48000 (48000/1)`); `None` while the PCM is `closed` or has no
/// parameters yet.
#[must_use]
pub fn bus_rate(hw_params: &str) -> Option<u32> {
    hw_params
        .lines()
        .find_map(|l| l.trim().strip_prefix("rate:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .filter(|r| *r > 0)
}

/// The sample rate of the followed device: `key`, or the daemon's only
/// device.
///
/// # Errors
/// No such device, no device, several devices without `key`, or no rate.
pub fn device_rate(status: &Status, key: Option<&str>) -> Result<u32, String> {
    let d = match key {
        Some(k) => status
            .devices
            .iter()
            .find(|d| d.key.0 == k)
            .ok_or_else(|| {
                format!("the daemon has no device `{k}` ([appliance.rate_follow] device)")
            })?,
        None => match status.devices.as_slice() {
            [d] => d,
            [] => return Err("the daemon reports no device".into()),
            many => {
                return Err(format!(
                    "the daemon has {} devices; name the one to follow with [appliance.rate_follow] device",
                    many.len()
                ));
            }
        },
    };
    match d.sample_rate.0 {
        0 => Err(format!(
            "the daemon reports no sample rate for `{}`",
            d.key.0
        )),
        r => Ok(r),
    }
}

/// The rates of the engine's running configuration (`GetConfigJson`).
#[must_use]
pub fn engine_rates(config: &Value) -> Option<Rates> {
    let d = config.get("devices")?;
    let rate = |v: &Value| v.as_u64().and_then(|n| u32::try_from(n).ok());
    let samplerate = rate(d.get("samplerate")?)?;
    let capture_samplerate = match d.get("capture_samplerate") {
        None | Some(Value::Null) => None,
        Some(v) => Some(rate(v)?),
    };
    Some(Rates {
        samplerate,
        capture_samplerate,
    })
}

/// The running configuration with `devices.samplerate`, and a set
/// `devices.capture_samplerate`, at `rate`; everything else as it is.
#[must_use]
pub fn with_rate(config: &Value, rate: u32) -> Option<Value> {
    engine_rates(config)?;
    let mut out = config.clone();
    let d = out.get_mut("devices")?.as_object_mut()?;
    d.insert("samplerate".into(), Value::from(rate));
    if d.get("capture_samplerate").is_some_and(|v| !v.is_null()) {
        d.insert("capture_samplerate".into(), Value::from(rate));
    }
    Some(out)
}

/// The error of a `GetStopReason` answer: `None` for `"None"` and `"Done"`.
#[must_use]
pub fn stop_error(reason: &Value) -> Option<String> {
    match reason {
        Value::String(s) if s == "None" || s == "Done" => None,
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o.iter().next().map(|(k, v)| match v {
            Value::String(m) => format!("{k}: {m}"),
            Value::Null => k.clone(),
            other => format!("{k}: {other}"),
        }),
        _ => None,
    }
}

// ---- what the page shows -----------------------------------------------------------------

/// The follower's state for the Speaker page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The rate followed; `None` until a round has read it.
    pub rate: Option<u32>,
    /// The last problem or failure, until the chain settles without one.
    pub error: Option<String>,
}

// ---- the rounds --------------------------------------------------------------------------

/// One round's observations and what the actions need of them.
#[derive(Debug, Clone)]
pub struct Observation {
    /// For [`plan`].
    pub observed: Observed,
    /// The active configuration's path (the engine's, or its state file's).
    pub active: Option<PathBuf>,
    /// The engine's stop error, when asked and failed.
    pub engine_stop: Option<String>,
    /// Something the follower cannot act on (an unknown active
    /// configuration, a file it may not edit).
    pub problem: Option<String>,
}

/// Read the chain for the device rate `rate`. `health`: also ask whether
/// the engine stopped with an error.
pub async fn observe(cfg: &ApplianceConfig, rate: u32, health: bool) -> Observation {
    let rf = &cfg.rate_follow;
    let rate_file = tokio::fs::read_to_string(&rf.rate_file)
        .await
        .is_ok_and(|t| holds(&t, rate));
    let bus = match &rf.bus_hw_params {
        Some(p) => tokio::fs::read_to_string(p)
            .await
            .ok()
            .and_then(|t| bus_rate(&t)),
        None => None,
    };
    let mut o = Observation {
        observed: Observed {
            rate,
            rate_file,
            bus,
            eq: cfg.camilladsp.enabled,
            file: ConfigFile::Absent,
            engine: None,
            engine_failed: false,
        },
        active: None,
        engine_stop: None,
        problem: None,
    };
    if !cfg.camilladsp.enabled {
        return o;
    }
    let c = &cfg.camilladsp;
    let client = Client::new(&c.websocket).with_timeout(READ_TIMEOUT);
    match Active::read(c, &client).await {
        Ok(Active { path: Some(p), .. }) => {
            o.observed.file = read_file(c, &p, rate).await;
            if let ConfigFile::Unfixable(why) = &o.observed.file {
                o.problem = Some(why.clone());
            }
            o.active = Some(p);
        }
        Ok(_) => {}
        Err(e) => o.problem = Some(e),
    }
    o.observed.engine = match client.get_config_json().await {
        Ok(Some(v)) => engine_rates(&v),
        Ok(None) => None,
        Err(e) => {
            tracing::debug!(%e, "rate follower: the running EQ configuration cannot be read");
            None
        }
    };
    if health && client.get_state().await.is_ok_and(|s| s == "Inactive") {
        o.engine_stop = match client.get_stop_reason().await {
            Ok(v) => stop_error(&v),
            Err(_) => None,
        };
        o.observed.engine_failed = o.engine_stop.is_some();
    }
    o
}

async fn read_file(c: &CamillaDspConfig, path: &Path, rate: u32) -> ConfigFile {
    let shown = camilla::display_name(c, path);
    let yaml = match tokio::fs::read_to_string(path).await {
        Ok(y) => y,
        Err(e) => return ConfigFile::Unfixable(format!("cannot read {shown}: {e}")),
    };
    let rates = match capture_rate::rates(&yaml) {
        Ok((samplerate, capture_samplerate)) => Rates {
            samplerate,
            capture_samplerate,
        },
        Err(e) => {
            return ConfigFile::Unfixable(format!(
                "the rates of {shown} cannot be kept at the device's: {e}"
            ));
        }
    };
    if !rates.at(rate) {
        if let Err(e) = camilla::editable(c, path) {
            return ConfigFile::Unfixable(format!("{e}: set its samplerate to {rate} by hand"));
        }
    }
    ConfigFile::Rates(rates)
}

/// The follower: its back-off and what it last logged.
#[derive(Debug, Default)]
pub struct Follower {
    gate: Gate,
    logged_problem: Option<String>,
    logged_actions: Option<(u32, Vec<Action>)>,
    logged_error: Option<String>,
}

impl Follower {
    /// One round at `now` with the daemon's `status`: observe, decide, act,
    /// and update the page's [`Report`]. Returns the decision.
    pub async fn round(
        &mut self,
        a: &Appliance,
        status: Result<Status, String>,
        now: Instant,
    ) -> Turn {
        let rf = &a.config.rate_follow;
        let rate = match status.and_then(|s| device_rate(&s, rf.device.as_deref())) {
            Ok(r) => r,
            Err(e) => {
                self.log_problem(Some(e.as_str()));
                set_report(a, None, Some(e));
                return Turn::Wait;
            }
        };
        let retrying = self.gate.retrying(rate);
        let obs = observe(&a.config, rate, retrying).await;
        let o = &obs.observed;
        // Still at another rate after the follower reopened it: a client
        // that is not restarted holds the bus.
        let stuck = o.bus.filter(|b| retrying && *b != rate).map(|b| {
            format!(
                "the bus still runs at {b} Hz: a client that is not in [appliance.rate_follow] restart may hold it open"
            )
        });
        let failed = obs
            .engine_stop
            .as_ref()
            .map(|e| format!("the EQ engine stopped: {e}"));
        self.log_problem(obs.problem.as_deref());
        let turn = self.gate.turn(now, o);
        let error = match &turn {
            Turn::Settled => obs.problem.clone(),
            Turn::Wait => failed.or(stuck).or(obs.problem.clone()).or_else(|| {
                a.rate_follow
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .error
                    .clone()
            }),
            Turn::Act(actions) => {
                self.log_actions(rate, actions);
                let r = execute(a, rate, actions, &obs).await;
                self.log_error(r.as_ref().err());
                r.err().or(failed).or(stuck).or(obs.problem.clone())
            }
        };
        if matches!(turn, Turn::Settled) {
            self.log_error(None);
        }
        set_report(a, Some(rate), error);
        turn
    }

    fn log_problem(&mut self, problem: Option<&str>) {
        if problem != self.logged_problem.as_deref() {
            if let Some(p) = problem {
                tracing::warn!(problem = p, "rate follower");
            }
            self.logged_problem = problem.map(str::to_owned);
        }
    }

    fn log_actions(&mut self, rate: u32, actions: &[Action]) {
        let key = (rate, actions.to_vec());
        if self.logged_actions.as_ref() == Some(&key) {
            tracing::debug!(rate, ?actions, "rate follower: trying again");
        } else {
            tracing::info!(
                rate,
                ?actions,
                "rate follower: following the device's sample rate"
            );
            self.logged_actions = Some(key);
        }
    }

    fn log_error(&mut self, error: Option<&String>) {
        match error {
            Some(e) if self.logged_error.as_ref() != Some(e) => {
                tracing::warn!(error = %e, "rate follower");
                self.logged_error = Some(e.clone());
            }
            Some(e) => tracing::debug!(error = %e, "rate follower"),
            None => self.logged_error = None,
        }
    }
}

/// The page's report: the rate when known (the last one otherwise), and the
/// error.
fn set_report(a: &Appliance, rate: Option<u32>, error: Option<String>) {
    let mut r = a.rate_follow.lock().unwrap_or_else(PoisonError::into_inner);
    if rate.is_some() {
        r.rate = rate;
    }
    r.error = error;
}

/// Run `actions` in order. Every action is tried; the first failure is
/// returned. The engine's part runs with the file lock held.
async fn execute(
    a: &Appliance,
    rate: u32,
    actions: &[Action],
    obs: &Observation,
) -> Result<(), String> {
    let cfg = &a.config;
    let runner = &*a.runner;
    let mut errors: Vec<String> = Vec::new();
    let client = Client::new(&cfg.camilladsp.websocket);
    for act in actions {
        match act {
            Action::WriteRateFile => {
                let path = cfg.rate_follow.rate_file.clone();
                let r = tokio::task::spawn_blocking(move || {
                    files::write_atomic_mode(&path, rate_text(rate).as_bytes(), RATE_FILE_MODE)
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
                if let Err(e) = r {
                    errors.push(format!("cannot write the rate file: {e}"));
                }
            }
            Action::StopEngine => {
                if let Err(e) = client.stop().await {
                    tracing::debug!(%e, "rate follower: the EQ engine did not stop");
                }
            }
            Action::RestartBusClients => {
                for unit in &cfg.rate_follow.restart {
                    let r = match services::is_active(runner, unit).await {
                        Ok(true) => services::restart_unit(runner, cfg, unit).await,
                        Ok(false) => Ok(()),
                        Err(e) => Err(e),
                    };
                    if let Err(e) = r {
                        errors.push(e);
                    }
                }
            }
            Action::RewriteConfig | Action::Reload | Action::PatchEngine => {}
        }
    }
    let engine = actions.iter().any(|x| {
        matches!(
            x,
            Action::RewriteConfig | Action::Reload | Action::PatchEngine
        )
    });
    if engine {
        let _g = a.eq_lock.lock().await;
        let mut rewritten = true;
        if actions.contains(&Action::RewriteConfig) {
            if let Err(e) = rewrite(&cfg.camilladsp, rate).await {
                errors.push(e);
                rewritten = false;
            }
        }
        let stopped = actions.contains(&Action::StopEngine);
        // A file that could not be set is not reloaded into a running
        // engine (it would restart at the old rate); a stopped one must
        // come back all the same.
        if actions.contains(&Action::Reload) && (rewritten || stopped) {
            if let Err(e) = reload(
                cfg,
                runner,
                stopped.then_some(obs.active.as_deref()).flatten(),
            )
            .await
            {
                errors.push(e);
            }
        }
        if actions.contains(&Action::PatchEngine) {
            if let Err(e) = patch(&client, rate).await {
                errors.push(e);
            }
        }
    }
    match errors.into_iter().next() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Set the active file's rates to `rate`, if it needs it. The active file
/// is resolved again under the lock.
async fn rewrite(c: &CamillaDspConfig, rate: u32) -> Result<(), String> {
    let client = Client::new(&c.websocket);
    let active = Active::read(c, &client).await?;
    let path = active
        .path
        .ok_or_else(|| "the EQ engine has no active configuration".to_owned())?;
    let (path, name) = camilla::editable(c, &path)?;
    tokio::task::spawn_blocking(move || {
        let yaml = files::read_regular(&path)
            .map_err(|e| format!("cannot read {name}: {e}"))
            .and_then(|b| String::from_utf8(b).map_err(|_| format!("{name} is not UTF-8")))?;
        let new = capture_rate::set_rates(&yaml, rate).map_err(|e| format!("{name}: {e}"))?;
        if new != yaml {
            files::write_atomic(&path, new.as_bytes())
                .map_err(|e| format!("cannot write {name}: {e}"))?;
            tracing::info!(config = %name, rate, "rate follower: EQ configuration set to the device's rate");
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// `Reload`; after a `Stop`, `SetConfigFilePath` of `path` first. Without
/// the websocket, an engine whose unit runs (and is allowlisted) is
/// restarted; a stopped one reads the file when it starts.
async fn reload(
    cfg: &ApplianceConfig,
    runner: &dyn services::Runner,
    path: Option<&Path>,
) -> Result<(), String> {
    let client = Client::new(&cfg.camilladsp.websocket);
    if let Some(p) = path {
        if let Err(e) = client.set_config_file_path(&p.to_string_lossy()).await {
            tracing::debug!(%e, "rate follower: the EQ engine did not take its configuration path");
        }
    }
    match client.reload().await {
        Ok(()) => Ok(()),
        Err(WsError::Unreachable(why)) => {
            let listed = cfg.services.iter().any(|e| e.has_unit(camilla::UNIT));
            if listed && camilla::unit_active(runner).await? {
                tracing::info!(%why, "rate follower: restarting the EQ engine, its websocket does not answer");
                services::restart_unit(runner, cfg, camilla::UNIT).await
            } else {
                Ok(())
            }
        }
        Err(e) => Err(format!("the EQ engine did not reload: {e}")),
    }
}

/// `SetConfigJson` with the running configuration at `rate`.
async fn patch(client: &Client, rate: u32) -> Result<(), String> {
    let running = client
        .get_config_json()
        .await
        .map_err(|e| format!("cannot read the running EQ configuration: {e}"))?;
    let Some(running) = running else {
        return Ok(());
    };
    let Some(new) = with_rate(&running, rate) else {
        return Err("the running EQ configuration has no readable devices.samplerate".into());
    };
    if new == running {
        return Ok(());
    }
    client
        .set_config_json(&new)
        .await
        .map_err(|e| format!("the EQ engine refused the device's rate: {e}"))
}

/// The daemon's status, from the local daemon only.
async fn daemon_status(st: &AppState) -> Result<Status, String> {
    match crate::local_call(st, RequestBody::Query(Query::Status)).await {
        Ok(Reply::Status(s)) => Ok(s),
        Ok(other) => Err(format!("unexpected reply: {other:?}")),
        Err(e) => Err(crate::explain(&e)),
    }
}

/// Start the follower on a task of its own, when appliance mode is on and
/// `[appliance.rate_follow] enabled = true`: a round at once, then every
/// [`PERIOD`].
pub fn spawn(st: Arc<AppState>) {
    let Some(a) = st.appliance.clone() else {
        return;
    };
    let rf = &a.config.rate_follow;
    if !rf.enabled {
        return;
    }
    tracing::info!(
        rate_file = %rf.rate_file.display(),
        bus = ?rf.bus_hw_params,
        restart = ?rf.restart,
        eq = a.config.camilladsp.enabled,
        "rate follower: the speaker follows the tessera device's sample rate"
    );
    tokio::spawn(async move {
        let mut f = Follower::default();
        let mut tick = tokio::time::interval(PERIOD);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let status = daemon_status(&st).await;
            f.round(&a, status, Instant::now()).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: u32 = 96_000;

    fn rates(samplerate: u32, capture: Option<u32>) -> Rates {
        Rates {
            samplerate,
            capture_samplerate: capture,
        }
    }

    /// Everything at `R`, the engine part on.
    fn settled() -> Observed {
        Observed {
            rate: R,
            rate_file: true,
            bus: Some(R),
            eq: true,
            file: ConfigFile::Rates(rates(R, None)),
            engine: Some(rates(R, Some(R))),
            engine_failed: false,
        }
    }

    use super::Action::{
        PatchEngine, Reload, RestartBusClients, RewriteConfig, StopEngine, WriteRateFile,
    };

    #[test]
    fn the_decision_table() {
        let cases: Vec<(&str, Observed, bool, Vec<Action>)> = vec![
            ("settled", settled(), false, vec![]),
            (
                "settled, bus closed",
                Observed {
                    bus: None,
                    ..settled()
                },
                false,
                vec![],
            ),
            (
                "settled, engine unknown",
                Observed {
                    engine: None,
                    file: ConfigFile::Absent,
                    ..settled()
                },
                false,
                vec![],
            ),
            (
                "rate file stale",
                Observed {
                    rate_file: false,
                    ..settled()
                },
                false,
                vec![WriteRateFile],
            ),
            (
                "bus at the old rate",
                Observed {
                    bus: Some(48_000),
                    ..settled()
                },
                false,
                vec![StopEngine, RestartBusClients, Reload],
            ),
            (
                "a device-rate change: everything old",
                Observed {
                    rate_file: false,
                    bus: Some(48_000),
                    file: ConfigFile::Rates(rates(48_000, None)),
                    engine: Some(rates(48_000, None)),
                    ..settled()
                },
                false,
                vec![
                    WriteRateFile,
                    StopEngine,
                    RestartBusClients,
                    RewriteConfig,
                    Reload,
                ],
            ),
            (
                "an old configuration restored",
                Observed {
                    file: ConfigFile::Rates(rates(48_000, None)),
                    engine: Some(rates(48_000, None)),
                    ..settled()
                },
                false,
                vec![RewriteConfig, Reload],
            ),
            (
                "only capture_samplerate differs",
                Observed {
                    file: ConfigFile::Rates(rates(R, Some(48_000))),
                    ..settled()
                },
                false,
                vec![RewriteConfig, Reload],
            ),
            (
                "a GUI applied an unsaved configuration",
                Observed {
                    engine: Some(rates(R, Some(44_100))),
                    ..settled()
                },
                false,
                vec![PatchEngine],
            ),
            (
                "the file wins over the engine",
                Observed {
                    file: ConfigFile::Rates(rates(44_100, None)),
                    engine: Some(rates(48_000, None)),
                    ..settled()
                },
                false,
                vec![RewriteConfig, Reload],
            ),
            (
                "an unfixable file: the engine is still corrected",
                Observed {
                    file: ConfigFile::Unfixable("flow style".into()),
                    engine: Some(rates(48_000, None)),
                    ..settled()
                },
                false,
                vec![PatchEngine],
            ),
            (
                "an unfixable file, engine at the rate",
                Observed {
                    file: ConfigFile::Unfixable("flow style".into()),
                    ..settled()
                },
                false,
                vec![],
            ),
            (
                "the engine failed after the follower acted",
                Observed {
                    engine_failed: true,
                    ..settled()
                },
                true,
                vec![Reload],
            ),
            (
                "the engine failed, the follower did nothing",
                Observed {
                    engine_failed: true,
                    ..settled()
                },
                false,
                vec![],
            ),
            (
                "bus reopen with an unfixable file still restarts the engine",
                Observed {
                    bus: Some(44_100),
                    file: ConfigFile::Unfixable("outside".into()),
                    ..settled()
                },
                false,
                vec![StopEngine, RestartBusClients, Reload],
            ),
            (
                "no engine configured: the rate file and the bus only",
                Observed {
                    rate_file: false,
                    bus: Some(48_000),
                    eq: false,
                    file: ConfigFile::Absent,
                    engine: None,
                    ..settled()
                },
                false,
                vec![WriteRateFile, RestartBusClients],
            ),
            (
                "no engine configured, settled",
                Observed {
                    eq: false,
                    file: ConfigFile::Absent,
                    engine: None,
                    ..settled()
                },
                true,
                vec![],
            ),
        ];
        for (name, o, retrying, want) in cases {
            assert_eq!(plan(&o, retrying), want, "{name}");
        }
    }

    #[test]
    fn the_gate_acts_at_once_then_backs_off_and_starts_over_for_a_new_rate() {
        let t0 = Instant::now();
        let s = |secs: u64| t0 + Duration::from_secs(secs);
        let mut g = Gate::default();
        let wrong = Observed {
            rate_file: false,
            ..settled()
        };
        assert_eq!(g.turn(t0, &wrong), Turn::Act(vec![WriteRateFile]));
        assert!(g.retrying(R));
        // The next round, 2 s later, may try again; then 4, 8, ... 60 s apart.
        assert_eq!(g.turn(s(1), &wrong), Turn::Wait);
        assert_eq!(g.turn(s(2), &wrong), Turn::Act(vec![WriteRateFile]));
        assert_eq!(g.turn(s(5), &wrong), Turn::Wait);
        assert_eq!(g.turn(s(6), &wrong), Turn::Act(vec![WriteRateFile]));
        assert_eq!(g.turn(s(13), &wrong), Turn::Wait);
        assert_eq!(g.turn(s(14), &wrong), Turn::Act(vec![WriteRateFile]));
        // A new rate acts at once.
        let fine = Observed {
            rate: 48_000,
            file: ConfigFile::Rates(rates(48_000, None)),
            engine: None,
            bus: None,
            ..settled()
        };
        let stale = Observed {
            rate_file: false,
            ..fine.clone()
        };
        assert_eq!(g.turn(s(15), &stale), Turn::Act(vec![WriteRateFile]));
        assert!(!g.retrying(R) && g.retrying(48_000));
        // Settled resets the count: the next problem is acted on at once.
        assert_eq!(g.turn(s(16), &fine), Turn::Settled);
        assert!(!g.retrying(48_000));
        assert_eq!(g.turn(s(16), &stale), Turn::Act(vec![WriteRateFile]));
        // An engine that fails after the follower acted is reloaded again,
        // with the back-off; one that fails on its own is left alone.
        let mut g = Gate::default();
        let failed = Observed {
            engine_failed: true,
            ..settled()
        };
        assert_eq!(g.turn(t0, &failed), Turn::Settled);
        let restored = Observed {
            file: ConfigFile::Rates(rates(48_000, None)),
            ..settled()
        };
        assert_eq!(
            g.turn(t0, &restored),
            Turn::Act(vec![RewriteConfig, Reload])
        );
        assert_eq!(g.turn(s(1), &failed), Turn::Wait);
        assert_eq!(g.turn(s(2), &failed), Turn::Act(vec![Reload]));
        assert_eq!(g.turn(s(6), &failed), Turn::Act(vec![Reload]));
        assert_eq!(g.turn(s(7), &settled()), Turn::Settled);
    }

    #[test]
    fn the_package_creates_and_opens_the_default_rate_file() {
        let rate_file = crate::config::RateFollowConfig::DEFAULT_RATE_FILE;
        let dir = Path::new(rate_file).parent().unwrap().to_str().unwrap();
        let unit = include_str!("../../dist/tessera-web.service");
        assert!(
            unit.lines().any(|l| l == format!("ReadWritePaths=-{dir}")),
            "the unit opens {dir}"
        );
        let tmpfiles = include_str!("../../../../dist/tmpfiles.d/tessera-web.conf");
        let lines: Vec<Vec<&str>> = tmpfiles
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| l.split_whitespace().collect())
            .collect();
        assert!(lines.contains(&vec!["d", dir, "0755", "tessera-web", "tessera-web", "-"]));
        let seed = lines
            .iter()
            .find(|f| f.first() == Some(&"f") && f.get(1) == Some(&rate_file))
            .expect("the rate file is seeded");
        assert_eq!(seed[2..6], ["0644", "tessera-web", "tessera-web", "-"]);
        assert!(holds(&seed[6..].join(" "), 48_000), "{seed:?}");
    }

    #[test]
    fn the_backoff_doubles_from_2_s_to_60_s() {
        let secs: Vec<u64> = (1..=9).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 16, 32, 60, 60, 60, 60]);
        assert_eq!(backoff(0), BACKOFF_MIN);
        assert_eq!(backoff(u32::MAX), BACKOFF_MAX);
    }

    #[test]
    fn rate_files_hw_params_and_devices_are_read_strictly() {
        assert_eq!(rate_text(96_000), "tessera.rate 96000\n");
        assert!(holds("tessera.rate 96000\n", 96_000));
        assert!(
            holds("tessera.rate 96000", 96_000),
            "a tmpfiles seed has no newline"
        );
        for other in [
            "tessera.rate 48000\n",
            "tessera.rate 960000\n",
            "tessera.rate  96000\n",
            "# x\ntessera.rate 96000\n",
            "tessera.rate 96000\n\n",
            "",
        ] {
            assert!(!holds(other, 96_000), "{other:?}");
        }
        let open = "access: MMAP_INTERLEAVED\nformat: S32_LE\nsubformat: STD\nchannels: 2\nrate: 48000 (48000/1)\nperiod_size: 1024\nbuffer_size: 8192\n";
        assert_eq!(bus_rate(open), Some(48_000));
        assert_eq!(bus_rate("rate: 96000 (96000/1)\n"), Some(96_000));
        for none in [
            "closed\n",
            "no setup\n",
            "",
            "rate: fast\n",
            "rate: 0 (0/1)\n",
        ] {
            assert_eq!(bus_rate(none), None, "{none:?}");
        }
        use tessera_control_types::{DeviceKey, DeviceStatus, Encoding, LatencyUs, SampleRate};
        let dev = |k: &str, r: u32| {
            DeviceStatus::new(
                DeviceKey(k.into()),
                k,
                SampleRate(r),
                Encoding(24),
                LatencyUs(10_000),
            )
        };
        let mut st = Status::new("test");
        assert!(device_rate(&st, None).unwrap_err().contains("no device"));
        st.devices.push(dev("main", 96_000));
        assert_eq!(device_rate(&st, None), Ok(96_000));
        assert_eq!(device_rate(&st, Some("main")), Ok(96_000));
        assert!(
            device_rate(&st, Some("other"))
                .unwrap_err()
                .contains("no device `other`")
        );
        st.devices.push(dev("second", 48_000));
        let e = device_rate(&st, None).unwrap_err();
        assert!(
            e.contains("2 devices") && e.contains("[appliance.rate_follow] device"),
            "{e}"
        );
        assert_eq!(device_rate(&st, Some("second")), Ok(48_000));
        st.devices[1] = dev("second", 0);
        assert!(device_rate(&st, Some("second")).is_err());
    }

    #[test]
    fn the_running_configuration_is_corrected_in_its_rates_only() {
        let running = serde_json::json!({
            "devices": {
                "samplerate": 48000,
                "capture_samplerate": null,
                "chunksize": 1024,
                "capture": {"type": "Alsa", "device": "hw:Loopback,1,0"},
            },
            "filters": {"g": {"type": "Gain", "parameters": {"gain": 3.0}}},
        });
        assert_eq!(engine_rates(&running), Some(rates(48_000, None)));
        let new = with_rate(&running, R).unwrap();
        let mut want = running.clone();
        want["devices"]["samplerate"] = Value::from(R);
        assert_eq!(new, want, "a null capture rate stays null");
        let set =
            serde_json::json!({"devices": {"samplerate": 48000, "capture_samplerate": 44100}});
        assert_eq!(engine_rates(&set), Some(rates(48_000, Some(44_100))));
        assert_eq!(
            with_rate(&set, R).unwrap(),
            serde_json::json!({"devices": {"samplerate": R, "capture_samplerate": R}})
        );
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"devices": {"samplerate": "fast"}}),
            serde_json::json!({"devices": {"samplerate": 48000, "capture_samplerate": -1}}),
            serde_json::json!({"devices": []}),
        ] {
            assert_eq!(engine_rates(&bad), None, "{bad}");
            assert_eq!(with_rate(&bad, R), None, "{bad}");
        }
        assert_eq!(stop_error(&serde_json::json!("None")), None);
        assert_eq!(stop_error(&serde_json::json!("Done")), None);
        assert_eq!(
            stop_error(&serde_json::json!({"PlaybackError": "snd_pcm_hw_params_set_rate"}))
                .as_deref(),
            Some("PlaybackError: snd_pcm_hw_params_set_rate")
        );
        assert_eq!(
            stop_error(&serde_json::json!({"CaptureFormatChange": 44100})).as_deref(),
            Some("CaptureFormatChange: 44100")
        );
        assert_eq!(
            stop_error(&serde_json::json!("CaptureError")).as_deref(),
            Some("CaptureError")
        );
        assert!(rates(R, None).at(R) && rates(R, Some(R)).at(R));
        assert!(!rates(R, Some(48_000)).at(R) && !rates(48_000, None).at(R));
    }
}
