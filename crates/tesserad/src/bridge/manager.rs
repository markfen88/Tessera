//! The bridge manager: one control thread (normal priority) that opens the
//! cards, builds the bridges, attaches their rings to the media engine,
//! retries with a back-off, applies new plans and reports the status; and
//! one real-time thread per running bridge ([`super::rt_loop`]).
//!
//! * **Opening** (control thread, allocation allowed): the card
//!   ([`AlsaCard::open`], non-blocking), the bridge and its resampler, a ring
//!   for the bridge's channel range sized like an audio client's
//!   ([`crate::audio::plan_ring`], with room for the bridge's own reads), the
//!   media session for the ring's other end (without a wake socket: the
//!   bridge runs on its card's clock), and the clock overlay. Only then is
//!   the bridge moved to its thread.
//! * **Hand-off** to the running loop only through the bridge's lock-free
//!   queues ([`BridgeHandle`]): the ring and the clock are attached before
//!   the thread starts, live retunes ([`BridgeHandle::set_loop`], the
//!   controller built here) while it runs. The thread hands the bridge back
//!   when it ends, and it is dropped here.
//! * **Failures.** A card that is missing, busy or cannot be configured, a
//!   card that is unplugged while running, or a card that keeps failing
//!   (an xrun or card error in each of [`TROUBLE_WINDOWS`] consecutive
//!   seconds) is closed and reopened after a back-off (state `reopening`):
//!   `error_backoff_min_ms`, doubling up to `error_backoff_max_ms` (by
//!   default 0.5 s to 30 s; `error_backoff = false` waits the minimum every
//!   time), from the bridge's entry over `[defaults.bridge]`, read at each
//!   error ([`BridgePlan::reopen_wait`]; ADR 0014). A run of 10 s resets
//!   it. Single xruns are recovered in place by the
//!   bridge and only counted. A configuration the bridge cannot run (the
//!   library refuses the card's sizes, a pinned rate the device no longer
//!   has) is `failed` until the next plan change. A panic on a bridge thread
//!   (a bug) is caught there: that bridge is `failed`, the daemon and the
//!   other bridges go on. A panic on the manager thread itself aborts the
//!   daemon (ADR 0004: never half-alive; the service manager restarts it).
//! * **Plans** come from the runtime reconciler. A bridge whose card,
//!   direction, channels, sizes, rate, latency, resampler or scheduling
//!   changed is restarted; a changed time constant or clamp is applied live
//!   (retried until the bridge's queue takes it; the status shows the tuning
//!   in effect); an unchanged bridge is not touched.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tessera_audio_ipc::proto::Hello;
use tessera_audio_ipc::{Direction as RingDir, Ring, SampleFormat};
use tessera_backend_alsa::bridge::{
    AlsaCard, Bridge, BridgeConfig as LibConfig, BridgeError, BridgeHandle, BridgeStats,
    CardParams, CardPcm, ErrorSleep, LocalClock, LoopParams, RxLink, TxLink,
};
use tessera_backend_alsa::clock::OverlayClock;
use tessera_control_types::{BridgeDirection as WireDir, BridgeState, BridgeStatus, DeviceKey};
use tessera_state::config::BridgeDirection;

use super::plan::{BridgePlan, PlanError};
use super::rt_loop::{self, Health, LoopEnd, ThreadSetup};
use crate::audio::{AudioConfig, SharedAudioConfig, plan_ring};
use crate::engine_thread::{DeviceInfo, EngineHandle};
use crate::media::worker::SessionEnd;
use crate::media::{MediaHandle, SessionSpec};
use crate::ptp::ClockHandle;
use crate::sanitize::log_safe;

/// Consecutive one-second windows with an xrun or a card error after which
/// a running card is closed and reopened.
pub const TROUBLE_WINDOWS: u32 = 3;
/// A run this long resets the back-off.
const STABLE: Duration = Duration::from_secs(10);
/// How often the status is reported to the state engine.
const REPORT_EVERY: Duration = Duration::from_secs(1);
/// Media session ids of bridges (audio clients count up from 1).
const SESSION_BASE: u64 = 1 << 62;
/// Input frames a resampler call may need beyond a chunk (filter delay).
const RESAMPLER_MARGIN: u32 = 1024;

/// What can keep a bridge from running, on the daemon's side of the library.
#[derive(Debug)]
pub enum ManagerError {
    /// The card could not be opened or configured (missing, busy, the
    /// device's rate or a format it refuses): retried with a back-off,
    /// whatever variant the library used.
    Open(BridgeError),
    /// Building the bridge on the opened card failed (sizes, resampler):
    /// `Config` and `Resample` are a configuration it cannot run.
    Bridge(BridgeError),
    /// Handing the ring or the clock to the new bridge failed (its queue):
    /// retried.
    Handoff(BridgeError),
    /// The ring for the bridge's channel range.
    Ring(String),
    /// The ring the bridge needs is beyond the ring-size limit (a slow loop
    /// at a high rate or with many channels): not retried.
    RingTooLarge(String),
    /// The clock overlay could not be mapped.
    Clock(std::io::Error),
    /// The media engine did not take the session (queue full or stopped).
    MediaBusy,
    /// The bridge thread could not be started.
    Thread(std::io::Error),
}

impl ManagerError {
    /// A configuration the bridge cannot run: not retried until a change.
    fn fatal(&self) -> bool {
        matches!(
            self,
            ManagerError::Bridge(BridgeError::Config(_) | BridgeError::Resample(_))
                | ManagerError::RingTooLarge(_)
        )
    }
}

impl std::fmt::Display for ManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManagerError::Open(e) | ManagerError::Bridge(e) => write!(f, "{e}"),
            ManagerError::Handoff(e) => write!(f, "hand-off to the bridge: {e}"),
            ManagerError::Ring(m) => write!(f, "ring: {m}"),
            ManagerError::RingTooLarge(m) => write!(
                f,
                "the ring this bridge needs is too large ({m}): lower its time constant or clamp, or raise --audio-max-ring-mib"
            ),
            ManagerError::Clock(e) => write!(f, "clock overlay: {e}"),
            ManagerError::MediaBusy => write!(f, "the media engine is busy or gone"),
            ManagerError::Thread(e) => write!(f, "cannot start the bridge thread: {e}"),
        }
    }
}

impl std::error::Error for ManagerError {}

impl From<BridgeError> for ManagerError {
    fn from(e: BridgeError) -> Self {
        ManagerError::Bridge(e)
    }
}

/// What the manager needs from the rest of the daemon.
#[derive(Debug, Clone)]
pub struct BridgeDeps {
    /// Where the status goes.
    pub engine: EngineHandle,
    /// The media clock overlay.
    pub clock: ClockHandle,
    /// The media engine that serves the bridges' rings.
    pub media: MediaHandle,
    /// Ring sizing, as for audio clients (`rx_margin_us` included); read at
    /// each open, so a reload of `[audio]` applies at the next one.
    pub audio: Arc<SharedAudioConfig>,
}

enum Msg {
    Plans(Vec<BridgePlan>, Vec<PlanError>),
    Stop,
}

impl std::fmt::Debug for Msg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Msg")
    }
}

/// Handle to the bridge manager thread.
#[derive(Debug)]
pub struct BridgeManager {
    tx: Sender<Msg>,
    join: std::sync::Mutex<Option<JoinHandle<()>>>,
}

/// Starts the manager thread.
///
/// # Errors
/// The thread could not be spawned.
pub fn spawn(deps: BridgeDeps) -> std::io::Result<BridgeManager> {
    let (tx, rx) = mpsc::channel();
    let join = std::thread::Builder::new()
        .name("tessera-bridges".into())
        .spawn(move || Manager::new(deps).run(&rx))?;
    Ok(BridgeManager {
        tx,
        join: std::sync::Mutex::new(Some(join)),
    })
}

impl BridgeManager {
    /// Make exactly these bridges run; `errors` are bridges that cannot run
    /// (shown as `failed` with the reason).
    pub fn apply(&self, plans: Vec<BridgePlan>, errors: Vec<PlanError>) {
        let _ = self.tx.send(Msg::Plans(plans, errors));
    }

    /// Stops every bridge and the manager (waits for them).
    pub fn stop(&self) {
        let _ = self.tx.send(Msg::Stop);
        let join = self
            .join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(j) = join {
            let _ = j.join();
        }
    }
}

/// Counters of the previous runs of one bridge (a reopen starts a new
/// bridge with fresh counters).
#[derive(Debug, Default, Clone, Copy)]
struct Totals {
    xruns: u64,
    card_errors: u64,
    chunks: u64,
    silence: u64,
    missing: u64,
    slips: u64,
    short: u64,
}

struct Running {
    handle: BridgeHandle,
    rt: JoinHandle<(Bridge, LoopEnd)>,
    stop: Arc<AtomicBool>,
    health: Arc<Health>,
    session: u64,
    /// The bridge's own latency: its guard (playback) or lead (capture).
    added_latency: u64,
    _ring: Ring,
    since: Instant,
    last_check: Instant,
    last_trouble: u64,
    trouble: u32,
}

enum Phase {
    Waiting { until: Instant },
    Running(Box<Running>),
    Failed,
}

struct Instance {
    plan: BridgePlan,
    phase: Phase,
    /// Successful opens.
    opened: u64,
    open_failures: u64,
    /// Of the latest successful open.
    generation: u64,
    /// Consecutive failures since the last stable run (sizes the next wait).
    failures: u32,
    last_error: Option<String>,
    totals: Totals,
    /// Stats of the current (or last) bridge, for the gauges.
    stats: Option<Arc<BridgeStats>>,
    rt_degraded: bool,
    /// The loop tuning in effect on the running bridge: (time constant s,
    /// clamp ppm). Differs from the plan while a retune is pending.
    applied: (f64, f64),
    /// `last_error` is the clamp-below-trend warning (cleared once the loop
    /// is back within the clamp).
    clamp_warning: Option<String>,
}

/// What a pending retune did.
enum Retune {
    Nothing,
    Changed,
    /// The new tuning does not fit the open bridge: reopen it.
    Reopen(String),
}

struct Manager {
    deps: BridgeDeps,
    bridges: BTreeMap<String, Instance>,
    failed: Vec<PlanError>,
    next_session: AtomicU64,
    next_generation: u64,
}

fn stat_of(s: &BridgeStats, playback: bool) -> Totals {
    let g = |a: &AtomicU64| a.load(Relaxed);
    if playback {
        Totals {
            xruns: g(&s.rx_xruns),
            card_errors: g(&s.card_errors),
            chunks: g(&s.rx_chunks),
            silence: g(&s.rx_silence_chunks),
            missing: g(&s.rx_missing_frames),
            slips: g(&s.rx_slips),
            short: g(&s.rx_short_frames),
        }
    } else {
        Totals {
            xruns: g(&s.tx_xruns),
            card_errors: g(&s.card_errors),
            chunks: g(&s.tx_chunks),
            silence: g(&s.tx_discarded_chunks),
            missing: g(&s.tx_late_frames),
            slips: g(&s.tx_slips),
            short: 0,
        }
    }
}

impl Totals {
    fn plus(self, o: Totals) -> Totals {
        Totals {
            xruns: self.xruns + o.xruns,
            card_errors: self.card_errors + o.card_errors,
            chunks: self.chunks + o.chunks,
            silence: self.silence + o.silence,
            missing: self.missing + o.missing,
            slips: self.slips + o.slips,
            short: self.short + o.short,
        }
    }
}

fn wire_dir(d: BridgeDirection) -> WireDir {
    match d {
        BridgeDirection::Playback => WireDir::Playback,
        BridgeDirection::Capture => WireDir::Capture,
    }
}

fn panic_text(p: &(dyn Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no message)".into())
}

/// Hand a plan's pause setting to a running bridge thread (it reads it at
/// its next error).
fn push_error_sleep(health: &Health, p: &BridgePlan) {
    health.error_sleep.set(error_sleep_of(p));
}

/// The library's form of the plan's `error_sleep*`.
fn error_sleep_of(p: &BridgePlan) -> ErrorSleep {
    ErrorSleep {
        doubling: p.error_sleep.doubling,
        min_ms: p.error_sleep.min_ms,
        max_ms: p.error_sleep.max_ms,
    }
}

impl Instance {
    /// Take a new plan for the same card ([`BridgePlan::same_card`]): the
    /// manager's back-off reads it at the next failure, a running bridge
    /// thread gets the new pause through its lock-free setting at once (it
    /// reads it at its next error), and the loop tuning is retuned by
    /// `service`.
    fn update_plan(&mut self, p: BridgePlan) {
        if let Phase::Running(run) = &self.phase {
            push_error_sleep(&run.health, &p);
        }
        self.plan = p;
    }

    fn new(plan: BridgePlan) -> Self {
        let applied = (plan.time_constant_s, plan.max_correction_ppm);
        Self {
            plan,
            phase: Phase::Waiting {
                until: Instant::now(),
            },
            opened: 0,
            open_failures: 0,
            generation: 0,
            failures: 0,
            last_error: None,
            totals: Totals::default(),
            stats: None,
            rt_degraded: false,
            applied,
            clamp_warning: None,
        }
    }

    fn playback(&self) -> bool {
        self.plan.direction == BridgeDirection::Playback
    }

    fn state(&self) -> BridgeState {
        match &self.phase {
            Phase::Running(_) => BridgeState::Running,
            Phase::Failed => BridgeState::Failed,
            Phase::Waiting { .. } if self.opened == 0 && self.last_error.is_none() => {
                BridgeState::Starting
            }
            Phase::Waiting { .. } => BridgeState::Reopening,
        }
    }

    fn row(&self) -> BridgeStatus {
        let p = &self.plan;
        let mut r = BridgeStatus::new(
            p.name.as_str(),
            DeviceKey(p.device.clone()),
            p.pcm.as_str(),
            wire_dir(p.direction),
            self.state(),
        );
        r.first_channel = p.first_channel;
        r.channels = p.channels;
        let running = matches!(self.phase, Phase::Running(_));
        (r.time_constant_s, r.max_correction_ppm) = if running {
            self.applied
        } else {
            (p.time_constant_s, p.max_correction_ppm)
        };
        r.reopens = self.opened.saturating_sub(1);
        r.open_failures = self.open_failures;
        r.generation = self.generation;
        r.last_error.clone_from(&self.last_error);
        r.rt_degraded = self.rt_degraded;
        let mut t = self.totals;
        if let Some(s) = &self.stats {
            let pb = self.playback();
            if running {
                t = t.plus(stat_of(s, pb));
            }
            let (fill, err, trim, trend) = if pb {
                (&s.rx_queue, &s.rx_err, &s.rx_trim_ppb, &s.rx_trend_ppb)
            } else {
                (&s.tx_queue, &s.tx_err, &s.tx_trim_ppb, &s.tx_trend_ppb)
            };
            if running {
                r.fill_frames = fill.load(Relaxed);
                r.position_error_frames = err.load(Relaxed);
            }
            r.correction_ppb = trim.load(Relaxed);
            r.trend_ppb = trend.load(Relaxed);
            r.ratio = 1.0 + r.correction_ppb as f64 * 1e-9;
        }
        if let Phase::Running(run) = &self.phase {
            r.rt_degraded |= run.health.rt_degraded.load(Relaxed);
            r.added_latency_frames = run.added_latency;
        }
        r.xruns = t.xruns;
        r.card_errors = t.card_errors;
        r.chunks = t.chunks;
        r.silence_chunks = t.silence;
        r.missing_frames = t.missing;
        r.slips = t.slips;
        r.short_frames = t.short;
        r
    }

    fn trend_ppb(&self) -> i64 {
        self.stats.as_ref().map_or(0, |s| {
            if self.playback() {
                s.rx_trend_ppb.load(Relaxed)
            } else {
                s.tx_trend_ppb.load(Relaxed)
            }
        })
    }

    /// Applies a pending retune to the running bridge.
    fn retune(&mut self) -> Retune {
        let want = (self.plan.time_constant_s, self.plan.max_correction_ppm);
        if self.clamp_warning.is_some()
            && (self.trend_ppb().unsigned_abs() as f64) < want.1 * 1000.0
        {
            // The loop is back within its clamp: clear the warning, unless
            // another error has replaced it meanwhile.
            let w = self.clamp_warning.take();
            if self.last_error == w {
                self.last_error = None;
                if self.applied == want {
                    return Retune::Changed;
                }
            }
        }
        if self.applied == want {
            return Retune::Nothing;
        }
        let Phase::Running(run) = &mut self.phase else {
            return Retune::Nothing;
        };
        let name = log_safe(&self.plan.name, 64);
        match run.handle.set_loop(loop_params(&self.plan)) {
            Ok(()) => {
                self.applied = want;
                tracing::info!(
                    "bridge `{name}`: loop retuned live (time constant {} s, clamp {} ppm)",
                    want.0,
                    want.1
                );
                let trend_ppm = self.trend_ppb().unsigned_abs() as f64 / 1000.0;
                if trend_ppm >= want.1 {
                    let msg = format!(
                        "the new clamp of {} ppm is not above the {trend_ppm:.1} ppm offset the loop has learned: it will fall behind and slip",
                        want.1
                    );
                    tracing::warn!("bridge `{name}`: {msg}");
                    self.clamp_warning = Some(msg.clone());
                    self.last_error = Some(msg);
                }
                Retune::Changed
            }
            // The bridge's queue is full: retried on the next pass.
            Err(BridgeError::Busy) => Retune::Nothing,
            // A slower loop or a larger clamp than the open bridge's guard,
            // lead and thresholds hold: build it afresh with the new tuning.
            Err(BridgeError::NeedsReopen(m)) => Retune::Reopen(m),
            Err(e) => {
                let msg = format!("cannot retune the loop: {e}");
                if self.last_error.as_deref() != Some(msg.as_str()) {
                    tracing::warn!("bridge `{name}`: {msg}");
                    self.last_error = Some(msg);
                    return Retune::Changed;
                }
                Retune::Nothing
            }
        }
    }
}

/// The ring for a bridge's channel range: sized like an audio client's whose
/// period covers everything the bridge reads or writes at once beyond the
/// latency: the card buffer, one more period, the guard (playback) or lead
/// (capture) the library computed (grown for a slow loop's transient) and
/// the resampler's look-ahead. The bridge is configured by the operator, not
/// an untrusted client: the clients' period cap does not apply to it, the
/// ring-size limit does (a ring beyond it is a configuration this daemon
/// cannot run: lower the time constant or the clamp, or raise
/// `--audio-max-ring-mib`). Returns the ring and its effective latency.
///
/// # Errors
/// [`ManagerError::RingTooLarge`] (fatal) or [`ManagerError::Ring`].
pub fn bridge_ring(
    p: &BridgePlan,
    buffer: u64,
    period: u64,
    ahead: u64,
    audio: &AudioConfig,
) -> Result<(tessera_audio_ipc::RingParams, u64), ManagerError> {
    let playback = p.direction == BridgeDirection::Playback;
    let reach = u32::try_from(buffer + period + ahead)
        .unwrap_or(u32::MAX)
        .saturating_add(RESAMPLER_MARGIN);
    let mut audio = audio.clone();
    audio.max_period_frames = audio.max_period_frames.max(reach);
    let hello = Hello {
        direction: if playback { RingDir::Rx } else { RingDir::Tx },
        format: SampleFormat::I32,
        first_channel: u32::from(p.first_channel),
        channel_count: u32::from(p.channels),
        sample_rate_hz: p.rate,
        max_period_frames: reach,
        endpoint: p.device.clone(),
        client: format!("bridge {}", p.name),
    };
    let (params, latency) = plan_ring(&hello, &Manager::device_info(p), &audio)
        .map_err(|r| ManagerError::Ring(r.message))?;
    params
        .validate(&audio.limits)
        .map_err(|e| ManagerError::RingTooLarge(e.to_string()))?;
    Ok((params, latency))
}

fn loop_params(p: &BridgePlan) -> LoopParams {
    LoopParams {
        bandwidth_hz: p.loop_bandwidth_hz(),
        max_correction: p.max_correction(),
    }
}

impl Manager {
    fn new(deps: BridgeDeps) -> Self {
        Self {
            deps,
            bridges: BTreeMap::new(),
            failed: Vec::new(),
            next_session: AtomicU64::new(SESSION_BASE),
            next_generation: 1,
        }
    }

    fn run(mut self, rx: &mpsc::Receiver<Msg>) {
        // The manager owns every bridge thread's hand-back: if it died, they
        // would run unsupervised. A panic here aborts the daemon.
        let _abort = crate::rt::AbortOnPanic;
        let mut last_report = Instant::now();
        let mut dirty = false;
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Msg::Plans(p, e)) => {
                    self.reconcile(p, e);
                    dirty = true;
                }
                Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            let names: Vec<String> = self.bridges.keys().cloned().collect();
            for n in names {
                dirty |= self.service(&n);
            }
            if dirty || last_report.elapsed() >= REPORT_EVERY {
                if self.deps.engine.report_bridges(self.rows()) {
                    dirty = false;
                }
                last_report = Instant::now();
            }
        }
        let names: Vec<String> = self.bridges.keys().cloned().collect();
        for n in names {
            if let Some(mut i) = self.bridges.remove(&n) {
                self.close(&mut i, None);
            }
        }
        let _ = self.deps.engine.report_bridges(Vec::new());
    }

    fn rows(&self) -> Vec<BridgeStatus> {
        let mut rows: Vec<BridgeStatus> = self.bridges.values().map(Instance::row).collect();
        for e in &self.failed {
            let mut r = BridgeStatus::new(
                e.name.as_str(),
                DeviceKey(e.device.clone()),
                e.pcm.as_str(),
                wire_dir(e.direction),
                BridgeState::Failed,
            );
            r.first_channel = e.first_channel;
            r.channels = e.channels;
            r.last_error = Some(e.reason.clone());
            rows.push(r);
        }
        rows
    }

    fn reconcile(&mut self, plans: Vec<BridgePlan>, errors: Vec<PlanError>) {
        // Remove what is gone or must restart.
        let names: Vec<String> = self.bridges.keys().cloned().collect();
        for n in names {
            let keep = plans
                .iter()
                .find(|p| p.name == n)
                .is_some_and(|p| self.bridges.get(&n).is_some_and(|i| i.plan.same_card(p)));
            if !keep {
                if let Some(mut i) = self.bridges.remove(&n) {
                    tracing::info!("bridge `{}`: stopping", log_safe(&n, 64));
                    self.close(&mut i, None);
                }
            }
        }
        for p in plans {
            match self.bridges.get_mut(&p.name) {
                // Only the loop tuning and the error waits can differ (see
                // above): the plan is stored, so the tuning is applied live by
                // `service` (and retried there if the queue is full), and the
                // waits apply at the next error.
                Some(i) => i.update_plan(p),
                None => {
                    self.bridges.insert(p.name.clone(), Instance::new(p));
                }
            }
        }
        for e in &errors {
            if !self.failed.iter().any(|f| f == e) {
                tracing::error!(
                    "bridge `{}` cannot run: {}",
                    log_safe(&e.name, 64),
                    e.reason
                );
            }
        }
        self.failed = errors;
    }

    /// Advances one bridge; `true` if its status changed.
    fn service(&mut self, name: &str) -> bool {
        let Some(mut inst) = self.bridges.remove(name) else {
            return false;
        };
        let before = inst.state();
        let now = Instant::now();
        let mut changed = false;
        let mut reopen_why = None;
        match &mut inst.phase {
            Phase::Waiting { until } if now >= *until => {
                self.open(&mut inst);
            }
            Phase::Running(run) => {
                run.handle.reap();
                if run.rt.is_finished() {
                    reopen_why = Some(String::new());
                } else if now.duration_since(run.last_check) >= Duration::from_secs(1) {
                    run.last_check = now;
                    let t = inst.stats.as_ref().map_or(0, |s| {
                        let t = stat_of(s, inst.plan.direction == BridgeDirection::Playback);
                        t.xruns + t.card_errors
                    });
                    if t > run.last_trouble {
                        run.trouble += 1;
                    } else {
                        run.trouble = 0;
                    }
                    run.last_trouble = t;
                    if run.trouble >= TROUBLE_WINDOWS {
                        reopen_why = Some(format!(
                            "the card had xruns or errors in each of {TROUBLE_WINDOWS} consecutive seconds"
                        ));
                    }
                }
            }
            _ => {}
        }
        if let Some(why) = reopen_why {
            self.close(&mut inst, Some(why));
        } else {
            match inst.retune() {
                Retune::Nothing => {}
                Retune::Changed => changed = true,
                // The bridge (its counters, back-off and generation history
                // stay with the instance) is reopened at once.
                Retune::Reopen(m) => {
                    tracing::info!(
                        "bridge `{}`: {m}; reopening with the new tuning",
                        log_safe(&inst.plan.name, 64)
                    );
                    // `close` schedules a reopen with the back-off; this one
                    // is not a failure: reopen on the next pass (100 ms), with
                    // the back-off at its start. It is not an open failure;
                    // the successful open counts in `reopens` and changes
                    // `generation`, like any start of the card.
                    self.close(&mut inst, Some(format!("retune: {m}")));
                    inst.last_error = None;
                    inst.failures = 0;
                    inst.phase = Phase::Waiting {
                        until: Instant::now(),
                    };
                    changed = true;
                }
            }
        }
        changed |= inst.state() != before;
        self.bridges.insert(name.to_owned(), inst);
        changed
    }

    fn device_info(p: &BridgePlan) -> DeviceInfo {
        let span = (p.first_channel - 1).saturating_add(p.channels);
        DeviceInfo {
            key: p.device.clone(),
            rx_channels: if p.direction == BridgeDirection::Playback {
                span
            } else {
                0
            },
            tx_channels: if p.direction == BridgeDirection::Capture {
                span
            } else {
                0
            },
            sample_rate: p.rate,
            latency_us: p.latency_us,
            latency_max_us: p.latency_max_us,
        }
    }

    fn open(&mut self, inst: &mut Instance) {
        match self.try_open(inst) {
            Ok(run) => {
                inst.opened += 1;
                inst.generation = self.next_generation;
                self.next_generation += 1;
                inst.applied = (inst.plan.time_constant_s, inst.plan.max_correction_ppm);
                tracing::info!(
                    "bridge `{}`: running{} on {}",
                    log_safe(&inst.plan.name, 64),
                    if inst.opened > 1 || inst.last_error.is_some() {
                        " again"
                    } else {
                        ""
                    },
                    log_safe(&inst.plan.pcm, 128)
                );
                inst.phase = Phase::Running(run);
            }
            Err(e) => {
                inst.open_failures += 1;
                let fatal = e.fatal();
                let msg = e.to_string();
                if inst.last_error.as_deref() != Some(msg.as_str()) {
                    tracing::warn!(
                        "bridge `{}`: {}{}",
                        log_safe(&inst.plan.name, 64),
                        log_safe(&msg, 300),
                        if fatal {
                            "; not retried until the configuration changes"
                        } else {
                            "; retrying"
                        }
                    );
                }
                inst.last_error = Some(msg);
                inst.phase = if fatal {
                    Phase::Failed
                } else {
                    inst.failures = inst.failures.saturating_add(1);
                    Phase::Waiting {
                        until: Instant::now() + inst.plan.reopen_wait(inst.failures),
                    }
                };
            }
        }
    }

    /// Opens everything.
    fn try_open(&mut self, inst: &mut Instance) -> Result<Box<Running>, ManagerError> {
        let p = inst.plan.clone();
        let playback = p.direction == BridgeDirection::Playback;
        let periods = (p.buffer_frames.div_ceil(p.period_frames.max(1))).max(2) as usize;
        let params = CardParams {
            period_frames: p.period_frames as usize,
            periods,
            ..CardParams::new(&p.pcm, p.rate, u32::from(p.channels))
        };
        let card = AlsaCard::open(&params, playback).map_err(ManagerError::Open)?;
        let (period, buffer) = (card.period_frames(), card.buffer_frames());
        let cfg = LibConfig {
            quality: p.media_quality(),
            loop_bandwidth_hz: p.loop_bandwidth_hz(),
            max_correction: p.max_correction(),
            prefill_periods: (buffer / period.max(1)).saturating_sub(1).max(1),
            ..LibConfig::default()
        };
        let card: Box<dyn CardPcm> = Box::new(card);
        let (pb, cap) = if playback {
            (Some(card), None)
        } else {
            (None, Some(card))
        };
        let local = Box::new(LocalClock::new(p.rate));
        let (mut bridge, mut handle) = Bridge::new(pb, cap, &cfg, local)?;

        let ahead = handle.rx_guard_frames().max(handle.tx_lead_frames());
        let added = ahead;
        // Manager thread (normal priority), at open: never on the bridge
        // thread (the shared options sit behind a lock).
        let audio = self.deps.audio.get();
        let (ring_params, latency) = bridge_ring(&p, buffer as u64, period as u64, ahead, &audio)?;
        let ring = Ring::create(&ring_params, &audio.limits)
            .map_err(|e| ManagerError::Ring(format!("cannot create it: {e}")))?;
        ring.set_effective_latency(latency)
            .map_err(|e| ManagerError::Ring(format!("latency: {e}")))?;
        let ring_err = |e: tessera_audio_ipc::Error| ManagerError::Ring(e.to_string());
        let end = if playback {
            let consumer = ring.consumer().map_err(ring_err)?;
            handle
                .attach_rx(RxLink { consumer })
                .map_err(ManagerError::Handoff)?;
            SessionEnd::Producer(ring.producer().map_err(ring_err)?)
        } else {
            let producer = ring.producer().map_err(ring_err)?;
            handle
                .attach_tx(TxLink { producer })
                .map_err(ManagerError::Handoff)?;
            SessionEnd::Consumer(ring.consumer().map_err(ring_err)?)
        };
        let reader = self.deps.clock.reader().map_err(ManagerError::Clock)?;
        handle
            .set_clock(Some(Box::new(OverlayClock::new(Arc::new(reader), p.rate))))
            .map_err(ManagerError::Handoff)?;
        let session = self.next_session.fetch_add(1, Relaxed);
        let spec = SessionSpec {
            id: session,
            device: p.device.clone(),
            first_channel: u32::from(p.first_channel),
            channel_count: u32::from(p.channels),
            format: SampleFormat::I32,
            end,
            wake: None,
        };
        if !self.deps.media.add_session(spec) {
            return Err(ManagerError::MediaBusy);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let health = Arc::new(Health::default());
        health.error_sleep.set(error_sleep_of(&p));
        let setup = ThreadSetup {
            rt_priority: p.rt_priority,
            cpu: p.cpu,
            #[cfg(feature = "bridge-test-hooks")]
            panic_after: (std::env::var("TESSERA_TEST_BRIDGE_PANIC").ok().as_deref()
                == Some(p.name.as_str()))
            .then_some(50),
        };
        let (s2, h2) = (Arc::clone(&stop), Arc::clone(&health));
        let rt = std::thread::Builder::new()
            .name("tessera-bridge".into())
            .spawn(move || {
                // A panic (a bug) ends only this bridge; the manager marks it
                // failed.
                let end = rt_loop::thread_body(&mut bridge, &s2, &h2, setup);
                (bridge, end)
            });
        let rt = match rt {
            Ok(j) => j,
            Err(e) => {
                self.deps.media.remove_session(session);
                return Err(ManagerError::Thread(e));
            }
        };
        inst.stats = Some(Arc::clone(handle.stats()));
        let now = Instant::now();
        Ok(Box::new(Running {
            handle,
            rt,
            stop,
            health,
            session,
            added_latency: added,
            _ring: ring,
            since: now,
            last_check: now,
            last_trouble: 0,
            trouble: 0,
        }))
    }

    /// Stops a running bridge (if it runs) and schedules the reopen when
    /// `why` is given (`Some("")`: the loop ended by itself).
    fn close(&mut self, inst: &mut Instance, why: Option<String>) {
        let phase = std::mem::replace(&mut inst.phase, Phase::Failed);
        let Phase::Running(run) = phase else {
            inst.phase = phase;
            return;
        };
        let run = *run;
        run.stop.store(true, Relaxed);
        let ended = run.rt.join();
        self.deps.media.remove_session(run.session);
        let mut handle = run.handle;
        handle.reap();
        inst.rt_degraded |= run.health.rt_degraded.load(Relaxed);
        if let Some(s) = &inst.stats {
            inst.totals = inst
                .totals
                .plus(stat_of(s, inst.plan.direction == BridgeDirection::Playback));
        }
        let (loop_err, panicked) = match ended {
            Ok((bridge, end)) => {
                drop(bridge);
                match end {
                    LoopEnd::Stopped => (None, false),
                    LoopEnd::Card(e) => (Some(e.to_string()), false),
                    LoopEnd::Panicked(p) => (
                        Some(format!("the bridge thread panicked: {}", panic_text(&*p))),
                        true,
                    ),
                }
            }
            // Only if the catch above failed: the thread is gone all the same.
            Err(p) => (
                Some(format!("the bridge thread panicked: {}", panic_text(&*p))),
                true,
            ),
        };
        let name = log_safe(&inst.plan.name, 64);
        if panicked {
            let msg = loop_err.unwrap_or_default();
            tracing::error!(
                "bridge `{name}`: {}; failed until the configuration changes",
                log_safe(&msg, 300)
            );
            inst.last_error = Some(msg);
            inst.phase = Phase::Failed;
            return;
        }
        let Some(why) = why else {
            return;
        };
        let msg = match (why.is_empty(), loop_err) {
            (false, _) => why,
            (true, Some(e)) => e,
            (true, None) => "the bridge loop stopped".into(),
        };
        if run.since.elapsed() >= STABLE {
            inst.failures = 0;
        }
        inst.failures = inst.failures.saturating_add(1);
        let wait = inst.plan.reopen_wait(inst.failures);
        tracing::warn!(
            "bridge `{name}`: {}; reopening in {wait:?}",
            log_safe(&msg, 300),
        );
        inst.last_error = Some(msg);
        inst.phase = Phase::Waiting {
            until: Instant::now() + wait,
        };
    }
}

// tessera-state validates against copies of the library's limits (it cannot
// depend on the library, which links libasound): a build with the bridge
// refuses to compile if they drift apart.
// The bridge thread's default pause is the schema's default, and the
// schema's floor keeps it from ever being zero.
const _: () = {
    use tessera_state::config::{
        DEFAULT_BRIDGE_ERROR_SLEEP_MAX_MS, DEFAULT_BRIDGE_ERROR_SLEEP_MIN_MS,
        MIN_BRIDGE_ERROR_SLEEP_MS,
    };
    assert!(ErrorSleep::DEFAULT.doubling);
    assert!(ErrorSleep::DEFAULT.min_ms == DEFAULT_BRIDGE_ERROR_SLEEP_MIN_MS);
    assert!(ErrorSleep::DEFAULT.max_ms == DEFAULT_BRIDGE_ERROR_SLEEP_MAX_MS);
    assert!(MIN_BRIDGE_ERROR_SLEEP_MS >= 1);
};

const _: () = {
    use tessera_backend_alsa::bridge::{ERROR_FILTER_TAU, MAX_CORRECTION};
    use tessera_state::config::{BRIDGE_ERROR_FILTER_TAU_S, MAX_BRIDGE_CORRECTION_PPM};
    let d = MAX_CORRECTION * 1e6 - MAX_BRIDGE_CORRECTION_PPM;
    assert!(
        d < 1e-6 && d > -1e-6,
        "MAX_BRIDGE_CORRECTION_PPM != MAX_CORRECTION"
    );
    let d = ERROR_FILTER_TAU - BRIDGE_ERROR_FILTER_TAU_S;
    assert!(
        d < 1e-12 && d > -1e-12,
        "BRIDGE_ERROR_FILTER_TAU_S != ERROR_FILTER_TAU"
    );
    let d = tessera_backend_alsa::bridge::MAX_TRANSIENT_S
        - tessera_state::config::MAX_BRIDGE_TRANSIENT_S;
    assert!(
        d < 1e-12 && d > -1e-12,
        "MAX_BRIDGE_TRANSIENT_S != MAX_TRANSIENT_S"
    );
};

#[cfg(test)]
mod tests {
    use tessera_backend_alsa::bridge::{ERROR_FILTER_TAU, MAX_CORRECTION, transient_frames};
    use tessera_state::config::{BRIDGE_ERROR_FILTER_TAU_S, MAX_BRIDGE_CORRECTION_PPM};

    /// Card-open and hand-off errors are retried, whatever the library's
    /// variant; a configuration the bridge cannot build is not.
    #[test]
    fn open_and_handoff_errors_are_not_fatal() {
        use super::ManagerError;
        use tessera_backend_alsa::bridge::BridgeError;
        for e in [
            ManagerError::Open(BridgeError::Config("x".into())),
            ManagerError::Open(BridgeError::Open("busy".into())),
            ManagerError::Handoff(BridgeError::Busy),
            ManagerError::Handoff(BridgeError::Config("x".into())),
            ManagerError::Ring("x".into()),
            ManagerError::MediaBusy,
        ] {
            assert!(!e.fatal(), "{e}");
        }
        assert!(ManagerError::Bridge(BridgeError::Config("x".into())).fatal());
        assert!(ManagerError::RingTooLarge("x".into()).fatal());
    }

    fn plan(rate: u32, tau: f64, ppm: f64) -> super::BridgePlan {
        super::BridgePlan {
            name: "b".into(),
            device: "main".into(),
            pcm: "hw:0".into(),
            direction: tessera_state::config::BridgeDirection::Playback,
            first_channel: 1,
            channels: 2,
            rate,
            period_frames: rate / 100,
            buffer_frames: rate / 25,
            quality: tessera_state::config::Resampler::Cubic,
            latency_us: 4_000,
            latency_max_us: 40_000,
            rt_priority: None,
            cpu: None,
            time_constant_s: tau,
            max_correction_ppm: ppm,
            backoff: tessera_state::config::BridgeConfig::new(
                "main",
                "hw:0",
                tessera_state::config::BridgeDirection::Playback,
            )
            .backoff(&tessera_state::config::BridgeDefaults::default()),
            error_sleep: tessera_state::config::BridgeConfig::new(
                "main",
                "hw:0",
                tessera_state::config::BridgeDirection::Playback,
            )
            .error_sleep(&tessera_state::config::BridgeDefaults::default()),
        }
    }

    /// A reloaded `error_backoff*` / `error_sleep*` for the same card is
    /// stored (no reopen), so the next failure waits by the new values.
    #[test]
    fn a_same_card_plan_change_is_stored_for_the_next_error() {
        let a = plan(48_000, 120.0, 200.0);
        let mut inst = super::Instance::new(a.clone());
        let mut b = a.clone();
        b.backoff.min_ms = 40;
        b.backoff.doubling = false;
        b.error_sleep.min_ms = 5;
        assert!(a.same_card(&b));
        inst.update_plan(b.clone());
        assert_eq!(inst.plan, b);
        assert_eq!(
            inst.plan.reopen_wait(3),
            std::time::Duration::from_millis(40)
        );
        assert_eq!(super::error_sleep_of(&inst.plan).min_ms, 5);
        // What update_plan hands a running bridge thread: its next error
        // pauses by the reloaded values.
        let health = super::Health::default();
        super::push_error_sleep(&health, &inst.plan);
        assert_eq!(health.error_sleep.get().min_ms, 5);
        assert_eq!(
            health.error_sleep.get().wait(1),
            std::time::Duration::from_millis(5)
        );
    }

    /// The ring tesserad builds holds the card buffer, one more period and
    /// the guard the library computed, at the default tuning and at the
    /// slowest loop the validation allows (a 60 s transient: tau 49 999 s at
    /// 3000 ppm), where it is still within the default ring-size limit at
    /// 48 kHz and two channels; at 96 kHz it is not, and that is a clear,
    /// fatal error rather than a bridge that reopens forever.
    #[test]
    fn the_ring_covers_buffer_period_and_guard() {
        use crate::audio::AudioConfig;
        let audio = AudioConfig::default();
        for (rate, tau, ppm) in [
            (48_000, 120.0, 200.0),
            (96_000, 120.0, 200.0),
            (48_000, 49_999.0, 3000.0),
        ] {
            let p = plan(rate, tau, ppm);
            let (period, buffer) = (u64::from(p.period_frames), u64::from(p.buffer_frames));
            let t = super::super::plan::transient(rate, tau, ppm);
            // The library's playback guard for this tuning.
            let guard = (2 * period + 64).max(t + period + 64);
            let (params, latency) =
                super::bridge_ring(&p, buffer, period, guard, &audio).expect("ring");
            assert!(
                params.capacity_frames >= latency + buffer + period + guard,
                "{rate} Hz, tau {tau}: capacity {} for latency {latency} + buffer {buffer} + period {period} + guard {guard}",
                params.capacity_frames
            );
        }
        let p = plan(96_000, 49_999.0, 3000.0);
        let t = super::super::plan::transient(96_000, 49_999.0, 3000.0);
        match super::bridge_ring(&p, 3840, 960, t + 1024, &audio) {
            Err(e @ super::ManagerError::RingTooLarge(_)) => assert!(e.fatal()),
            other => panic!("{other:?}"),
        }
    }

    /// The manager's headroom check and the library's guard use one formula.
    #[test]
    fn the_transient_is_the_library_transient() {
        for (rate, tau, ppm) in [
            (48_000, 120.0, 200.0),
            (96_000, 120.0, 200.0),
            (44_100, 7.5, 0.5),
        ] {
            let bw = 1.0 / (2.0 * std::f64::consts::PI * tau);
            let lib = transient_frames(rate, bw, ppm * 1e-6);
            let ours = super::super::plan::transient(rate, tau, ppm);
            assert!(
                lib.abs_diff(ours) <= 1,
                "{rate} {tau} {ppm}: {lib} vs {ours}"
            );
        }
        // The number the review quoted: 200 ppm, 96 kHz, 120 s.
        assert_eq!(super::super::plan::transient(96_000, 120.0, 200.0), 922);
    }

    /// tessera-state validates against copies of the library's limits (it
    /// cannot depend on the library, which links libasound).
    #[test]
    fn the_validated_limits_are_the_library_limits() {
        assert!((MAX_CORRECTION * 1e6 - MAX_BRIDGE_CORRECTION_PPM).abs() < 1e-6);
        assert!((ERROR_FILTER_TAU - BRIDGE_ERROR_FILTER_TAU_S).abs() < 1e-12);
    }
}
