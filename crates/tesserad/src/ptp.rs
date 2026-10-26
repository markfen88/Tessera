//! The clock domain: PTP runners and the media-clock overlay (ADR 0004,
//! ADR 0005).
//!
//! One [`tessera_clock`] runner thread per configured interface (the RT
//! thread: `SCHED_FIFO` from `[engine] ptp_rt_priority`, sockets, servo). A
//! small non-real-time **manager thread** (this module) supervises them:
//!
//! * the **primary** interface (the first one) publishes the media clock into
//!   the process-wide *clock overlay*, one sealed memfd that every audio
//!   client and the media engine map read-only. Other interfaces run their own
//!   PTP port (status, later redundancy) and publish into private overlays;
//! * a change of role, priority or timestamping mode from a command or a
//!   reload is applied **live**: it is sent to the running runner as a command
//!   (`RunnerHandle::send`, lock-free) and re-runs the election. It never
//!   steps media time and never changes the epoch (the epoch changes only if
//!   the servo later has to step). The runner is **not** restarted. The
//!   `[clock] priority` is advertised as stratum, preferred flag and variance
//!   ([`priority_clock`]);
//! * only a change of the interface restarts a runner (`rt_priority` and `cpu`
//!   are applied when a runner starts, never live). The restart hands the
//!   overlay writer over (a [`SlotPublisher`] gives it back when the runner
//!   ends), starts the new port at the old mapping's time and rate (anchored
//!   at the instant the daemon read the mapping, so the thread start-up does
//!   not step media time), publishes `holdover` while the new port catches up
//!   so TX keeps flowing, and still bumps the epoch so that clients re-anchor
//!   once;
//! * if a runner cannot start (an interface without an address, no privilege
//!   for the PTP ports, `lo`), the domain **degrades loudly, never silently**:
//!   a free-running overlay keeps being published (state `free_running`, the
//!   last mapping and rate), the failure is logged and kept in the metrics,
//!   and the start is retried every few seconds;
//! * loopback-only configurations (`interfaces = ["lo"]`) have no media
//!   network and never start PTP.
//!
//! Nothing here steers any kernel clock (project rule 8): `tessera-clock`
//! only reads `CLOCK_REALTIME`/`CLOCK_TAI`.

use std::fmt;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tessera_audio_ipc::{
    ClockOverlayReader, ClockOverlayWriter, ClockSnapshot, ClockState as IpcState, raw_monotonic_ns,
};
use tessera_clock::config::{LocalClock, PortConfig, Role};
use tessera_clock::media_clock::{ClockPublisher, MediaClockSnapshot};
use tessera_clock::platform::publisher::to_overlay;
use tessera_clock::platform::runner::RunnerEventKind;
use tessera_clock::platform::{
    CommandError, OverlaySnapshot, RunnerCommand, RunnerConfig, RunnerHandle, TimestampMode, tai_ns,
};
use tessera_control_types::{ClockRole, ClockState, ClockStatus, Timestamping};
use tessera_state::config::Timestamping as TsMode;

use crate::engine_thread::EngineHandle;
use crate::netif;

/// How the clock domain is configured (derived from `[clock]`, `[network]`
/// and `[engine]` by the runtime reconciler).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockPlan {
    /// Interfaces, by name; the first is the primary (publishes the media
    /// clock). Empty: no media network.
    pub interfaces: Vec<String>,
    /// Role.
    pub role: ClockRole,
    /// Leader-election priority (lower wins). PROVISIONAL mapping, see
    /// [`priority_clock`].
    pub priority: u8,
    /// `[clock] timestamping`.
    pub timestamping: TsMode,
    /// `SCHED_FIFO` priority of the runner threads (`None`: leave alone).
    pub rt_priority: Option<u8>,
    /// CPU to pin the runner threads to.
    pub cpu: Option<usize>,
    /// Allow Wi-Fi interfaces.
    pub allow_wireless: bool,
    /// False: never start runners (`--no-ptp`); the overlay stays
    /// free-running.
    pub enabled: bool,
}

impl ClockPlan {
    /// A plan with no interfaces and PTP disabled (a free-running clock).
    #[must_use]
    pub fn free_running() -> Self {
        Self {
            interfaces: Vec::new(),
            role: ClockRole::Follower,
            priority: 128,
            timestamping: TsMode::Auto,
            rt_priority: None,
            cpu: None,
            allow_wireless: false,
            enabled: false,
        }
    }
}

/// PROVISIONAL, tessera-only (confidence: guess; source: OQ-CLK-1, PTPv1 has
/// no priority field): what `[clock] priority` (lower wins, default 128)
/// makes a leader-capable port advertise. The PTPv1 comparison goes preferred
/// flag, stratum, identifier, variance, and a lower priority never loses on
/// any of them:
///
/// * `preferred` for priority below 64 (a Dante-style "preferred leader"),
/// * stratum 4 (the leader-capable default) up to priority 128, then one
///   step worse for every further 32 (255 gives 7), so a node that is told
///   to yield does so against default-priority nodes and hardware alike,
/// * variance scaled around the reference default ([`priority_variance`]),
///   the fine ordering between tessera nodes.
///
/// The default priority therefore advertises exactly what a leader-capable
/// port advertises with no configuration (stratum 4, not preferred,
/// variance -4000). Never stratum 1 to 3, which could outrank hardware.
#[must_use]
pub fn priority_clock(priority: u8) -> LocalClock {
    let yield_steps = priority.saturating_sub(128) / 32;
    LocalClock {
        preferred: priority < 64,
        stratum: LocalClock::leader_capable().stratum + yield_steps,
        variance: priority_variance(priority),
        ..LocalClock::leader_capable()
    }
}

/// PROVISIONAL, tessera-only: how `[clock] priority` (lower wins) maps onto
/// the PTPv1 comparison. PTPv1 has no priority field (OQ-CLK-1); the best-
/// master comparison ends with the *variance*, lower wins, so priority is
/// scaled into it around the reference default (-4000). Between two tessera
/// nodes with the same role this makes the lower priority win; against real
/// hardware the earlier criteria (preferred, stratum, identifier) decide.
#[must_use]
pub fn priority_variance(priority: u8) -> i16 {
    const BASE: i32 = -4000;
    const STEP: i32 = 16;
    let v = BASE + (i32::from(priority) - 128) * STEP;
    i16::try_from(v).unwrap_or(0)
}

/// Capacity of the manager's request queue.
const QUEUE: usize = 64;
/// How often the manager looks at the runners.
const POLL: Duration = Duration::from_millis(100);
/// Retry period of a runner that failed to start.
const RETRY: Duration = Duration::from_secs(5);
/// How often a running port checks that its interface is still the one it
/// opened its sockets on, and how often a port whose interface went away
/// looks for it again.
const IFACE_CHECK: Duration = Duration::from_secs(1);
/// How long a clock that was usable stays in holdover while its interface
/// is missing (the port's own holdover default), before it is free running.
const HOLDOVER_WITHOUT_IFACE: Duration = Duration::from_secs(60);
/// Republish period of the free-running overlay (liveness).
const FALLBACK_PERIOD: Duration = Duration::from_millis(250);
/// A status row is re-sent at least this often while its numbers move.
const REPORT_EVERY: Duration = Duration::from_secs(2);

enum Msg {
    Apply(Box<ClockPlan>),
    Stop,
}

/// What a finished runner (or a fresh domain) leaves behind.
struct Handoff {
    writer: ClockOverlayWriter,
    /// The last snapshot published (`None` before the first).
    last: Option<ClockSnapshot>,
    /// Nothing but the initial free-running snapshot was ever published.
    fresh: bool,
}

type Slot = Mutex<Option<Handoff>>;

/// A [`ClockPublisher`] that writes into the overlay writer it owns and gives
/// the writer back (through the slot) when it is dropped, so the memfd the
/// clients mapped survives runner restarts. The published epoch is offset by
/// `epoch_base` so it continues across restarts.
struct SlotPublisher {
    writer: Option<ClockOverlayWriter>,
    last: Option<ClockSnapshot>,
    epoch_base: u64,
    slot: Arc<Slot>,
    /// Nothing but the initial free-running snapshot was ever published, by
    /// this runner or before it.
    fresh: bool,
    /// Set while a runner that is being replaced shuts down: its final
    /// `free_running` snapshot is published as `holdover`, so the engine does
    /// not see the clock drop out for the restart.
    hold_on_stop: Arc<AtomicBool>,
    /// After a restart that continued a usable clock: until this local time
    /// (or until the port reaches `locked`/`leader`, or steps), the states the
    /// new port passes through on its way (free running, listening,
    /// acquiring) are published as `holdover` with the old grandmaster, so
    /// that TX does not stop for the restart.
    hold_until: Option<(u64, u64)>,
}

/// The clock states in which the engine transmits.
fn usable_state(s: IpcState) -> bool {
    matches!(s, IpcState::Locked | IpcState::Leader | IpcState::Holdover)
}

/// How long a restarted port may be shown as `holdover` while it catches up
/// (the port's own holdover limit is 60 s).
const HOLD_RESTART_NS: u64 = 30_000_000_000;

/// How a new runner continues the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Continuation {
    /// Added to the new port's epoch.
    epoch_base: u64,
    /// The old mapping to stay continuous with, if any.
    old: Option<ClockSnapshot>,
}

/// Decide how a runner starting now continues the overlay whose last
/// snapshot is `last` (`fresh`: nothing but the initial free-running
/// snapshot was ever published). A first start of a leader moves the overlay
/// from the monotonic timeline to a TAI reading: media time jumps, so the
/// epoch must change. Any other first start (a follower) stays on the
/// monotonic timeline. A restart (an interface change) starts the new port
/// at the old mapping's time and rate, anchored where the mapping was read
/// (`PortConfig::initial_local_ns`); the epoch changes all the same, so that
/// clients re-anchor once.
fn continuation(last: Option<ClockSnapshot>, fresh: bool, leads: bool) -> Continuation {
    let epoch = last.map_or(0, |s| s.epoch);
    if fresh {
        Continuation {
            epoch_base: if leads { epoch.wrapping_add(1) } else { epoch },
            old: None,
        }
    } else {
        // The new port is created on its own thread a little after the old
        // mapping was sampled; it is anchored at the sampling instant, so
        // media time does not step by the start-up time (hundreds of
        // microseconds, milliseconds on a busy host), which the followers'
        // servos would take tens of seconds to work off. The epoch changes
        // all the same: clients re-anchor once. Only an interface change
        // restarts a runner.
        Continuation {
            epoch_base: epoch.wrapping_add(1),
            old: last,
        }
    }
}

impl fmt::Debug for SlotPublisher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SlotPublisher")
    }
}

impl ClockPublisher for SlotPublisher {
    fn publish(&mut self, snap: &MediaClockSnapshot) {
        let mut s = to_overlay(snap);
        s.epoch = s.epoch.wrapping_add(self.epoch_base);
        self.fresh = false;
        if self.hold_on_stop.load(Ordering::Relaxed) && s.state == IpcState::FreeRunning {
            s.state = IpcState::Holdover;
        }
        if let Some((until, gm)) = self.hold_until {
            let catching_up = matches!(
                s.state,
                IpcState::FreeRunning | IpcState::Listening | IpcState::Acquiring
            );
            if snap.epoch != 0 || usable_state(s.state) || s.updated_mono_ns > until {
                self.hold_until = None;
            } else if catching_up {
                s.state = IpcState::Holdover;
                if s.gm_identity == 0 {
                    s.gm_identity = gm;
                }
            }
        }
        if let Some(w) = self.writer.as_mut() {
            w.publish(&s);
        }
        self.last = Some(s);
    }
}

impl Drop for SlotPublisher {
    fn drop(&mut self) {
        // Once, when the runner ends: not on any steady-state path.
        if let Some(writer) = self.writer.take() {
            let mut g = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
            *g = Some(Handoff {
                writer,
                last: self.last,
                fresh: self.fresh,
            });
        }
    }
}

/// What a running port is set up with, as far as live commands can change it.
/// `rt_priority` and `cpu` are not part of it: they are applied when a runner
/// starts and not live (`docs/configuration.md`), and nothing else a plan
/// carries needs a restart except the interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Applied {
    role: ClockRole,
    /// What the port advertises. A follower advertises the slave-only
    /// values whatever the priority, so a priority change on a follower is no
    /// change at all.
    local: LocalClock,
    timestamping: TsMode,
}

impl Applied {
    fn of(plan: &ClockPlan) -> Self {
        Self {
            role: plan.role,
            local: if plan.role == ClockRole::Follower {
                LocalClock::follower()
            } else {
                priority_clock(plan.priority)
            },
            timestamping: plan.timestamping,
        }
    }
}

/// One live change to a running runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Clock(LocalClock),
    Role(ClockRole),
    Timestamping(TsMode),
}

impl Step {
    fn command(self) -> RunnerCommand {
        match self {
            Step::Clock(l) => RunnerCommand::SetLocalClock(l),
            Step::Role(r) => RunnerCommand::SetRole(match r {
                ClockRole::Leader => Role::Leader,
                ClockRole::Auto => Role::Auto,
                _ => Role::Follower,
            }),
            Step::Timestamping(t) => RunnerCommand::SetTimestamping(match t {
                TsMode::Auto => TimestampMode::Auto,
                TsMode::Hardware => TimestampMode::Hardware,
                TsMode::Software => TimestampMode::Software,
            }),
        }
    }

    fn record(self, a: &mut Applied) {
        match self {
            Step::Clock(l) => a.local = l,
            Step::Role(r) => a.role = r,
            Step::Timestamping(t) => a.timestamping = t,
        }
    }
}

/// The commands that take a running port from `have` to `want`, in the order
/// they must be sent. A port started as a follower advertises stratum 255
/// (slave-only), so promoting it needs the leader-capable clock *before* the
/// role; demoting it stops the leader first and then drops the clock.
fn live_steps(have: &Applied, want: &Applied) -> Vec<Step> {
    let mut steps = Vec::new();
    let role = (have.role != want.role).then_some(Step::Role(want.role));
    let clock = (have.local != want.local).then_some(Step::Clock(want.local));
    if have.role == ClockRole::Follower && want.role != ClockRole::Follower {
        steps.extend(clock);
        steps.extend(role);
    } else {
        steps.extend(role);
        steps.extend(clock);
    }
    if have.timestamping != want.timestamping {
        steps.push(Step::Timestamping(want.timestamping));
    }
    steps
}

struct Domain {
    iface: String,
    primary: bool,
    slot: Arc<Slot>,
    reader: ClockOverlayReader,
    runner: Option<RunnerHandle>,
    /// What the running port has (`None`: no runner).
    applied: Option<Applied>,
    failed_with: Option<Applied>,
    next_attempt: Instant,
    last_error: Option<String>,
    last_fallback: Instant,
    reported: Option<ClockStatus>,
    reported_at: Instant,
    rt_warned: bool,
    /// Runners started since the daemon began.
    starts: u64,
    /// Live commands (applied, rejected) of runners that have ended.
    cmd_base: (u64, u64),
    /// Rejections of the current runner already reported.
    rejected_seen: u64,
    /// Shared with the publisher: the runner is being replaced.
    hold_on_stop: Arc<AtomicBool>,
    /// The interface as the running port found it at start: its sockets
    /// belong to this index and address.
    started_on: Option<netif::IfInfo>,
    last_iface_check: Instant,
    /// When the port was stopped because its interface went away or changed:
    /// look for it every [`IFACE_CHECK`], not every [`RETRY`], and keep a
    /// usable clock in holdover for [`HOLDOVER_WITHOUT_IFACE`].
    iface_lost: Option<Instant>,
}

fn initial_snapshot(now: u64) -> ClockSnapshot {
    ClockSnapshot {
        epoch: 0,
        base_mono_ns: now,
        base_media_ns: now,
        rate_ppb: 0,
        state: IpcState::FreeRunning,
        gm_identity: 0,
        updated_mono_ns: now,
    }
}

impl Domain {
    fn new(iface: String, primary: bool) -> std::io::Result<(Self, OwnedFd)> {
        let mut writer = ClockOverlayWriter::create().map_err(std::io::Error::other)?;
        let export = writer.try_clone_fd().map_err(std::io::Error::other)?;
        let reader =
            ClockOverlayReader::attach(writer.try_clone_fd().map_err(std::io::Error::other)?)
                .map_err(std::io::Error::other)?;
        let first = initial_snapshot(raw_monotonic_ns());
        writer.publish(&first);
        let slot = Arc::new(Mutex::new(Some(Handoff {
            writer,
            last: Some(first),
            fresh: true,
        })));
        let d = Self {
            iface,
            primary,
            slot,
            reader,
            runner: None,
            applied: None,
            failed_with: None,
            next_attempt: Instant::now(),
            last_error: None,
            last_fallback: Instant::now(),
            reported: None,
            reported_at: Instant::now(),
            rt_warned: false,
            starts: 0,
            cmd_base: (0, 0),
            rejected_seen: 0,
            hold_on_stop: Arc::new(AtomicBool::new(false)),
            started_on: None,
            last_iface_check: Instant::now(),
            iface_lost: None,
        };
        Ok((d, export))
    }

    /// Stop the runner (a graceful stop: it publishes a final `free_running`
    /// state with the mapping and epoch unchanged). `for_restart`: the runner is
    /// being replaced, so a usable clock is published as `holdover` instead
    /// and TX keeps flowing.
    fn stop_runner(&mut self, for_restart: bool) {
        let Some(r) = self.runner.take() else {
            self.applied = None;
            return;
        };
        let usable = OverlaySnapshot::read(&self.reader).is_ok_and(|s| {
            use tessera_clock::media_clock::ClockState as C;
            matches!(s.state, C::Locked | C::Leader | C::Holdover)
        });
        self.hold_on_stop
            .store(for_restart && usable, Ordering::Relaxed);
        let st = r.status();
        self.cmd_base.0 += st.commands_applied.load(Ordering::Relaxed);
        self.cmd_base.1 += st.commands_rejected.load(Ordering::Relaxed);
        let _ = r.stop();
        self.hold_on_stop.store(false, Ordering::Relaxed);
        self.applied = None;
        if for_restart {
            self.hold_overlay();
        }
    }
    /// Build the port configuration, continuing the old mapping.
    fn port_config(plan: &ClockPlan, info: &netif::IfInfo, h: &Handoff, now: u64) -> PortConfig {
        let uuid = info.mac;
        let mut pc = match plan.role {
            ClockRole::Leader => PortConfig::leader(uuid),
            ClockRole::Auto => PortConfig::auto(uuid),
            _ => PortConfig::follower(uuid),
        };
        let leads = !matches!(plan.role, ClockRole::Follower);
        pc.local = Applied::of(plan).local;
        match h.last {
            Some(s) if !h.fresh => {
                let m = OverlaySnapshot::from_ipc(s);
                pc.initial_media_ns = Some(m.media_ns_at(now));
                // The runner builds the port on its own thread later; it
                // anchors the mapping at `now`, where it was read.
                pc.initial_local_ns = Some(now);
                pc.initial_rate_ppb = s.rate_ppb;
            }
            _ => {
                // First start: a leader starts at a plausible TAI time (read,
                // never set); a follower starts on the monotonic timeline.
                if leads {
                    pc.initial_media_ns = Some(tai_ns());
                }
            }
        }
        pc
    }

    fn start(&mut self, plan: &ClockPlan) -> Result<(), String> {
        let info = netif::lookup(&self.iface).map_err(|e| e.to_string())?;
        if info.wireless && !plan.allow_wireless {
            return Err(format!(
                "`{}` is a wireless interface (set [network] allow_wireless to use it)",
                self.iface
            ));
        }
        let handoff = self
            .slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .ok_or_else(|| "the overlay writer is not available".to_owned())?;
        let now = raw_monotonic_ns();
        let pc = Self::port_config(plan, &info, &handoff, now);
        let leads = !matches!(plan.role, ClockRole::Follower);
        let cont = continuation(handoff.last, handoff.fresh, leads);
        let publisher = SlotPublisher {
            writer: Some(handoff.writer),
            last: handoff.last,
            epoch_base: cont.epoch_base,
            slot: Arc::clone(&self.slot),
            fresh: handoff.fresh,
            hold_on_stop: Arc::clone(&self.hold_on_stop),
            hold_until: cont
                .old
                .filter(|s| usable_state(s.state))
                .map(|s| (now + HOLD_RESTART_NS, s.gm_identity)),
        };
        let mut rc = RunnerConfig::new(&info.name, info.addr, pc);
        rc.timestamping = match plan.timestamping {
            TsMode::Auto => TimestampMode::Auto,
            TsMode::Hardware => TimestampMode::Hardware,
            TsMode::Software => TimestampMode::Software,
        };
        rc.rt_priority = plan.rt_priority;
        rc.cpu = plan.cpu;
        // On failure the publisher is dropped, which returns the writer.
        let runner = tessera_clock::platform::Runner::new(rc, publisher)
            .map_err(|e| e.to_string())?
            .spawn()
            .map_err(|e| e.to_string())?;
        self.runner = Some(runner);
        self.started_on = Some(info);
        self.iface_lost = None;
        self.rt_warned = false;
        self.rejected_seen = 0;
        self.starts += 1;
        self.applied = Some(Applied::of(plan));
        Ok(())
    }

    /// While a runner is replaced: keep the overlay alive and usable. If the
    /// clock was locked, leading or already in holdover it is published as
    /// `holdover` (same mapping, fresh liveness) so that TX keeps flowing; the
    /// new port keeps it that way until it has caught up (see
    /// [`SlotPublisher`]).
    fn hold_overlay(&mut self) {
        let mut g = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(h) = g.as_mut() {
            if let Some(mut s) = h.last {
                if usable_state(s.state) {
                    s.state = IpcState::Holdover;
                    s.updated_mono_ns = raw_monotonic_ns();
                    h.writer.publish(&s);
                    h.last = Some(s);
                }
            }
        }
    }

    /// Keep the runner in step with the plan: a running port gets role,
    /// advertised-clock and timestamping changes as live commands (no restart,
    /// no step of media time); a stopped one is started.
    fn reconcile(&mut self, plan: &ClockPlan, now: Instant) {
        let startable = plan.enabled
            && !self.iface.is_empty()
            && !netif::only_loopback(std::slice::from_ref(&self.iface));
        if !startable {
            self.stop_runner(false);
            return;
        }
        self.check_interface(now);
        let wants = Applied::of(plan);
        if let Some(have) = self.applied {
            if self.runner.is_some() && have != wants {
                self.apply_live(have, wants, now);
            }
        }
        // A failed start is retried on the timer, unless the plan changed
        // since: then at once.
        if self.failed_with.as_ref().is_some_and(|f| *f != wants) {
            self.failed_with = None;
            self.next_attempt = now;
        }
        if self.runner.is_none() && now >= self.next_attempt {
            match self.start(plan) {
                Ok(()) => {
                    if self.last_error.take().is_some() {
                        tracing::info!("ptp {}: started after an earlier failure", self.iface);
                    } else {
                        tracing::info!("ptp {}: started (role {:?})", self.iface, plan.role);
                    }
                }
                Err(e) => {
                    self.next_attempt = now
                        + if self.iface_lost.is_some() {
                            IFACE_CHECK
                        } else {
                            RETRY
                        };
                    self.failed_with = Some(wants);
                    if self.last_error.as_deref() != Some(e.as_str()) {
                        tracing::error!(
                            "ptp {}: cannot start, running free (retrying every {} s): {e}",
                            self.iface,
                            self.next_attempt.duration_since(now).as_secs()
                        );
                    }
                    self.last_error = Some(e);
                }
            }
        }
    }

    /// A running port's sockets belong to the interface it started on. If
    /// that interface is gone, or has been replaced (a new index: an adapter
    /// unplugged and plugged back in, a driver reset) or re-addressed, the
    /// sockets are dead: stop the port, keep the clock in holdover, and start
    /// a new port as soon as the interface is usable again.
    fn check_interface(&mut self, now: Instant) {
        if self.runner.is_none() || now.duration_since(self.last_iface_check) < IFACE_CHECK {
            return;
        }
        self.last_iface_check = now;
        let current = netif::lookup(&self.iface).ok();
        if current.is_some() && current == self.started_on {
            return;
        }
        match &current {
            None => tracing::warn!(
                "ptp {}: the interface went away; holding the clock until it is back",
                self.iface
            ),
            Some(i) => tracing::info!(
                "ptp {}: the interface was replaced or re-addressed (index {}, {}); \
                 restarting the port",
                self.iface,
                i.index,
                i.addr
            ),
        }
        self.stop_runner(true);
        self.started_on = None;
        self.iface_lost = Some(now);
        self.failed_with = None;
        self.next_attempt = now;
    }

    /// Send the commands that bring the running port from `have` to `want`.
    /// A full command ring leaves the rest for the next poll; a stopped
    /// runner (its thread died) is restarted.
    fn apply_live(&mut self, have: Applied, want: Applied, now: Instant) {
        let iface = self.iface.clone();
        let mut cur = have;
        let mut dead = false;
        if let Some(r) = self.runner.as_mut() {
            for step in live_steps(&have, &want) {
                match r.send(step.command()) {
                    Ok(()) => {
                        tracing::info!("ptp {iface}: live change {step:?}");
                        step.record(&mut cur);
                    }
                    Err(CommandError::Full) => break,
                    Err(CommandError::Stopped) => {
                        dead = true;
                        break;
                    }
                }
            }
        }
        self.applied = Some(cur);
        if dead {
            tracing::error!("ptp {iface}: the runner thread has stopped; restarting it");
            self.stop_runner(true);
            self.next_attempt = now;
        }
    }

    /// Republish the free-running overlay while no runner owns the writer.
    fn fallback_publish(&mut self, now: Instant) {
        if self.runner.is_some() || now.duration_since(self.last_fallback) < FALLBACK_PERIOD {
            return;
        }
        self.last_fallback = now;
        let mut g = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(h) = g.as_mut() {
            let mut s = h
                .last
                .unwrap_or_else(|| initial_snapshot(raw_monotonic_ns()));
            // The mapping (anchor, rate, epoch) is kept: media time stays
            // continuous; only the state and the liveness stamp change. A
            // usable clock whose interface just went away is in holdover for a
            // while, as it would be if only the leader had gone.
            let holding = self
                .iface_lost
                .is_some_and(|t| now.duration_since(t) < HOLDOVER_WITHOUT_IFACE)
                && usable_state(s.state);
            s.state = if holding {
                IpcState::Holdover
            } else {
                IpcState::FreeRunning
            };
            s.updated_mono_ns = raw_monotonic_ns();
            h.writer.publish(&s);
            h.last = Some(s);
        }
    }

    fn drain_events(&mut self) {
        let Some(r) = self.runner.as_mut() else {
            return;
        };
        while let Some(ev) = r.pop_event() {
            match ev.kind {
                RunnerEventKind::StateChanged { from, to } => {
                    tracing::info!("ptp {}: port {from:?} -> {to:?}", self.iface);
                }
                RunnerEventKind::Port(e) => tracing::debug!("ptp {}: {e:?}", self.iface),
                RunnerEventKind::RealtimeStep => {
                    tracing::warn!(
                        "ptp {}: CLOCK_REALTIME stepped; samples discarded",
                        self.iface
                    );
                }
            }
        }
        let rejected = r.status().commands_rejected.load(Ordering::Relaxed);
        if rejected > self.rejected_seen {
            self.rejected_seen = rejected;
            tracing::error!(
                "ptp {}: the runner rejected a live command ({rejected} so far; hardware \
                 timestamping on an interface that cannot stamp PTPv1 is refused and the \
                 previous mode stays)",
                self.iface
            );
        }
        if !self.rt_warned
            && r.status()
                .rt_degraded
                .load(std::sync::atomic::Ordering::Relaxed)
                != 0
        {
            self.rt_warned = true;
            tracing::warn!(
                "ptp {}: real-time scheduling or CPU affinity was not granted; running degraded",
                self.iface
            );
        }
    }

    fn status(&self) -> ClockStatus {
        let snap = OverlaySnapshot::read(&self.reader).ok();
        let (state, ts) = match (&self.runner, snap) {
            (Some(r), Some(s)) => {
                let hw = r
                    .status()
                    .hardware_timestamping
                    .load(std::sync::atomic::Ordering::Relaxed)
                    != 0;
                (
                    map_state(s.state),
                    if hw {
                        Timestamping::Hardware
                    } else {
                        Timestamping::Software
                    },
                )
            }
            _ => (ClockState::FreeRunning, Timestamping::Software),
        };
        let mut st = ClockStatus::new(self.iface.clone(), state, ts);
        if let (Some(r), Some(s)) = (&self.runner, snap) {
            let offset = r
                .status()
                .offset_ns
                .load(std::sync::atomic::Ordering::Relaxed);
            if matches!(
                state,
                ClockState::Acquiring | ClockState::Locked | ClockState::Holdover
            ) {
                st = st.with_offset_ns(Some(offset));
            }
            st = st.with_rate_ppb(s.rate_ppb).with_epoch(s.epoch);
            if s.gm_identity != 0 {
                st = st.with_grandmaster(Some(format!("{:012x}", s.gm_identity)));
            }
        } else if let Some(s) = snap {
            st = st.with_rate_ppb(s.rate_ppb).with_epoch(s.epoch);
        }
        let (applied, rejected) = self.runner.as_ref().map_or((0, 0), |r| {
            (
                r.status().commands_applied.load(Ordering::Relaxed),
                r.status().commands_rejected.load(Ordering::Relaxed),
            )
        });
        st.with_runner(
            self.starts,
            self.cmd_base.0 + applied,
            self.cmd_base.1 + rejected,
        )
    }
}

fn map_state(s: tessera_clock::media_clock::ClockState) -> ClockState {
    use tessera_clock::media_clock::ClockState as C;
    match s {
        C::FreeRunning => ClockState::FreeRunning,
        C::Listening => ClockState::Listening,
        C::Acquiring => ClockState::Acquiring,
        C::Locked => ClockState::Locked,
        C::Holdover => ClockState::Holdover,
        C::Leader => ClockState::Leader,
    }
}

/// Handle to the clock domain.
#[derive(Clone)]
pub struct ClockHandle {
    tx: SyncSender<Msg>,
    overlay: Arc<OwnedFd>,
}

impl fmt::Debug for ClockHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClockHandle")
    }
}

impl ClockHandle {
    /// A descriptor for the (sealed, read-only for clients) media-clock
    /// overlay. All devices of the daemon share one media clock.
    ///
    /// # Errors
    /// A message if the descriptor cannot be duplicated.
    pub fn overlay_fd(&self, _device: &str) -> Result<OwnedFd, String> {
        self.overlay.try_clone().map_err(|e| e.to_string())
    }

    /// A reader of the media-clock overlay (for the media engine).
    ///
    /// # Errors
    /// The overlay cannot be mapped.
    pub fn reader(&self) -> std::io::Result<ClockOverlayReader> {
        let fd = self.overlay.try_clone()?;
        ClockOverlayReader::attach(fd).map_err(std::io::Error::other)
    }

    /// Apply a new plan live (see the module documentation).
    pub fn apply(&self, plan: ClockPlan) {
        // A full queue means plans are arriving faster than they can be
        // applied; the newest one must still win, so wait for room.
        let _ = self.tx.send(Msg::Apply(Box::new(plan)));
    }

    /// Ask the thread to stop.
    pub fn stop(&self) {
        let _ = self.tx.send(Msg::Stop);
    }
}

fn reconfigure(domains: &mut Vec<Domain>, plan: &ClockPlan) -> std::io::Result<()> {
    // The primary domain is never destroyed: its overlay was handed to
    // clients. Its interface follows the plan.
    let want: Vec<&String> = plan.interfaces.iter().collect();
    if let Some(d) = domains.first_mut() {
        let new = want.first().map_or_else(String::new, |s| (*s).clone());
        if d.iface != new {
            d.stop_runner(!new.is_empty());
            d.iface = new;
            d.next_attempt = Instant::now();
            d.last_error = None;
            d.reported = None;
        }
    }
    // Secondary domains: keep those still wanted, drop the rest, add new.
    let mut keep: Vec<Domain> = Vec::new();
    for mut d in domains.drain(1..) {
        if want.iter().skip(1).any(|n| **n == d.iface) {
            keep.push(d);
        } else {
            d.stop_runner(false);
        }
    }
    for n in want.iter().skip(1) {
        if !keep.iter().any(|d| d.iface == **n) {
            let (d, _fd) = Domain::new((*n).clone(), false)?;
            keep.push(d);
        }
    }
    domains.extend(keep);
    Ok(())
}

fn manager(rx: &mpsc::Receiver<Msg>, mut domains: Vec<Domain>, engine: &EngineHandle) {
    let mut plan: Option<ClockPlan> = None;
    loop {
        match rx.recv_timeout(POLL) {
            Ok(Msg::Apply(p)) => {
                if let Err(e) = reconfigure(&mut domains, &p) {
                    tracing::error!("clock domain: {e}");
                }
                plan = Some(*p);
            }
            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let now = Instant::now();
        for d in &mut domains {
            if let Some(p) = &plan {
                d.reconcile(p, now);
            }
            d.drain_events();
            if d.primary {
                d.fallback_publish(now);
            }
            if d.iface.is_empty() {
                continue;
            }
            let st = d.status();
            let changed = match &d.reported {
                None => true,
                Some(r) => {
                    r.state != st.state
                        || r.timestamping != st.timestamping
                        || r.epoch != st.epoch
                        || r.grandmaster != st.grandmaster
                        || r.runner_starts != st.runner_starts
                        || r.commands_applied != st.commands_applied
                        || r.commands_rejected != st.commands_rejected
                        || (now.duration_since(d.reported_at) >= REPORT_EVERY && *r != st)
                }
            };
            if changed {
                engine.report_clock(st.clone());
                d.reported = Some(st);
                d.reported_at = now;
            }
        }
    }
    for d in &mut domains {
        d.stop_runner(false);
    }
}

/// Start the clock-domain manager.
///
/// The overlay exists from here on (state `free_running`); the first
/// [`ClockHandle::apply`] starts the runners.
///
/// # Errors
/// The overlay or the thread could not be created.
pub fn spawn(engine: EngineHandle) -> std::io::Result<(ClockHandle, JoinHandle<()>)> {
    let (primary, export) = Domain::new(String::new(), true)?;
    let (tx, rx) = mpsc::sync_channel::<Msg>(QUEUE);
    let join = std::thread::Builder::new()
        .name("tessera-clock-mgr".into())
        .spawn(move || {
            let _abort = crate::rt::AbortOnPanic;
            manager(&rx, vec![primary], &engine);
        })?;
    Ok((
        ClockHandle {
            tx,
            overlay: Arc::new(export),
        },
        join,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_maps_monotonically_and_never_overflows() {
        let mut prev = i16::MIN;
        for p in 0..=255u8 {
            let v = priority_variance(p);
            assert!(v > prev || p == 0, "priority {p}");
            prev = v;
        }
        assert_eq!(priority_variance(128), -4000);
        assert!(priority_variance(0) < priority_variance(255));
    }

    #[test]
    fn a_runner_restart_hands_the_writer_back_and_continues_the_epoch() {
        let (mut d, _fd) = Domain::new("x".into(), true).unwrap();
        let h = d.slot.lock().unwrap().take().unwrap();
        let slot = Arc::clone(&d.slot);
        let mut p = SlotPublisher {
            writer: Some(h.writer),
            last: h.last,
            epoch_base: 5,
            slot,
            fresh: false,
            hold_on_stop: Arc::new(AtomicBool::new(false)),
            hold_until: None,
        };
        let mut s = MediaClockSnapshot {
            epoch: 2,
            base_mono_ns: 10,
            base_media_ns: 20,
            rate_ppb: 7,
            state: tessera_clock::media_clock::ClockState::Locked,
            gm_identity: 9,
            updated_mono_ns: 11,
        };
        p.publish(&s);
        assert_eq!(OverlaySnapshot::read(&d.reader).unwrap().epoch, 7);
        s.epoch = 3;
        p.publish(&s);
        drop(p);
        let back = d.slot.lock().unwrap().take().expect("writer handed back");
        assert_eq!(back.last.unwrap().epoch, 8);
        assert!(!back.fresh);
        // The same memfd keeps working for the reader.
        let mut w = back.writer;
        let mut again = back.last.unwrap();
        again.rate_ppb = 11;
        w.publish(&again);
        assert_eq!(OverlaySnapshot::read(&d.reader).unwrap().rate_ppb, 11);
        d.stop_runner(false);
    }

    #[test]
    fn a_failed_start_is_retried_on_the_timer_not_on_every_poll() {
        let (mut d, _fd) = Domain::new("tessera-no-such-if".into(), true).unwrap();
        let mut plan = ClockPlan::free_running();
        plan.enabled = true;
        plan.interfaces = vec!["tessera-no-such-if".into()];
        let t0 = Instant::now();
        d.reconcile(&plan, t0);
        let first = d.next_attempt;
        assert!(first > t0 + Duration::from_secs(1));
        // Polling again does not move the retry time ...
        d.reconcile(&plan, t0 + Duration::from_millis(100));
        assert_eq!(d.next_attempt, first);
        // ... until the plan changes, which retries at once.
        plan.role = ClockRole::Auto;
        let t1 = t0 + Duration::from_millis(200);
        d.reconcile(&plan, t1);
        assert!(d.last_error.is_some());
        assert!(
            d.next_attempt > t1,
            "the new attempt failed and was rescheduled"
        );
    }

    fn snap_in(state: IpcState, epoch: u64) -> ClockSnapshot {
        ClockSnapshot {
            epoch,
            base_mono_ns: 1_000,
            base_media_ns: 5_000,
            rate_ppb: 0,
            state,
            gm_identity: 0xabc,
            updated_mono_ns: 1_000,
        }
    }

    #[test]
    fn a_first_leader_start_changes_the_epoch_because_the_timeline_jumps() {
        // The overlay is on the monotonic timeline; a leader moves it to TAI.
        let last = Some(snap_in(IpcState::FreeRunning, 3));
        let leader = continuation(last, true, true);
        assert_eq!(leader.epoch_base, 4, "media time jumps: new epoch");
        assert_eq!(leader.old, None, "nothing to stay continuous with");
        // A first follower start stays on the monotonic timeline.
        let follower = continuation(last, true, false);
        assert_eq!(follower.epoch_base, 3);
        assert_eq!(follower.old, None);
        // A restart starts at the old mapping and changes the epoch, whoever
        // leads.
        for leads in [false, true] {
            let c = continuation(Some(snap_in(IpcState::Locked, 9)), false, leads);
            assert_eq!(c.epoch_base, 10);
            assert_eq!(c.old.map(|s| s.epoch), Some(9));
        }
    }

    #[test]
    fn a_restarted_port_is_anchored_where_the_old_mapping_was_read() {
        // The runner builds the port on its own thread, later (longer on a
        // busy host): its clock continues the old mapping at the instant the
        // daemon read it, so followers see no step.
        let (d, _fd) = Domain::new("x".into(), true).unwrap();
        let mut h = d.slot.lock().unwrap().take().unwrap();
        let mut old = snap_in(IpcState::Locked, 2);
        old.rate_ppb = 20_000;
        h.last = Some(old);
        h.fresh = false;
        let info = netif::IfInfo {
            name: "eth0".into(),
            index: 2,
            addr: std::net::Ipv4Addr::new(10, 77, 0, 1),
            mac: [2, 0, 0, 0, 0, 1],
            wireless: false,
        };
        let mut plan = ClockPlan::free_running();
        plan.role = ClockRole::Leader;
        let now = 5_000_000;
        let pc = Domain::port_config(&plan, &info, &h, now);
        assert_eq!(pc.initial_local_ns, Some(now));
        let old = OverlaySnapshot::from_ipc(old);
        assert_eq!(pc.initial_media_ns, Some(old.media_ns_at(now)));
        assert_eq!(pc.initial_rate_ppb, 20_000);
        // A first start has nothing to continue.
        h.fresh = true;
        assert_eq!(
            Domain::port_config(&plan, &info, &h, now).initial_local_ns,
            None
        );
    }

    #[test]
    fn a_restarted_port_is_shown_as_holdover_while_it_catches_up() {
        let (d, _fd) = Domain::new("x".into(), true).unwrap();
        let h = d.slot.lock().unwrap().take().unwrap();
        let old = snap_in(IpcState::Locked, 2);
        let mut p = SlotPublisher {
            writer: Some(h.writer),
            last: Some(old),
            epoch_base: 2,
            slot: Arc::clone(&d.slot),
            fresh: false,
            hold_on_stop: Arc::new(AtomicBool::new(false)),
            hold_until: Some((u64::MAX, old.gm_identity)),
        };
        let mk = |state, epoch| MediaClockSnapshot {
            epoch,
            base_mono_ns: 2_000,
            base_media_ns: 6_000,
            rate_ppb: 0,
            state,
            gm_identity: 0,
            updated_mono_ns: 2_000,
        };
        use tessera_clock::media_clock::ClockState as C;
        for st in [C::FreeRunning, C::Listening, C::Acquiring] {
            p.publish(&mk(st, 0));
            let s = p.last.unwrap();
            assert_eq!(s.state, IpcState::Holdover, "{st:?} while catching up");
            assert_eq!(s.gm_identity, 0xabc, "the old grandmaster is kept");
        }
        // Reaching locked ends the hold and tells the truth from then on.
        p.publish(&mk(C::Locked, 0));
        assert_eq!(p.last.unwrap().state, IpcState::Locked);
        p.publish(&mk(C::Listening, 0));
        assert_eq!(
            p.last.unwrap().state,
            IpcState::Listening,
            "no hold any more"
        );
    }

    #[test]
    fn a_restart_that_began_unusable_is_not_dressed_up_as_holdover() {
        let (d, _fd) = Domain::new("x".into(), true).unwrap();
        let h = d.slot.lock().unwrap().take().unwrap();
        let old = snap_in(IpcState::Listening, 2);
        // The caller sets `hold_until` only for a usable old state.
        let hold = Some(old).filter(|s| usable_state(s.state));
        assert!(hold.is_none());
        drop(h);
    }

    #[test]
    fn a_follower_restarts_for_nothing_and_a_live_change_is_a_few_commands() {
        let mut plan = ClockPlan::free_running();
        plan.role = ClockRole::Follower;
        plan.priority = 10;
        let a = Applied::of(&plan);
        plan.priority = 200;
        assert!(
            live_steps(&a, &Applied::of(&plan)).is_empty(),
            "a follower never uses priority"
        );
        plan.rt_priority = Some(50);
        plan.cpu = Some(2);
        assert!(
            live_steps(&a, &Applied::of(&plan)).is_empty(),
            "rt priority and cpu apply at the next start, with no command"
        );
        // Timestamping alone is one command.
        plan.timestamping = TsMode::Software;
        assert_eq!(
            live_steps(&a, &Applied::of(&plan)),
            vec![Step::Timestamping(TsMode::Software)]
        );
    }

    #[test]
    fn promotion_sends_the_clock_before_the_role_and_demotion_the_role_first() {
        let mut plan = ClockPlan::free_running();
        plan.role = ClockRole::Follower;
        let follower = Applied::of(&plan);
        plan.role = ClockRole::Auto;
        plan.priority = 20;
        let auto = Applied::of(&plan);
        assert_eq!(
            live_steps(&follower, &auto),
            vec![Step::Clock(priority_clock(20)), Step::Role(ClockRole::Auto)],
            "a follower advertises slave-only (255): it must not lead with that"
        );
        assert_eq!(
            live_steps(&auto, &follower),
            vec![
                Step::Role(ClockRole::Follower),
                Step::Clock(LocalClock::follower())
            ],
            "the leader stops first"
        );
        // A priority change of a leader-capable port is the clock alone.
        plan.priority = 200;
        assert_eq!(
            live_steps(&auto, &Applied::of(&plan)),
            vec![Step::Clock(priority_clock(200))]
        );
        // Auto <-> Leader is the role alone.
        plan.priority = 20;
        plan.role = ClockRole::Leader;
        assert_eq!(
            live_steps(&auto, &Applied::of(&plan)),
            vec![Step::Role(ClockRole::Leader)]
        );
        assert!(live_steps(&auto, &auto).is_empty());
    }

    #[test]
    fn the_default_priority_advertises_what_a_leader_advertises_unconfigured() {
        assert_eq!(priority_clock(128), LocalClock::leader_capable());
    }

    #[test]
    fn a_lower_priority_never_loses_on_any_comparison_criterion() {
        // The PTPv1 comparison order: preferred (set wins), stratum (lower
        // wins), identifier (equal here), variance (lower wins).
        let key = |p: u8| {
            let l = priority_clock(p);
            (!l.preferred, l.stratum, l.variance)
        };
        for p in 0..=254u8 {
            assert!(key(p) <= key(p + 1), "priority {p}");
        }
        assert!(key(0) < key(255));
        // Never slave-only, never a stratum that could outrank hardware (1-3).
        for p in 0..=255u8 {
            let l = priority_clock(p);
            assert!((4..=7).contains(&l.stratum), "priority {p}");
        }
        assert!(priority_clock(10).preferred && !priority_clock(100).preferred);
    }

    #[test]
    fn a_replaced_runner_publishes_its_final_state_as_holdover() {
        let (d, _fd) = Domain::new("x".into(), true).unwrap();
        let h = d.slot.lock().unwrap().take().unwrap();
        let stopping = Arc::new(AtomicBool::new(true));
        let mut p = SlotPublisher {
            writer: Some(h.writer),
            last: h.last,
            epoch_base: 0,
            slot: Arc::clone(&d.slot),
            fresh: false,
            hold_on_stop: Arc::clone(&stopping),
            hold_until: None,
        };
        let fin = MediaClockSnapshot {
            epoch: 1,
            base_mono_ns: 10,
            base_media_ns: 20,
            rate_ppb: 3,
            state: tessera_clock::media_clock::ClockState::FreeRunning,
            gm_identity: 4,
            updated_mono_ns: 11,
        };
        p.publish(&fin);
        assert_eq!(p.last.unwrap().state, IpcState::Holdover);
        // A plain stop is not dressed up.
        stopping.store(false, Ordering::Relaxed);
        p.publish(&fin);
        assert_eq!(p.last.unwrap().state, IpcState::FreeRunning);
    }

    #[test]
    fn a_runner_that_published_nothing_leaves_the_overlay_fresh() {
        let (d, _fd) = Domain::new("x".into(), true).unwrap();
        let h = d.slot.lock().unwrap().take().unwrap();
        let p = SlotPublisher {
            writer: Some(h.writer),
            last: h.last,
            epoch_base: 0,
            slot: Arc::clone(&d.slot),
            fresh: h.fresh,
            hold_on_stop: Arc::new(AtomicBool::new(false)),
            hold_until: None,
        };
        drop(p);
        assert!(
            d.slot.lock().unwrap().as_ref().unwrap().fresh,
            "a failed start must not count as a first start"
        );
    }

    #[test]
    fn hold_overlay_keeps_a_usable_clock_usable_while_the_runner_is_replaced() {
        let (d, _fd) = Domain::new("x".into(), true).unwrap();
        {
            let mut g = d.slot.lock().unwrap();
            let h = g.as_mut().unwrap();
            let mut s = snap_in(IpcState::Locked, 2);
            s.updated_mono_ns = raw_monotonic_ns();
            h.writer.publish(&s);
            h.last = Some(s);
        }
        let mut d = d;
        d.hold_overlay();
        let o = OverlaySnapshot::read(&d.reader).unwrap();
        assert_eq!(o.state, tessera_clock::media_clock::ClockState::Holdover);
        assert_eq!(o.epoch, 2);
        // A free-running clock is left alone.
        {
            let mut g = d.slot.lock().unwrap();
            let h = g.as_mut().unwrap();
            let s = snap_in(IpcState::FreeRunning, 2);
            h.writer.publish(&s);
            h.last = Some(s);
        }
        d.hold_overlay();
        assert_eq!(
            OverlaySnapshot::read(&d.reader).unwrap().state,
            tessera_clock::media_clock::ClockState::FreeRunning
        );
    }

    #[test]
    fn loopback_only_never_starts_ptp() {
        let (mut d, _fd) = Domain::new("lo".into(), true).unwrap();
        let mut plan = ClockPlan::free_running();
        plan.enabled = true;
        plan.interfaces = vec!["lo".into()];
        d.reconcile(&plan, Instant::now());
        assert!(d.runner.is_none() && d.last_error.is_none());
    }

    #[test]
    fn a_missing_interface_degrades_to_free_running_and_keeps_publishing() {
        let (mut d, _fd) = Domain::new("tessera-no-such-if".into(), true).unwrap();
        let mut plan = ClockPlan::free_running();
        plan.enabled = true;
        plan.interfaces = vec!["tessera-no-such-if".into()];
        d.reconcile(&plan, Instant::now());
        assert!(d.runner.is_none());
        assert!(
            d.last_error
                .as_deref()
                .unwrap()
                .contains("no network interface")
        );
        let before = OverlaySnapshot::read(&d.reader).unwrap().updated_mono_ns;
        d.last_fallback = Instant::now() - Duration::from_secs(1);
        std::thread::sleep(Duration::from_millis(2));
        d.fallback_publish(Instant::now());
        let after = OverlaySnapshot::read(&d.reader).unwrap();
        assert!(after.updated_mono_ns > before);
        assert_eq!(
            after.state,
            tessera_clock::media_clock::ClockState::FreeRunning
        );
        assert_eq!(d.status().state, ClockState::FreeRunning);
    }
}
