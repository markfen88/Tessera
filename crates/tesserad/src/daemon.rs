//! Start-up, signal handling and shutdown of the daemon.
//!
//! Order of events:
//!
//! 1. logging, then raise the soft `RLIMIT_NOFILE` (descriptors arrive over
//!    `SCM_RIGHTS`, ADR 0010 amendment);
//! 2. a **read-only** look at the configuration to learn the runtime
//!    directory; then take the instance lock `<runtime dir>/tesserad.lock`.
//!    Nothing is written anywhere before this process holds the lock, so a
//!    second daemon that is turned away cannot disturb the first;
//! 3. bind the sockets (a stale socket of a crashed daemon is replaced only
//!    if connecting is refused), then load configuration and state through
//!    `tessera-state` (fatal on error, the message lists every problem),
//!    spawn the state-engine thread and the PTP and media stubs;
//! 4. build the small tokio runtime, install signal handlers, start the
//!    supervised accept loops, then tell the service manager `READY=1`;
//! 5. `SIGHUP` reloads in its own task (a command through the engine,
//!    `Origin::Config`; `RELOADING=1` / `READY=1` bracket it for
//!    `Type=notify-reload`), so `SIGTERM`/`SIGINT` can preempt a slow reload.
//!    They stop the daemon: `STOPPING=1`, close the listeners and connections,
//!    detach audio clients, flush `state.toml` (bounded by `[daemon]
//!    shutdown_timeout_secs`), remove the sockets *that are still ours*,
//!    exit 0.
//!
//! **Configuration.** Every tunable is a key of the configuration
//! ([`crate::tunables`]); the command-line options are overrides
//! ([`crate::options`]), passed to the loader so that they win over every
//! file, now and on every reload. Each overridden key is logged at start.
//! Keys of apply class `reload` are applied after a reload (new connections,
//! new audio clients, the debouncer, the rate limiters, the shutdown
//! timeout); keys of class `restart` changed by a reload are logged by name
//! and shown in the status until the next service start.
//!
//! **Keys resolved before the lock.** `[daemon] runtime_dir` (unless
//! `--runtime-dir`), `[control] socket_mode`, `socket_group` and `[audio]
//! socket` are read from the files and the command line before the state
//! engine starts, because the lock and the sockets come first. They are
//! restart-class keys that `state.toml` and `locked` cannot carry; if the
//! engine's own load disagrees (the files changed in between), start fails
//! rather than run with two values. Everything else, the debounce window and
//! the rate limit included, is taken from the engine's load.
//!
//! The watchdog is petted from the control runtime on a timer, independent of
//! how long ordinary state writes take; it is withheld only if the state
//! thread has been inside a single operation for [`ENGINE_STALL`] (a stuck
//! file system), which is what the watchdog exists to catch.
//!
//! If the state-engine thread dies unexpectedly the daemon exits with 1 so the
//! service manager restarts it (the state is persisted).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use nix::sys::resource::{Resource, getrlimit, setrlimit};
use tessera_control_types::{Command, Origin};
use tessera_state::{
    Config, ConfigPaths, Effect, Environment, MonotonicClock, Overrides, Paths, RestartReason,
    StateEngine, load_from_sources_with,
};
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::admission::Admission;
use crate::audio::{self, AudioCtx, SharedAudioConfig};
use crate::control::{self, ControlCtx, EffectSink};
use crate::engine_thread::{self, EngineHandle, Settings};
use crate::logging::{self, LogControl};
use crate::metrics::Counters;
use crate::notify::{self, Notifier};
use crate::options::Options;
use crate::peer::resolve_group;
use crate::peerlog::PeerLog;
use crate::ptp::ClockHandle;
use crate::ratelimit::PeerLimiter;
use crate::runtime::{self, Runtime, RuntimeOpts};
use crate::sanitize;
use crate::sockets::{self, BoundFile, GroupSpec, InstanceLock};
use crate::subscriptions;
use crate::tunables::{self, Reloadable};
use crate::{media, ptp, supervisor};

/// How long the state thread may sit inside one operation before the
/// watchdog ping is withheld.
pub const ENGINE_STALL: Duration = Duration::from_secs(120);

/// Run the daemon; returns the process exit code.
#[must_use]
pub fn run(opts: &Options) -> u8 {
    if opts.print_default_config {
        // Before logging, the lock and any file: a pure action.
        print!("{}", tessera_state::reference_config());
        return 0;
    }
    let overrides = match opts.overrides() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("tesserad: {e}");
            return 2;
        }
    };
    let log = logging::init(opts.log_level.as_deref().unwrap_or("info"));
    let notifier = Arc::new(Notifier::from_env());
    match start(opts, overrides, &log, &notifier) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("tesserad: {msg}");
            let _ = notifier.send("STATUS=failed to start\nERRNO=1");
            1
        }
    }
}

/// Raise the soft open-file limit to the hard limit (or as far as the kernel
/// allows).
fn raise_nofile() {
    let Ok((soft, hard)) = getrlimit(Resource::RLIMIT_NOFILE) else {
        return;
    };
    if soft >= hard {
        return;
    }
    if setrlimit(Resource::RLIMIT_NOFILE, hard, hard).is_ok() {
        return;
    }
    // The hard limit may be "infinity", above what the kernel accepts.
    let fallback: nix::libc::rlim_t = 1 << 20;
    if fallback > soft && setrlimit(Resource::RLIMIT_NOFILE, fallback.min(hard), hard).is_err() {
        tracing::warn!("cannot raise RLIMIT_NOFILE (soft limit stays {soft})");
    }
}

fn group_spec(socket_group: Option<&str>) -> Result<GroupSpec, String> {
    match socket_group {
        Some(g) => resolve_group(g).map(|gid| GroupSpec {
            gid: Some(gid),
            explicit: true,
        }),
        None => Ok(match resolve_group("tessera") {
            Ok(gid) => GroupSpec {
                gid: Some(gid),
                explicit: false,
            },
            Err(e) => {
                tracing::warn!("{e}; the sockets keep the daemon's own group");
                GroupSpec {
                    gid: None,
                    explicit: false,
                }
            }
        }),
    }
}

/// What a reload of a section does in this daemon (the log line after
/// `ConfigSectionChanged`). Keys of apply class `restart` in a section are
/// reported separately, by name (`Effect::RestartKeysChanged`).
#[must_use]
pub fn section_note(section: &str) -> &'static str {
    match section {
        "control" => "applies to new control connections",
        "audio" => "applies to new audio clients and to bridges when they next open",
        "daemon" => {
            "applied (log level, admin group, state write delay, shutdown timeout; restart keys are reported separately)"
        }
        "defaults" => {
            "applies to each flow when it next (re)starts and to each bridge at its next card error"
        }
        "network" | "clock" | "discovery" | "backends" => "applied by the runtime reconciler",
        "bridge" | "flow" => "applied: only the entries that changed restart",
        "engine" | "aes67" => "nothing in it applies before the next service restart",
        _ => "applied where it can be; see the keys reported for a restart",
    }
}

/// Follow-up work after commands and reloads.
#[derive(Debug)]
struct DaemonEffects {
    audio: Option<Arc<AudioCtx>>,
}

impl DaemonEffects {
    fn detach(&self, device: &str) {
        if let Some(a) = &self.audio {
            a.detach_device(device);
        }
    }
}

impl EffectSink for DaemonEffects {
    fn on_effects(&self, effects: &[Effect]) {
        for e in effects {
            match e {
                Effect::DeviceAdded(k) => {
                    tracing::info!("device `{}` added", sanitize::log_safe(&k.0, 64));
                }
                Effect::DeviceRemoved(k) => {
                    tracing::info!("device `{}` removed", sanitize::log_safe(&k.0, 64));
                    self.detach(&k.0);
                }
                Effect::DeviceRestartRequired { device, reason } => {
                    // The runtime reconciler rebuilds the device's flows (for
                    // the new counts or interfaces), its bridges and its mDNS
                    // records; its audio clients are detached here at once so
                    // they re-attach with the new ring geometry.
                    let key = sanitize::log_safe(&device.0, 64);
                    match reason {
                        RestartReason::ChannelCount => tracing::info!(
                            "device `{key}` restarts for its new channel counts: flows rebuilt, audio clients re-attach, records re-announced"
                        ),
                        RestartReason::Interface => {
                            tracing::info!("device `{key}` restarts on its new network interfaces")
                        }
                        other => {
                            tracing::info!("device `{key}` restarts ({other:?})");
                        }
                    }
                    self.detach(&device.0);
                }
                Effect::ConfigSectionChanged(s) => {
                    tracing::info!("configuration section `{s}` changed: {}", section_note(s));
                }
                Effect::RestartKeysChanged(keys) => {
                    tracing::warn!(
                        "changed in the configuration, takes effect at the next service restart: {}",
                        keys.join(", ")
                    );
                }
                // A variant added to tessera-state later: nothing to do here
                // yet. Debug only (any peer's command can produce effects).
                other => {
                    tracing::debug!(
                        "follow-up not handled by this daemon: {}",
                        sanitize::log_safe(&format!("{other:?}"), 200)
                    );
                }
            }
        }
    }
}

fn paths(opts: &Options) -> Paths {
    Paths {
        config: ConfigPaths::new(
            opts.config_dir.join("config.toml"),
            opts.config_dir.join("config.d"),
        ),
        state_file: opts.state_dir.as_ref().map(|d| d.join("state.toml")),
    }
}

fn environment() -> Environment {
    let mut env = Environment::detect();
    // A build without the hardware bridge refuses `[[bridge]]` entries at
    // load and reload instead of ignoring them.
    env.bridges_supported = crate::bridge::SUPPORTED;
    env
}

/// The engine's environment with the tunables of the effective
/// configuration: the debounce window and the per-principal command limit
/// (the engine limits commands on every surface; the control socket
/// additionally paces request frames of all kinds, see `control`).
fn with_tunables(mut env: Environment, cfg: &Config) -> Environment {
    let t = Reloadable::of(cfg);
    env.persist_delay = t.persist_delay;
    env.rate_limit = Some(t.rate);
    env
}

/// Remove the socket files this process created (and only those).
fn remove_sockets(files: &[BoundFile]) {
    for f in files {
        f.remove_if_ours();
    }
}

fn start(
    opts: &Options,
    overrides: Overrides,
    log: &LogControl,
    notifier: &Arc<Notifier>,
) -> Result<u8, String> {
    raise_nofile();
    let paths = paths(opts);

    // A read-only look at the effective configuration (the command line
    // included), to learn where the sockets go and how they are made.
    // The reserved legacy file can only contain flows. It cannot affect
    // socket setup, and only the state engine knows whether it is still
    // eligible for fallback. Ignore it during this read-only preflight.
    let mut sources =
        tessera_state::loader::read_sources(&paths.config).map_err(|e| e.to_string())?;
    sources.retain(|s| {
        s.path
            .file_name()
            .is_none_or(|n| n != "90-probe-flows.toml")
    });
    let pre =
        load_from_sources_with(&sources, &environment(), &overrides).map_err(|e| e.to_string())?;
    let runtime_dir: PathBuf = opts
        .runtime_dir
        .clone()
        .unwrap_or_else(|| pre.config.daemon.runtime_dir.clone());
    let want_audio = tunables::want_audio_socket(&pre.config);
    let socket_mode = pre.config.control.socket_mode;
    let socket_group = pre.config.control.socket_group.clone();
    let env = with_tunables(environment(), &pre.config);
    drop(pre);

    // From here on this process is "the daemon": nothing is written before
    // the lock is ours, and the lock is held until the very end.
    let _lock = InstanceLock::acquire(&runtime_dir).map_err(|e| e.to_string())?;

    let group = group_spec(socket_group.as_deref())?;
    let (control_listener, control_file) =
        sockets::bind(&runtime_dir.join("control.sock"), socket_mode, group)
            .map_err(|e| e.to_string())?
            .into_parts();
    let mut files = vec![control_file];
    let audio_listener = if want_audio {
        match sockets::bind(&runtime_dir.join("audio.sock"), socket_mode, group) {
            Ok(b) => {
                let (l, f) = b.into_parts();
                files.push(f);
                Some(l)
            }
            Err(e) => {
                remove_sockets(&files);
                return Err(e.to_string());
            }
        }
    } else {
        None
    };

    let clock = Arc::new(MonotonicClock::new());
    let (engine, report) =
        match StateEngine::start_with_overrides(env, paths, overrides, clock.clone()) {
            Ok(v) => v,
            Err(e) => {
                remove_sockets(&files);
                return Err(e.to_string());
            }
        };
    for w in &report.warnings {
        tracing::warn!("{}", sanitize::log_safe(&w.to_string(), 300));
    }
    for f in &report.config_files {
        tracing::info!("read configuration {}", f.display());
    }
    for key in &engine.config().overridden {
        // Overrides only come from options, so `option_of` knows each key
        // (a test checks it); the key alone if a future source adds others.
        match Options::option_of(key) {
            Some(o) => tracing::info!(
                "the command line (--{o}) overrides `{key}`; a reload does not change it"
            ),
            None => tracing::info!("`{key}` is overridden; a reload does not change it"),
        }
    }
    let cfg = engine.config().config.clone();
    // The engine read the files again, with `state.toml` and `locked`: the
    // keys this process used before it held the lock must agree with it.
    // They are restart-class keys that neither `state.toml` nor `locked`
    // can carry, so they differ only if the files changed in between.
    let resolved_early = [
        (
            "daemon.runtime_dir",
            opts.runtime_dir.is_none() && cfg.daemon.runtime_dir != runtime_dir,
        ),
        (
            "control.socket_mode",
            cfg.control.socket_mode != socket_mode,
        ),
        (
            "control.socket_group",
            cfg.control.socket_group != socket_group,
        ),
        (
            "audio.socket",
            tunables::want_audio_socket(&cfg) != want_audio,
        ),
    ];
    let moved: Vec<&str> = resolved_early
        .iter()
        .filter(|(_, differs)| *differs)
        .map(|(k, _)| *k)
        .collect();
    if !moved.is_empty() {
        remove_sockets(&files);
        return Err(format!(
            "the configuration changed while the daemon was starting ({}); start it again",
            moved.join(", ")
        ));
    }
    // The engine's own reload-class tunables, from what it loaded (the same
    // values the control plane uses below).
    let mut engine = engine;
    let t = Reloadable::of(&cfg);
    engine.set_persist_delay(t.persist_delay);
    engine.set_rate_limit(Some(t.rate));
    if log.env_wins() {
        tracing::info!(
            "RUST_LOG is set: it overrides `[daemon] log_level` (`{}`), now and on reload",
            sanitize::log_safe(&cfg.daemon.log_level, 64)
        );
    } else if let Err(e) = log.set_filter(&cfg.daemon.log_level) {
        tracing::warn!("invalid [daemon] log_level: {e}");
    }

    let bus = engine.event_bus();
    let settings = Settings::of(&engine, &cfg);
    if settings.admin_gid.is_none() {
        tracing::warn!(
            "admin group `{}` does not exist: only root can change settings",
            sanitize::log_safe(&settings.admin_group, 64)
        );
    }
    let (settings_tx, settings_rx) = watch::channel(settings);
    let fatal = CancellationToken::new();
    let (handle, engine_join) = engine_thread::spawn(
        engine,
        clock,
        settings_tx,
        fatal.clone(),
        1024,
        Duration::from_secs(30),
    )
    .map_err(|e| format!("cannot start the state thread: {e}"))?;
    let counters = Arc::new(Counters::new());
    let (clock_handle, clock_join) =
        ptp::spawn(handle.clone()).map_err(|e| format!("cannot start the clock domain: {e}"))?;
    if cfg.engine.workers > 1 {
        tracing::warn!(
            "[engine] workers = {} is not implemented: the media engine runs one worker (sharding is future work)",
            cfg.engine.workers
        );
    }
    let media_cfg = tunables::media_config(&cfg);
    let overlay = clock_handle
        .reader()
        .map_err(|e| format!("cannot map the clock overlay: {e}"))?;
    let (media_handle, media_join) = media::spawn(media_cfg, overlay, Arc::clone(&counters.media))
        .map_err(|e| format!("cannot start the media engine: {e}"))?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(tunables::control_threads(&cfg))
        .thread_name("tessera-control")
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the control runtime: {e}"))?;

    let parts = Parts {
        runtime_opts: RuntimeOpts::of(&cfg),
        handle,
        bus,
        settings_rx,
        fatal,
        clock_handle: clock_handle.clone(),
        media_handle: media_handle.clone(),
        counters,
        control_listener,
        audio_listener,
        notifier: Arc::clone(notifier),
        devices: cfg.devices.len(),
    };
    let done = rt.block_on(serve(parts, log));
    drop(rt);

    media_handle.stop();
    clock_handle.stop();
    let _ = media_join.join();
    let _ = clock_join.join();
    match &done {
        // A state thread that never confirmed the flush may be stuck in a
        // system call: do not wait for it.
        Ok(d) if !d.engine_stuck => {
            let _ = engine_join.join();
        }
        _ => {}
    }
    remove_sockets(&files);
    done.map(|d| d.code)
}

struct Parts {
    /// Restart-class switches, read at start.
    runtime_opts: RuntimeOpts,
    counters: Arc<Counters>,
    handle: EngineHandle,
    bus: tessera_state::EventBus,
    settings_rx: watch::Receiver<Settings>,
    fatal: CancellationToken,
    clock_handle: ClockHandle,
    media_handle: media::MediaHandle,
    control_listener: std::os::unix::net::UnixListener,
    audio_listener: Option<std::os::unix::net::UnixListener>,
    notifier: Arc<Notifier>,
    devices: usize,
}

struct Done {
    code: u8,
    engine_stuck: bool,
}

/// "A reload is running" and "another one was asked for meanwhile".
type ReloadFlags = (Arc<AtomicBool>, Arc<AtomicBool>);

/// Run one reload (and any that were requested while it ran), without ever
/// blocking the signal loop.
fn spawn_reload(
    handle: EngineHandle,
    effects: Arc<dyn EffectSink>,
    notifier: Arc<Notifier>,
    shutdown: CancellationToken,
    (running, again): ReloadFlags,
    devices: usize,
    settings: watch::Receiver<Settings>,
) {
    tokio::spawn(async move {
        loop {
            notifier.reloading();
            let result = tokio::select! {
                () = shutdown.cancelled() => return,
                r = handle.apply(Origin::Config, Command::ReloadConfig) => r,
            };
            match result {
                Ok(applied) => {
                    tracing::info!("configuration reloaded");
                    effects.on_effects(&applied.effects);
                    // What still waits for a service restart (against the
                    // start, not only this reload's change); the state thread
                    // published it before it answered.
                    let restart = settings.borrow().restart_pending.len();
                    notifier.status(&if restart > 0 {
                        format!(
                            "serving {devices} device(s); {restart} changed key(s) take effect at the next service restart"
                        )
                    } else {
                        format!("serving {devices} device(s)")
                    });
                }
                Err(e) => {
                    let why = sanitize::log_safe(&e.to_string(), 300);
                    tracing::error!("reload failed, keeping the current configuration: {why}");
                    notifier.status(&format!(
                        "reload failed (still running the old configuration): {why}"
                    ));
                }
            }
            notifier.ready();
            if again.swap(false, Ordering::AcqRel) {
                continue; // another request arrived meanwhile
            }
            running.store(false, Ordering::Release);
            // A request that set `again` between the check and the store.
            if again.swap(false, Ordering::AcqRel) && !running.swap(true, Ordering::AcqRel) {
                continue;
            }
            return;
        }
    });
}

/// Audio ring sizing (`[audio]`), shared by audio clients and the
/// hardware-card bridges; the receive margin comes from the media engine.
fn audio_config(t: &Reloadable, media: &media::MediaHandle) -> audio::AudioConfig {
    t.audio
        .audio_config(u32::try_from(media.client_margin_ns() / 1_000).unwrap_or(u32::MAX))
}

/// Apply the tunables of apply class `reload` after a reload changed them.
fn apply_reloadable(
    t: &Reloadable,
    media: &media::MediaHandle,
    control: &ControlCtx,
    shared_audio: &SharedAudioConfig,
    audio: Option<&Arc<AudioCtx>>,
) {
    let a = &t.admission;
    control
        .admission
        .set_caps(a.max_total, a.reserved, a.per_uid);
    control
        .limiter
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .set_config(t.rate);
    shared_audio.set(audio_config(t, media));
    if let Some(a) = audio {
        a.limits_changed();
    }
}

/// The bridge manager (a thread that is idle without `[[bridge]]`).
#[cfg(feature = "alsa-bridge")]
fn start_bridges(
    p: &Parts,
    audio: Arc<SharedAudioConfig>,
) -> Result<Option<Arc<crate::bridge::BridgeManager>>, String> {
    crate::bridge::spawn(crate::bridge::BridgeDeps {
        engine: p.handle.clone(),
        clock: p.clock_handle.clone(),
        media: p.media_handle.clone(),
        audio,
    })
    .map(|m| Some(Arc::new(m)))
    .map_err(|e| format!("cannot start the bridge manager: {e}"))
}

/// Without the feature there is nothing to start (bridges are refused at
/// load).
#[cfg(not(feature = "alsa-bridge"))]
#[allow(clippy::unnecessary_wraps)]
fn start_bridges(
    _p: &Parts,
    _audio: Arc<SharedAudioConfig>,
) -> Result<Option<Arc<crate::bridge::BridgeManager>>, String> {
    Ok(None)
}

async fn serve(p: Parts, log: &LogControl) -> Result<Done, String> {
    let io_err = |what: &str, e: std::io::Error| format!("{what}: {e}");
    let mut hup = signal(SignalKind::hangup()).map_err(|e| io_err("SIGHUP handler", e))?;
    let mut term = signal(SignalKind::terminate()).map_err(|e| io_err("SIGTERM handler", e))?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| io_err("SIGINT handler", e))?;

    let shutdown = CancellationToken::new();
    let tracker = TaskTracker::new();
    let counters = Arc::clone(&p.counters);

    let tun = p.settings_rx.borrow().tunables.clone();
    let shared_audio = SharedAudioConfig::new(audio_config(&tun, &p.media_handle));
    let audio_ctx = match &p.audio_listener {
        Some(_) => Some(Arc::new(AudioCtx::new(
            p.handle.clone(),
            p.clock_handle.clone(),
            p.media_handle.clone(),
            Arc::clone(&shared_audio),
            shutdown.clone(),
            Arc::clone(&counters),
        ))),
        None => None,
    };
    let bridges = start_bridges(&p, Arc::clone(&shared_audio))?;
    let effects: Arc<dyn EffectSink> = Arc::new(DaemonEffects {
        audio: audio_ctx.clone(),
    });

    // The runtime reconciler (clock, flows, discovery). Wait for its first
    // pass so that flows that cannot start are reported before `READY=1`.
    let (first_tx, first_rx) = tokio::sync::oneshot::channel();
    tracker.spawn(runtime::run(
        Runtime {
            engine: p.handle.clone(),
            bus: p.bus.clone(),
            clock: p.clock_handle.clone(),
            media: p.media_handle.clone(),
            audio: audio_ctx.clone(),
            bridges: bridges.clone(),
            opts: p.runtime_opts,
            shutdown: shutdown.clone(),
        },
        first_tx,
    ));
    if tokio::time::timeout(Duration::from_secs(10), first_rx)
        .await
        .is_err()
    {
        tracing::warn!("the first runtime pass did not finish within 10 s; continuing");
    }

    // The live state of the RX subscriptions, sampled once a second from the
    // media engine's flow counters.
    tracker.spawn(subscriptions::run(
        p.handle.clone(),
        Arc::clone(p.media_handle.stats()),
        shutdown.clone(),
    ));

    let limiter = PeerLimiter::new(tun.rate);
    let control_ctx = Arc::new(ControlCtx {
        engine: p.handle.clone(),
        bus: p.bus.clone(),
        limiter: Mutex::new(limiter),
        settings: p.settings_rx.clone(),
        effects: Arc::clone(&effects),
        shutdown: shutdown.clone(),
        started: Instant::now(),
        admission: Admission::new(
            tun.admission.max_total,
            tun.admission.reserved,
            tun.admission.per_uid,
        ),
        counters: Arc::clone(&counters),
        peerlog: PeerLog::new(Duration::from_secs(5)),
    });

    let control_listener = Arc::new(
        UnixListener::from_std(p.control_listener).map_err(|e| io_err("control socket", e))?,
    );
    let (sup_control, restarts) = supervisor::supervise("control listener", shutdown.clone(), {
        let (l, c, t) = (control_listener, Arc::clone(&control_ctx), tracker.clone());
        move || control::serve(Arc::clone(&l), Arc::clone(&c), t.clone())
    });
    counters.track_restarts(restarts);
    let mut supervised = vec![sup_control];
    if let (Some(l), Some(ctx)) = (p.audio_listener, &audio_ctx) {
        let l = Arc::new(UnixListener::from_std(l).map_err(|e| io_err("audio socket", e))?);
        let (h, restarts) = supervisor::supervise("audio listener", shutdown.clone(), {
            let (c, t) = (Arc::clone(ctx), tracker.clone());
            move || audio::serve(Arc::clone(&l), Arc::clone(&c), t.clone())
        });
        counters.track_restarts(restarts);
        supervised.push(h);
    }

    // Watchdog: a timer on the control runtime. Slow state writes do not stop
    // it; only a state thread stuck inside one operation for ENGINE_STALL does.
    if let Some(every) = notify::watchdog_interval() {
        let (n, h, sd) = (Arc::clone(&p.notifier), p.handle.clone(), shutdown.clone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = sd.cancelled() => return,
                    () = tokio::time::sleep(every) => {}
                }
                match h.pulse().busy_for() {
                    Some(d) if d >= ENGINE_STALL => {
                        tracing::error!(
                            "the state thread has been inside one operation for {d:?}; withholding the watchdog ping"
                        );
                    }
                    _ => n.watchdog(),
                }
            }
        });
    }

    let mut settings_rx = p.settings_rx.clone();
    let mut settings_alive = true;
    p.notifier
        .status(&format!("serving {} device(s)", p.devices));
    p.notifier.ready();
    tracing::info!("ready");

    let reloading = Arc::new(AtomicBool::new(false));
    let reload_again = Arc::new(AtomicBool::new(false));
    let mut code = 0u8;
    loop {
        tokio::select! {
            _ = hup.recv() => {
                tracing::info!("SIGHUP: reloading the configuration");
                if reloading.swap(true, Ordering::AcqRel) {
                    reload_again.store(true, Ordering::Release);
                } else {
                    spawn_reload(
                        p.handle.clone(),
                        Arc::clone(&effects),
                        Arc::clone(&p.notifier),
                        shutdown.clone(),
                        (Arc::clone(&reloading), Arc::clone(&reload_again)),
                        p.devices,
                        p.settings_rx.clone(),
                    );
                }
            }
            _ = term.recv() => { tracing::info!("SIGTERM: shutting down"); break; }
            _ = int.recv() => { tracing::info!("SIGINT: shutting down"); break; }
            () = p.fatal.cancelled() => {
                tracing::error!("the state thread died; exiting so that the service manager restarts the daemon");
                code = 1;
                break;
            }
            changed = settings_rx.changed(), if settings_alive => {
                if changed.is_err() {
                    // The state thread is gone (the fatal arm handles it):
                    // stop polling a closed channel.
                    settings_alive = false;
                } else {
                    let s = settings_rx.borrow_and_update().clone();
                    // Logged before the filter changes, so that a test (and
                    // a reader of the log) can tell the new settings are in.
                    tracing::info!(
                        "reloaded settings applied (log level `{}`)",
                        sanitize::log_safe(&s.log_level, 64)
                    );
                    // `--log-level` is an override: the loader keeps it, so
                    // `s.log_level` is already the command line's value.
                    // `RUST_LOG` wins over both, now and on every reload
                    // (`set_filter` would ignore the call too; the guard
                    // says it here).
                    if !log.env_wins() {
                        if let Err(e) = log.set_filter(&s.log_level) {
                            tracing::warn!("invalid [daemon] log_level `{}`: {e}", sanitize::log_safe(&s.log_level, 64));
                        }
                    }
                    apply_reloadable(
                        &s.tunables,
                        &p.media_handle,
                        &control_ctx,
                        &shared_audio,
                        audio_ctx.as_ref(),
                    );
                }
            }
        }
    }

    p.notifier.stopping();
    shutdown.cancel();
    tracker.close();
    if tokio::time::timeout(Duration::from_secs(5), tracker.wait())
        .await
        .is_err()
    {
        tracing::warn!("some connections did not close within 5 s");
    }
    for h in supervised {
        let _ = h.await;
    }
    if let Some(b) = &bridges {
        // Joins the bridge threads: off the executor.
        let b = Arc::clone(b);
        let _ = tokio::task::spawn_blocking(move || b.stop()).await;
    }
    let mut engine_stuck = false;
    let shutdown_timeout = p.settings_rx.borrow().tunables.shutdown_timeout;
    match tokio::time::timeout(shutdown_timeout, p.handle.shutdown()).await {
        Ok(None) => {}
        Ok(Some(e)) => {
            tracing::error!("state was not saved: {e}");
            code = 1;
        }
        Err(_) => {
            tracing::error!(
                "the state flush did not finish within {} s; exiting without it",
                shutdown_timeout.as_secs()
            );
            engine_stuck = true;
            code = 1;
        }
    }
    tracing::info!("stopped");
    Ok(Done { code, engine_stuck })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_soft_descriptor_limit_is_raised_as_far_as_allowed() {
        let (before, hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
        raise_nofile();
        let (after, hard_after) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
        assert_eq!(hard, hard_after, "the hard limit is never touched");
        assert!(after >= before);
        // Either everything was granted, or the kernel capped it far above
        // the default of 1024.
        let cap: nix::libc::rlim_t = 1 << 16;
        assert!(
            after == hard || after >= cap.min(hard),
            "soft limit {after}"
        );
    }
}
