//! The state engine on its own thread (ADR 0004, ADR 0008 amendment).
//!
//! `tessera_state::StateEngine` is synchronous and does blocking file I/O
//! (`start`, `poll_persist`, `flush`, reload). It therefore never runs on the
//! async executor: one plain `std::thread` owns it and serves a bounded
//! command channel. The async side uses [`EngineHandle`], which never blocks:
//! a full queue is reported to the caller as a busy error, and each request
//! carries a `oneshot` for the answer.
//!
//! The thread also drives the debounced `state.toml` write: it sleeps until
//! the engine's persist deadline (or the next message), so it never spins.
//! Events are *not* routed through this thread: the [`EventBus`] handle is
//! taken before the engine moves here, and subscribers read it directly.
//!
//! If the thread ever ends without a shutdown request (a panic is a bug), the
//! `fatal` token is cancelled so the daemon exits and the service manager
//! restarts it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tessera_control_types::{
    BridgeStatus, Channel, ClockStatus, Command, CommandError, DeviceKey, ImpactToken,
    NetworkStatus, Origin, Query, Reply, SubscriptionState,
};
use tessera_state::{Applied, Config, MonotonicClock, StateEngine};

use crate::tunables::Reloadable;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

/// Values the control plane needs that can change on reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// `[daemon] admin_group`.
    pub admin_group: String,
    /// The gid of `admin_group` (`None` if the group does not exist),
    /// resolved here on the state thread so that connection tasks never
    /// touch the user database.
    pub admin_gid: Option<u32>,
    /// `[daemon] log_level`.
    pub log_level: String,
    /// The tunables of apply class `reload` the daemon applies itself.
    pub tunables: Reloadable,
    /// Keys of apply class `restart` whose value in the files differs from
    /// the one this process started with: they take effect at the next
    /// service restart (shown in the status warnings).
    pub restart_pending: Vec<String>,
}

impl Settings {
    /// Read the settings from an engine. Blocking (resolves the admin group).
    /// `started` is the configuration the process started with.
    #[must_use]
    pub fn of(engine: &StateEngine, started: &Config) -> Self {
        let cfg = &engine.config().config;
        let d = &cfg.daemon;
        Self {
            admin_group: d.admin_group.clone(),
            admin_gid: crate::peer::resolve_admin_group(&d.admin_group),
            log_level: d.log_level.clone(),
            tunables: Reloadable::of(cfg),
            restart_pending: tessera_state::impact::changed_restart_keys(started, cfg)
                .unwrap_or_else(|e| {
                    tracing::warn!(
                        "cannot compare the configuration with the one at start: {}",
                        crate::sanitize::log_safe(&e, 200)
                    );
                    Vec::new()
                }),
        }
    }

    /// The status warning for [`restart_pending`](Self::restart_pending).
    #[must_use]
    pub fn restart_warning(&self) -> Option<String> {
        (!self.restart_pending.is_empty()).then(|| {
            format!(
                "changed in the configuration, takes effect at the next service restart: {}",
                self.restart_pending.join(", ")
            )
        })
    }
}

/// How a command was confirmed (the control socket's request form).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// `RequestBody::Command`: a change that restarts something is refused
    /// with `ConfirmationRequired` (`StateEngine::apply`).
    No,
    /// `RequestBody::ConfirmedCommand { expect: None }`
    /// (`StateEngine::apply_confirmed`).
    Blind,
    /// `RequestBody::ConfirmedCommand { expect: Some(token) }`
    /// (`StateEngine::apply_expecting`).
    Expecting(ImpactToken),
}

/// What an audio client needs to know about a logical device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Config key.
    pub key: String,
    /// RX channel count.
    pub rx_channels: u16,
    /// TX channel count.
    pub tx_channels: u16,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Current latency in microseconds.
    pub latency_us: u32,
    /// Upper latency bound in microseconds (sizes the ring).
    pub latency_max_us: u32,
}

/// A copy of everything the runtime reconciler derives its work from: the
/// effective values (configuration combined with runtime changes) and the
/// loaded configuration.
#[derive(Debug, Clone)]
pub struct RuntimePlan {
    /// Effective devices and clock settings.
    pub model: tessera_state::Model,
    /// The merged configuration.
    pub config: tessera_state::Config,
}

/// Requests served by the engine thread.
#[derive(Debug)]
pub enum Msg {
    /// Apply a command.
    Apply {
        /// Who asked.
        origin: Origin,
        /// What to do.
        cmd: Command,
        /// How it was confirmed.
        confirm: Confirm,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<Applied, CommandError>>,
    },
    /// Answer a read-only query.
    Query {
        /// The query.
        query: Query,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<Reply, CommandError>>,
    },
    /// Look up a device (`None` = the first one).
    DeviceInfo {
        /// Device key.
        key: Option<String>,
        /// Where the answer goes.
        reply: oneshot::Sender<Option<DeviceInfo>>,
    },
    /// The effective model and configuration (for the runtime reconciler).
    RuntimePlan {
        /// Where the answer goes.
        reply: oneshot::Sender<RuntimePlan>,
    },
    /// A clock observation from the PTP thread.
    ReportClock(ClockStatus),
    /// The primary interface and the candidates, from the reconciler.
    ReportNetwork(NetworkStatus),
    /// The interfaces that run a PTP port now, from the reconciler.
    ClockInterfaces(Vec<String>),
    /// What the bridges are doing, from the bridge manager.
    ReportBridges(Vec<BridgeStatus>),
    /// One sweep of what the media layer sees of each rx subscription.
    ReportSubscriptions(Vec<(DeviceKey, Channel, SubscriptionState)>),
    /// Flush and stop.
    Shutdown {
        /// Signalled after the final flush; carries the flush error, if any.
        reply: oneshot::Sender<Option<String>>,
    },
}

/// Tells the watchdog whether the engine thread is stuck inside one
/// operation (a file system that stopped answering), without depending on how
/// long ordinary state writes take.
#[derive(Debug, Default)]
pub struct Pulse {
    /// Milliseconds since `origin` at which the current operation began, or 0
    /// when idle.
    busy_since_ms: AtomicU64,
    origin: OnceLock<Instant>,
}

impl Pulse {
    fn origin(&self) -> Instant {
        *self.origin.get_or_init(Instant::now)
    }

    fn enter(&self) {
        let ms = u64::try_from(self.origin().elapsed().as_millis()).unwrap_or(u64::MAX);
        self.busy_since_ms.store(ms.max(1), Ordering::Relaxed);
    }

    fn leave(&self) {
        self.busy_since_ms.store(0, Ordering::Relaxed);
    }

    /// How long the thread has been inside its current operation, if it is in
    /// one.
    #[must_use]
    pub fn busy_for(&self) -> Option<Duration> {
        let since = self.busy_since_ms.load(Ordering::Relaxed);
        if since == 0 {
            return None;
        }
        let now = u64::try_from(self.origin().elapsed().as_millis()).unwrap_or(u64::MAX);
        Some(Duration::from_millis(now.saturating_sub(since)))
    }
}

/// Cloneable, non-blocking client of the engine thread.
#[derive(Debug, Clone)]
pub struct EngineHandle {
    tx: SyncSender<Msg>,
    timeout: Duration,
    pulse: Arc<Pulse>,
}

fn busy(what: &str) -> CommandError {
    CommandError::Internal {
        message: format!("state engine {what}"),
    }
}

impl EngineHandle {
    /// The engine thread's busy indicator.
    #[must_use]
    pub fn pulse(&self) -> &Arc<Pulse> {
        &self.pulse
    }

    fn send(&self, msg: Msg) -> Result<(), CommandError> {
        self.tx.try_send(msg).map_err(|e| match e {
            TrySendError::Full(_) => busy("is busy, retry"),
            TrySendError::Disconnected(_) => busy("is not running"),
        })
    }

    async fn ask<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> Msg,
    ) -> Result<T, CommandError> {
        let (tx, rx) = oneshot::channel();
        self.send(make(tx))?;
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => Err(busy("stopped before answering")),
            Err(_) => Err(busy("did not answer in time")),
        }
    }

    /// Apply a command as a plain (unconfirmed) request: a change that
    /// restarts something is refused for gated origins (CLI, API) with
    /// `ConfirmationRequired`; `Origin::Config`, `Origin::Protocol` and
    /// `Origin::Internal` are never asked.
    ///
    /// # Errors
    /// The engine's refusal, or `Internal` if the engine is busy, stopped or
    /// too slow.
    pub async fn apply(&self, origin: Origin, cmd: Command) -> Result<Applied, CommandError> {
        self.apply_with(origin, cmd, Confirm::No).await
    }

    /// Apply a command with the confirmation it came with.
    ///
    /// # Errors
    /// As [`apply`](Self::apply).
    pub async fn apply_with(
        &self,
        origin: Origin,
        cmd: Command,
        confirm: Confirm,
    ) -> Result<Applied, CommandError> {
        self.ask(|reply| Msg::Apply {
            origin,
            cmd,
            confirm,
            reply,
        })
        .await?
    }

    /// Answer a query.
    ///
    /// # Errors
    /// As for [`apply`](Self::apply).
    pub async fn query(&self, query: Query) -> Result<Reply, CommandError> {
        self.ask(|reply| Msg::Query { query, reply }).await?
    }

    /// Look up a device by key, or the first device when `key` is `None`.
    ///
    /// # Errors
    /// `Internal` if the engine is unavailable.
    pub async fn device_info(
        &self,
        key: Option<String>,
    ) -> Result<Option<DeviceInfo>, CommandError> {
        self.ask(|reply| Msg::DeviceInfo { key, reply }).await
    }

    /// The current effective model and configuration.
    ///
    /// # Errors
    /// `Internal` if the engine is unavailable.
    pub async fn runtime_plan(&self) -> Result<RuntimePlan, CommandError> {
        self.ask(|reply| Msg::RuntimePlan { reply }).await
    }

    /// Report a clock observation (non-blocking; dropped if the queue is
    /// full, as the next observation supersedes it).
    pub fn report_clock(&self, status: ClockStatus) {
        let _ = self.tx.try_send(Msg::ReportClock(status));
    }

    /// Report the interfaces that run a PTP port now (non-blocking). `false`
    /// if the queue was full: the caller reports again on its next pass.
    pub fn report_clock_interfaces(&self, interfaces: Vec<String>) -> bool {
        self.tx.try_send(Msg::ClockInterfaces(interfaces)).is_ok()
    }

    /// Report what the reconciler found on the host (non-blocking). `false`
    /// if the queue was full: the caller reports again on its next pass.
    pub fn report_network(&self, status: NetworkStatus) -> bool {
        self.tx.try_send(Msg::ReportNetwork(status)).is_ok()
    }

    /// Report the bridges' status (non-blocking). `false` if the queue was
    /// full: the caller reports again shortly.
    pub fn report_bridges(&self, rows: Vec<BridgeStatus>) -> bool {
        self.tx.try_send(Msg::ReportBridges(rows)).is_ok()
    }

    /// Report one sweep of subscription states (non-blocking). `false` if the
    /// queue was full: the next sweep, a second later, supersedes it.
    pub fn report_subscriptions(&self, rows: Vec<(DeviceKey, Channel, SubscriptionState)>) -> bool {
        self.tx.try_send(Msg::ReportSubscriptions(rows)).is_ok()
    }

    /// Ask the thread to flush and stop; returns the flush error, if any.
    pub async fn shutdown(&self) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        // A blocking send is fine here: shutdown is not on a request path and
        // the thread drains its queue.
        let tx_chan = self.tx.clone();
        let sent = tokio::task::spawn_blocking(move || {
            tx_chan.send(Msg::Shutdown { reply: tx }).map_err(|_| ())
        })
        .await
        .ok()
        .and_then(Result::ok);
        sent?;
        rx.await.ok().flatten()
    }
}

/// Disarmed on a clean exit; otherwise cancels `fatal` when the thread ends.
struct FatalGuard {
    token: CancellationToken,
    armed: bool,
}

impl Drop for FatalGuard {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

/// Start the engine thread.
///
/// `clock` must be the clock the engine was started with: the persist
/// deadline is on its timeline.
///
/// # Errors
/// The thread could not be spawned.
pub fn spawn(
    engine: StateEngine,
    clock: Arc<MonotonicClock>,
    settings: watch::Sender<Settings>,
    fatal: CancellationToken,
    queue: usize,
    request_timeout: Duration,
) -> std::io::Result<(EngineHandle, JoinHandle<()>)> {
    let (tx, rx) = mpsc::sync_channel(queue.max(1));
    let pulse = Arc::new(Pulse::default());
    let thread_pulse = Arc::clone(&pulse);
    let join = std::thread::Builder::new()
        .name("tessera-state".into())
        .spawn(move || {
            let mut guard = FatalGuard {
                token: fatal,
                armed: true,
            };
            run(engine, &clock, &rx, &settings, &thread_pulse);
            guard.armed = false;
        })?;
    Ok((
        EngineHandle {
            tx,
            timeout: request_timeout,
            pulse,
        },
        join,
    ))
}

fn run(
    mut engine: StateEngine,
    clock: &MonotonicClock,
    rx: &mpsc::Receiver<Msg>,
    settings: &watch::Sender<Settings>,
    pulse: &Pulse,
) {
    use tessera_state::Clock as _;
    let mut shutdown_reply = None;
    // What this process started with: restart-class keys that differ from it
    // are pending until the next service start.
    let started = engine.config().config.clone();
    loop {
        let wait = engine
            .next_persist_deadline()
            .map(|d| d.saturating_sub(clock.now()));
        let msg = match wait {
            Some(t) => match rx.recv_timeout(t) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break,
            },
        };
        pulse.enter();
        if let Some(m) = msg {
            match m {
                Msg::Apply {
                    origin,
                    cmd,
                    confirm,
                    reply,
                } => {
                    let is_reload = matches!(cmd, Command::ReloadConfig);
                    let r = match confirm {
                        Confirm::No => engine.apply(origin, cmd),
                        Confirm::Blind => engine.apply_confirmed(origin, cmd),
                        Confirm::Expecting(t) => engine.apply_expecting(origin, cmd, &t),
                    };
                    if is_reload && r.is_ok() {
                        // Resolve first (the user database may block): the watch
                        // channel's lock is held while the closure runs, and
                        // every connection reads it.
                        let new = Settings::of(&engine, &started);
                        engine.set_persist_delay(new.tunables.persist_delay);
                        engine.set_rate_limit(Some(new.tunables.rate));
                        settings.send_if_modified(|s| {
                            let changed = *s != new;
                            if changed {
                                *s = new;
                            }
                            changed
                        });
                    }
                    let _ = reply.send(r);
                }
                Msg::Query { query, reply } => {
                    let _ = reply.send(engine.query(&query));
                }
                Msg::DeviceInfo { key, reply } => {
                    let _ = reply.send(device_info(&engine, key.as_deref()));
                }
                Msg::RuntimePlan { reply } => {
                    let _ = reply.send(RuntimePlan {
                        model: engine.model().clone(),
                        config: engine.runtime_config(),
                    });
                }
                Msg::ReportClock(status) => engine.report_clock_status(status),
                Msg::ReportNetwork(status) => engine.report_network_status(status),
                Msg::ClockInterfaces(v) => engine.set_clock_interfaces(v),
                Msg::ReportBridges(rows) => engine.report_bridge_status(rows),
                Msg::ReportSubscriptions(rows) => engine.report_subscription_sweep(&rows),
                Msg::Shutdown { reply } => {
                    shutdown_reply = Some(reply);
                    break;
                }
            }
        }
        if let Err(e) = engine.poll_persist() {
            tracing::warn!("cannot write the state file, will retry: {e}");
        }
        pulse.leave();
    }
    let err = engine.flush().err().map(|e| {
        tracing::error!("final state flush failed: {e}");
        e.to_string()
    });
    if let Some(reply) = shutdown_reply {
        let _ = reply.send(err);
    }
}

fn device_info(engine: &StateEngine, key: Option<&str>) -> Option<DeviceInfo> {
    let model = engine.model();
    let d = match key {
        Some(k) => model
            .device(k)
            .or_else(|| model.devices.iter().find(|d| d.name == k))?,
        None => model.devices.first()?,
    };
    Some(DeviceInfo {
        key: d.key.0.clone(),
        rx_channels: d.rx_channels,
        tx_channels: d.tx_channels,
        sample_rate: d.sample_rate,
        latency_us: d.latency_us,
        latency_max_us: d.latency_bounds_us[1].max(d.latency_us),
    })
}
