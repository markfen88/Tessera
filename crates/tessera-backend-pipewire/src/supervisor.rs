//! The control side of `tessera-pw`: owns the PipeWire loop, creates, updates
//! and removes nodes from the device descriptions, keeps every node's daemon
//! session alive (reconnect with back-off, re-attaching rings), and turns the
//! media clock into graph cycles.
//!
//! Everything here runs on the PipeWire main-loop thread. Blocking work (the
//! handshake, the control query) runs on short-lived worker threads and
//! reports back through a PipeWire channel, so a stalled daemon can never
//! stall the loop (and with it the graph cycles of the other nodes).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::loop_::{IoSource, Signal, TimerSource};
use pw::main_loop::MainLoopRc;
use pw::spa::support::system::IoFlags;
use tessera_audio_ipc::{Direction, WakeWait, monotonic_ns};

use crate::backoff::Backoff;
use crate::config::Config;
use crate::device::{DeviceDesc, Dir, EndpointSpec, Parsed, fetch_devices, plan_endpoints};
use crate::driver::{Driver, Tick};
use crate::error::Error;
use crate::node::{PwNode, create_node, node_name, update_labels};
use crate::rt::{RtControl, RtShared};
use crate::session::{Link, LinkState, Session, SessionRequest, connect_session};
use crate::sys;

/// Results delivered from worker threads to the main loop.
enum Done {
    Devices(Result<Parsed, Error>),
    Attached {
        id: String,
        token: u64,
        result: Result<Session, Error>,
    },
}

/// `AsRawFd` wrapper so the wake socket can be a loop IO source.
struct WakeFd(WakeWait);

impl AsRawFd for WakeFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_fd().as_raw_fd()
    }
}

type Timer = TimerSource<'static>;

struct Attached {
    link: Link,
    since: Instant,
    // Dropping the source removes it from the loop.
    _wake_io: IoSource<'static, WakeFd>,
}

struct NodeInner {
    ctl: RtControl,
    driver: Driver,
    attached: Option<Attached>,
    connecting: bool,
    token: u64,
    backoff: Backoff,
    lost: bool,
    pending_detach: bool,
    logged_failure: bool,
    last_notice: Option<Instant>,
    housekeeping_ticks: u64,
}

/// One endpoint: a PipeWire node plus its daemon session.
struct Node {
    spec: RefCell<EndpointSpec>,
    name: String,
    cfg: Rc<Config>,
    pw: PwNode,
    shared: Arc<RtShared>,
    inner: RefCell<NodeInner>,
    missing: Cell<u32>,
    tick_timer: Timer,
    reconnect_timer: Timer,
    maint_timer: Timer,
    loop_: &'static pw::loop_::Loop,
    done_tx: pw::channel::Sender<Done>,
    sup: Weak<Supervisor>,
    me: RefCell<Weak<Node>>,
}

const MAX_WAIT: Duration = Duration::from_millis(20);
const MIN_WAIT: Duration = Duration::from_micros(30);
const HOUSEKEEPING: Duration = Duration::from_millis(100);
/// Repeated loss/attach notices of one node are logged at warn/info at most
/// this often; the rest go to debug.
const NOTICE_EVERY: Duration = Duration::from_secs(30);

fn arm(timer: &Timer, d: Duration) {
    let _ = timer.update_timer(Some(d.max(MIN_WAIT)), None);
}

impl Node {
    /// True when a notice may be logged loudly now (and records it).
    fn notice_due(inner: &mut NodeInner) -> bool {
        let due = inner
            .last_notice
            .is_none_or(|t| t.elapsed() >= NOTICE_EVERY);
        if due {
            inner.last_notice = Some(Instant::now());
        }
        due
    }

    /// Drives the graph: called from the wake socket and the tick timer. At
    /// most one cycle is fired per call; the next wake-up is armed for the
    /// next period boundary (the graph is never driven in bursts).
    fn tick(&self) {
        let Ok(mut inner) = self.inner.try_borrow_mut() else {
            return;
        };
        let stream = &self.pw.stream;
        if !stream.is_driving() {
            // Another driver runs the graph: follower mode is handled in the
            // real-time callback. Look again soon in case that changes.
            inner.driver.reset();
            arm(&self.tick_timer, MAX_WAIT);
            return;
        }
        let hint = self.shared.period_hint.load(Relaxed);
        let period = if hint == 0 {
            self.cfg.period_frames
        } else {
            hint
        };
        let wake = match inner.driver.tick(period) {
            Tick::Fire(f) => {
                // The position goes into the queue first (a full queue drops
                // it). Trigger in every case: only a processed cycle drains
                // the queue, so skipping the trigger on a full queue (left by
                // triggers that ran no cycle, e.g. while the graph was being
                // relinked) would stall the node for good. The real-time side
                // takes the newest position and treats a stale one as silence.
                let _ = inner.ctl.push_cycle(f);
                let _ = stream.trigger_process();
                inner.driver.next_wake(period)
            }
            Tick::Wait { mono_ns } => mono_ns,
        };
        let wait = Duration::from_nanos(wake.saturating_sub(monotonic_ns()));
        arm(&self.tick_timer, wait.min(MAX_WAIT));
    }

    fn flag_lost(&self) {
        let first = match self.inner.try_borrow_mut() {
            Ok(mut inner) => !std::mem::replace(&mut inner.lost, true),
            Err(_) => true,
        };
        // Tear down from the maintenance timer, never from inside the IO
        // source callback that noticed it. Arm once: the source stays readable
        // (EOF) and would otherwise keep pushing the timer back.
        if first {
            arm(&self.maint_timer, Duration::from_millis(1));
        }
    }

    /// Drops the session, silences the node and schedules a reconnect.
    fn handle_loss(&self, why: &str) {
        let (delay, loud) = {
            let Ok(mut inner) = self.inner.try_borrow_mut() else {
                return;
            };
            inner.lost = false;
            let Some(att) = inner.attached.take() else {
                return;
            };
            inner.driver.detach();
            if !inner.ctl.detach() {
                inner.pending_detach = true;
            }
            inner.token += 1;
            inner.connecting = false;
            // Only a session that stayed up for a while counts as success: a
            // daemon that accepts and detaches again must back off.
            if att.since.elapsed() >= self.cfg.stable_after {
                inner.backoff.reset();
            }
            (inner.backoff.next_delay(), Self::notice_due(&mut inner))
        };
        if loud {
            tracing::warn!(node = %self.name, "lost the daemon session ({why}); the node stays up, reconnecting");
        } else {
            tracing::debug!(node = %self.name, "lost the daemon session again ({why})");
        }
        arm(&self.reconnect_timer, delay);
    }

    fn request(&self) -> SessionRequest {
        let spec = self.spec.borrow();
        SessionRequest {
            endpoint: spec.device.clone(),
            direction: match spec.dir {
                Dir::Rx => Direction::Rx,
                Dir::Tx => Direction::Tx,
            },
            first_channel: spec.first_channel,
            channels: spec.channels(),
            rate: spec.rate,
            max_period: self.cfg.max_period_frames,
            client: self.cfg.client_name.clone(),
        }
    }

    /// Starts a handshake on a worker thread.
    fn connect(&self) {
        let token = {
            let Ok(mut inner) = self.inner.try_borrow_mut() else {
                return;
            };
            if inner.attached.is_some() || inner.connecting {
                return;
            }
            inner.connecting = true;
            inner.token += 1;
            inner.token
        };
        let (req, path, timeout) = (
            self.request(),
            self.cfg.audio_socket.clone(),
            self.cfg.handshake_timeout,
        );
        let (tx, id) = (self.done_tx.clone(), self.spec.borrow().id());
        let spawned = std::thread::Builder::new()
            .name("tessera-pw-attach".into())
            .spawn(move || {
                sys::set_normal();
                let result = connect_session(&path, &req, timeout);
                let _ = tx.send(Done::Attached { id, token, result });
            });
        if spawned.is_err() {
            if let Ok(mut inner) = self.inner.try_borrow_mut() {
                inner.connecting = false;
            }
            self.schedule_retry();
        }
    }

    fn schedule_retry(&self) {
        let delay = match self.inner.try_borrow_mut() {
            Ok(mut inner) => inner.backoff.next_delay(),
            Err(_) => Duration::from_millis(500),
        };
        arm(&self.reconnect_timer, delay);
    }

    fn on_attached(&self, token: u64, result: Result<Session, Error>) {
        {
            let Ok(mut inner) = self.inner.try_borrow_mut() else {
                return;
            };
            if token != inner.token || inner.attached.is_some() {
                return; // stale
            }
            inner.connecting = false;
        }
        match result {
            Err(e) => {
                let first = {
                    let mut inner = self.inner.borrow_mut();
                    !std::mem::replace(&mut inner.logged_failure, true)
                };
                if first {
                    tracing::info!(node = %self.name, "daemon not available yet: {e}");
                } else {
                    tracing::debug!(node = %self.name, "attach failed: {e}");
                }
                if matches!(e, Error::Rejected { .. }) {
                    if let Some(sup) = self.sup.upgrade() {
                        sup.discover_soon();
                    }
                }
                self.schedule_retry();
            }
            Ok(session) => self.install(session),
        }
    }

    fn install(&self, session: Session) {
        let Session { link, wake, rt } = session;
        let generation = {
            let mut inner = self.inner.borrow_mut();
            match inner.ctl.attach(rt) {
                Ok(g) => g,
                Err(_) => {
                    drop(inner);
                    self.schedule_retry();
                    return;
                }
            }
        };
        let weak = self.me.borrow().clone();
        let wake_io = self
            .loop_
            .add_io(WakeFd(wake), IoFlags::IN, move |w: &mut WakeFd| {
                let Some(node) = weak.upgrade() else { return };
                match w.0.drain() {
                    Ok(_) => node.tick(),
                    Err(_) => node.flag_lost(),
                }
            });
        {
            let mut inner = self.inner.borrow_mut();
            inner.driver.attach(&link, generation);
            inner.logged_failure = false;
            // The back-off is NOT reset here: see handle_loss.
            if Self::notice_due(&mut inner) {
                tracing::info!(node = %self.name, session = link.session_id, "attached to the daemon");
            } else {
                tracing::debug!(node = %self.name, session = link.session_id, "attached to the daemon again");
            }
            inner.attached = Some(Attached {
                link,
                since: Instant::now(),
                _wake_io: wake_io,
            });
        }
        self.tick();
    }

    /// Periodic work: reap retired sessions, notice a dead or hung daemon,
    /// retry a refused detach, log counters.
    fn maintenance(&self) {
        let (lost, closed) = {
            let Ok(mut inner) = self.inner.try_borrow_mut() else {
                arm(&self.maint_timer, HOUSEKEEPING);
                return;
            };
            inner.ctl.reap();
            if inner.pending_detach && inner.ctl.detach() {
                inner.pending_detach = false;
            }
            inner.housekeeping_ticks += 1;
            if matches!(inner.housekeeping_ticks % 100, 0) {
                self.log_stats(false);
            }
            let stale = self.cfg.overlay_stale;
            let closed = match inner.attached.as_mut() {
                Some(a) => match a.link.poll() {
                    LinkState::Open => {
                        // A daemon that hangs keeps its sockets open but stops
                        // refreshing the clock overlay.
                        a.link
                            .overlay
                            .try_read()
                            .filter(|snap| {
                                !stale.is_zero()
                                    && a.since.elapsed() >= stale
                                    && monotonic_ns().saturating_sub(snap.updated_mono_ns)
                                        > stale.as_nanos() as u64
                            })
                            .map(|_| "the clock overlay stopped updating (hung daemon)".to_string())
                    }
                    LinkState::Detached(r) => Some(format!("daemon detached us: {r:?}")),
                    LinkState::Closed => Some("the daemon closed the socket".to_string()),
                    LinkState::Failed => Some("the audio socket failed".to_string()),
                },
                None => None,
            };
            (inner.lost, closed)
        };
        if lost {
            self.handle_loss("the wake socket closed");
        } else if let Some(why) = closed {
            self.handle_loss(&why);
        }
        arm(&self.maint_timer, HOUSEKEEPING);
    }

    fn log_stats(&self, last: bool) {
        let s = &self.shared.stats;
        macro_rules! line {
            ($mac:ident) => {
                tracing::$mac!(
                    node = %self.name,
                    cycles = s.cycles.load(Relaxed),
                    driven = s.driven.load(Relaxed),
                    followed = s.followed.load(Relaxed),
                    silent = s.silent.load(Relaxed),
                    no_cycle = s.no_cycle.load(Relaxed),
                    skipped_cycles = s.skipped_cycles.load(Relaxed),
                    stale_cycles = s.stale_cycles.load(Relaxed),
                    rx_valid = s.rx_valid.load(Relaxed),
                    rx_holes = s.rx_holes.load(Relaxed),
                    rx_unwritten = s.rx_unwritten.load(Relaxed),
                    rx_overrun = s.rx_overrun.load(Relaxed),
                    tx_late = s.tx_late.load(Relaxed),
                    slips = s.slips.load(Relaxed),
                    clock_misses = s.clock_misses.load(Relaxed),
                    follow_err = s.follow_err.load(Relaxed),
                    follow_ppm = s.follow_ppm.load(Relaxed),
                    follow_err_max = s.follow_err_max.load(Relaxed),
                    "node counters"
                )
            };
        }
        if last {
            line!(info);
        } else {
            line!(debug);
        }
    }

    /// Applies a changed device description without recreating the node.
    fn apply_description(&self, spec: EndpointSpec) {
        match update_labels(&self.pw, &spec) {
            Ok(()) => tracing::info!(node = %self.name, "updated the node's labels in place"),
            Err(e) => tracing::warn!(node = %self.name, "cannot update labels: {e}"),
        }
        *self.spec.borrow_mut() = spec;
    }

    /// Final tear-down: tell the daemon, silence the node, disconnect.
    fn shutdown(&self) {
        self.log_stats(true);
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            if let Some(a) = inner.attached.take() {
                a.link.send_detach();
            }
            inner.driver.detach();
            let _ = inner.ctl.detach();
        }
        let _ = self.pw.stream.disconnect();
        // Stop all callbacks that reference this node.
        for t in [&self.tick_timer, &self.reconnect_timer, &self.maint_timer] {
            let _ = t.update_timer(None, None);
        }
    }
}

/// The supervisor: all nodes and the device list.
struct Supervisor {
    cfg: Rc<Config>,
    mainloop: &'static MainLoopRc,
    core: pw::core::CoreRc,
    done_tx: pw::channel::Sender<Done>,
    nodes: RefCell<HashMap<String, Rc<Node>>>,
    discover_timer: Timer,
    disc_backoff: RefCell<Backoff>,
    discovering: Cell<bool>,
    logged_rejects: RefCell<HashSet<String>>,
    me: RefCell<Weak<Supervisor>>,
}

impl Supervisor {
    fn wanted(&self, d: &DeviceDesc) -> bool {
        self.cfg.only_devices.is_empty() || self.cfg.only_devices.contains(&d.key)
    }

    fn discover_soon(&self) {
        let _ = self
            .discover_timer
            .update_timer(Some(Duration::from_millis(50)), None);
    }

    fn discover(&self) {
        if self.discovering.replace(true) {
            return;
        }
        let (tx, path, timeout, max) = (
            self.done_tx.clone(),
            self.cfg.control_socket.clone(),
            self.cfg.handshake_timeout,
            self.cfg.max_device_channels,
        );
        let spawned = std::thread::Builder::new()
            .name("tessera-pw-discover".into())
            .spawn(move || {
                sys::set_normal();
                let _ = tx.send(Done::Devices(fetch_devices(&path, timeout, max)));
            });
        if spawned.is_err() {
            self.discovering.set(false);
            let d = self.disc_backoff.borrow_mut().next_delay();
            arm(&self.discover_timer, d);
        }
    }

    fn on_done(&self, done: Done) {
        match done {
            Done::Devices(r) => self.on_devices(r),
            Done::Attached { id, token, result } => {
                let node = self.nodes.borrow().get(&id).cloned();
                match node {
                    Some(n) => n.on_attached(token, result),
                    None => drop(result),
                }
            }
        }
    }

    fn on_devices(&self, result: Result<Parsed, Error>) {
        self.discovering.set(false);
        let next = match result {
            Ok(parsed) => {
                for why in &parsed.rejected {
                    if self.logged_rejects.borrow_mut().insert(why.clone()) {
                        tracing::warn!("ignoring {why}");
                    }
                }
                let mut devs = parsed.devices;
                // Static devices fill in what the control socket does not know.
                for s in &self.cfg.static_devices {
                    if !devs.iter().any(|d| d.key == s.key) {
                        devs.push(s.clone());
                    }
                }
                self.disc_backoff.borrow_mut().reset();
                self.reconcile(devs);
                self.cfg.discover_interval
            }
            Err(e) => {
                // A failed query says nothing about the devices: keep them.
                tracing::debug!("device query failed: {e}");
                self.disc_backoff.borrow_mut().next_delay()
            }
        };
        arm(&self.discover_timer, next);
    }

    /// Creates, updates and removes nodes so they match `devs`: a node whose
    /// labels or display name changed is updated in place (PipeWire links
    /// survive); a node whose shape (channels, rate) changed is recreated; a
    /// node is removed only after its device was missing from
    /// `missing_queries` consecutive answers.
    fn reconcile(&self, devs: Vec<DeviceDesc>) {
        let mut wanted: HashMap<String, EndpointSpec> = HashMap::new();
        for d in devs.iter().filter(|d| self.wanted(d)) {
            for spec in plan_endpoints(d, self.cfg.max_node_channels) {
                wanted.insert(spec.id(), spec);
            }
        }
        let existing: Vec<(String, Rc<Node>)> = self
            .nodes
            .borrow()
            .iter()
            .map(|(k, v)| (k.clone(), Rc::clone(v)))
            .collect();
        for (id, node) in existing {
            let remove = match wanted.get(&id) {
                None => {
                    let n = node.missing.get() + 1;
                    node.missing.set(n);
                    if n < self.cfg.missing_queries {
                        tracing::debug!(node = %node.name, missing = n, "device missing from the answer");
                    }
                    n >= self.cfg.missing_queries
                }
                Some(new) => {
                    node.missing.set(0);
                    let old = node.spec.borrow().clone();
                    if *new == old {
                        false
                    } else if old.same_shape(new) {
                        node.apply_description(new.clone());
                        false
                    } else {
                        true
                    }
                }
            };
            if remove {
                tracing::info!(node = %node.name, "removing node (device changed or gone)");
                self.nodes.borrow_mut().remove(&id);
                node.shutdown();
            }
        }
        let missing: Vec<EndpointSpec> = wanted
            .into_values()
            .filter(|s| !self.nodes.borrow().contains_key(&s.id()))
            .collect();
        for spec in missing {
            match self.make_node(spec.clone()) {
                Ok(node) => {
                    tracing::info!(node = %node.name, channels = spec.channels(), "created node");
                    node.connect();
                    self.nodes.borrow_mut().insert(spec.id(), node);
                }
                Err(e) => tracing::error!("cannot create the node for {}: {e}", spec.id()),
            }
        }
    }

    fn make_node(&self, spec: EndpointSpec) -> Result<Rc<Node>, Error> {
        let (pw_node, ctl) = create_node(&self.core, &self.cfg, &spec)?;
        let loop_: &'static pw::loop_::Loop = self.mainloop.loop_();
        let slot: Rc<RefCell<Weak<Node>>> = Rc::new(RefCell::new(Weak::new()));
        let timer = |f: fn(&Node)| -> Timer {
            let slot = Rc::clone(&slot);
            loop_.add_timer(move |_| {
                let node = slot.borrow().upgrade();
                if let Some(n) = node {
                    f(&n);
                }
            })
        };
        let tick_timer = timer(Node::tick);
        let reconnect_timer = timer(Node::connect);
        let maint_timer = timer(Node::maintenance);
        let seed = spec.id().bytes().fold(0x9e37_79b9_7f4a_7c15u64, |a, b| {
            a.rotate_left(5) ^ u64::from(b)
        });
        let shared = Arc::clone(&ctl.shared);
        let driver = Driver::new(
            spec.dir,
            spec.rate,
            u64::from(self.cfg.rx_offset_frames),
            Arc::clone(&shared),
        );
        let node = Rc::new(Node {
            name: node_name(&self.cfg, &spec),
            spec: RefCell::new(spec),
            cfg: Rc::clone(&self.cfg),
            pw: pw_node,
            shared,
            inner: RefCell::new(NodeInner {
                ctl,
                driver,
                attached: None,
                connecting: false,
                token: 0,
                backoff: Backoff::new(self.cfg.backoff, seed),
                lost: false,
                pending_detach: false,
                logged_failure: false,
                last_notice: None,
                housekeeping_ticks: 0,
            }),
            missing: Cell::new(0),
            tick_timer,
            reconnect_timer,
            maint_timer,
            loop_,
            done_tx: self.done_tx.clone(),
            sup: self.me.borrow().clone(),
            me: RefCell::new(Weak::new()),
        });
        *slot.borrow_mut() = Rc::downgrade(&node);
        *node.me.borrow_mut() = Rc::downgrade(&node);
        arm(&node.maint_timer, HOUSEKEEPING);
        arm(&node.tick_timer, Duration::from_millis(5));
        Ok(node)
    }

    fn shutdown(&self) {
        let nodes: Vec<Rc<Node>> = self.nodes.borrow_mut().drain().map(|(_, n)| n).collect();
        for n in nodes {
            n.shutdown();
        }
        tracing::info!(
            rt_allocs = crate::alloc_check::rt_allocs(),
            "allocations inside the real-time callbacks"
        );
    }
}

fn is_fatal_core_error(res: i32) -> bool {
    matches!(
        -res,
        libc::EPIPE | libc::ECONNRESET | libc::ECONNABORTED | libc::ENOTCONN | libc::ESHUTDOWN
    )
}

/// Runs the client until a termination signal (exit code 0) or until
/// PipeWire goes away (exit code 1, so that systemd restarts the unit).
pub fn run(cfg: Config) -> Result<i32, Error> {
    let pw_err = |what: &str, e: pw::Error| Error::Config(format!("{what}: {e}"));
    if cfg.mlock {
        // First: everything allocated from here on is locked too (ADR 0004).
        if let Err(e) = sys::lock_memory() {
            tracing::warn!(
                "cannot lock memory ({e}); page faults may disturb the audio (needs LimitMEMLOCK)"
            );
        }
    }
    pw::init();
    let mainloop: &'static MainLoopRc = Box::leak(Box::new(
        MainLoopRc::new(None).map_err(|e| pw_err("cannot create the PipeWire loop", e))?,
    ));
    let context = pw::context::ContextRc::new(mainloop, None)
        .map_err(|e| pw_err("cannot create the PipeWire context", e))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| pw_err("cannot connect to PipeWire", e))?;
    // Real-time priority only now: PipeWire's own threads exist already and
    // keep what the library gives them; helper threads reset themselves.
    if cfg.rt_priority > 0 {
        match sys::set_realtime(cfg.rt_priority) {
            Ok(()) => tracing::info!(priority = cfg.rt_priority, "control loop runs SCHED_FIFO"),
            Err(e) => tracing::warn!(
                priority = cfg.rt_priority,
                "cannot enter SCHED_FIFO ({e}); running at normal priority (needs LimitRTPRIO; --rt-priority 0 silences this)"
            ),
        }
    }
    let exit = Rc::new(Cell::new(0i32));
    let _core_listener = core
        .add_listener_local()
        .error({
            let exit = Rc::clone(&exit);
            move |id, _seq, res, msg| {
                if id == 0 && is_fatal_core_error(res) {
                    tracing::error!("PipeWire connection lost ({msg}); exiting for a restart");
                    exit.set(1);
                    mainloop.quit();
                } else {
                    tracing::warn!(id, res, "PipeWire error: {msg}");
                }
            }
        })
        .register();

    let (done_tx, done_rx) = pw::channel::channel::<Done>();
    let cfg = Rc::new(cfg);
    let sup_slot: Rc<RefCell<Weak<Supervisor>>> = Rc::new(RefCell::new(Weak::new()));
    let discover_timer = {
        let slot = Rc::clone(&sup_slot);
        mainloop.loop_().add_timer(move |_| {
            let sup = slot.borrow().upgrade();
            if let Some(s) = sup {
                s.discover();
            }
        })
    };
    let sup = Rc::new(Supervisor {
        cfg: Rc::clone(&cfg),
        mainloop,
        core: core.clone(),
        done_tx,
        nodes: RefCell::new(HashMap::new()),
        discover_timer,
        disc_backoff: RefCell::new(Backoff::new(cfg.backoff, 0x5eed)),
        discovering: Cell::new(false),
        logged_rejects: RefCell::new(HashSet::new()),
        me: RefCell::new(Weak::new()),
    });
    *sup_slot.borrow_mut() = Rc::downgrade(&sup);
    *sup.me.borrow_mut() = Rc::downgrade(&sup);

    let _rx = done_rx.attach(mainloop.loop_(), {
        let weak = Rc::downgrade(&sup);
        move |d| {
            if let Some(s) = weak.upgrade() {
                s.on_done(d);
            }
        }
    });
    let _sigs: Vec<_> = [Signal::INT, Signal::TERM]
        .into_iter()
        .map(|s| {
            mainloop.loop_().add_signal_local(s, move || {
                tracing::info!("terminating");
                mainloop.quit();
            })
        })
        .collect();

    // Static devices exist immediately; the control socket refines them.
    if !cfg.static_devices.is_empty() {
        sup.reconcile(cfg.static_devices.clone());
    }
    sup.discover();
    mainloop.run();
    sup.shutdown();
    Ok(exit.get())
}
