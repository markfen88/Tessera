//! The PTP runner thread (ADR 0004: one thread per interface).
//!
//! The loop owns the sockets, the [`Port`] and the publisher. Per iteration:
//! `ppoll` until the port's next deadline, then drain the event socket, its
//! error queue (TX timestamps) and the general socket, convert kernel
//! timestamps to the monotonic domain, run due timers. The loop does not
//! allocate, lock or log: counters go to [`SharedStatus`] atomics, `Copy`
//! events to an SPSC ring ([`RunnerEvent`]).

use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsFd, AsRawFd};
use std::string::String;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use nix::poll::{PollFd, PollFlags, ppoll};
use nix::sys::eventfd::{EfdFlags, EventFd};
use nix::sys::time::TimeSpec;

use super::clockconv::{RealtimeToMono, mono_ns};
use super::socket::{PtpSocket, Received, SocketConfig, Stamps};
use super::status::SharedStatus;
use super::tsinfo::{Phc, TsCaps, enable_hw_timestamping, query_ts_caps};
use super::txmatch::{TxSlots, find_event_key};
use crate::action::{Action, ActionSink, Channel, PortEvent};
use crate::command::PortCommand;
use crate::config::{LocalClock, PortConfig, Role};
use crate::media_clock::{ClockPublisher, MediaClockSnapshot};
use crate::port::{Port, PortState, PortStats};
use crate::provisional::{DSCP_EVENT, DSCP_GENERAL};

/// Timestamping mode (`[clock].timestamping`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampMode {
    /// Hardware when the NIC supports PTPv1 stamping and a PHC is readable,
    /// else software.
    Auto,
    /// Hardware or fail to start.
    Hardware,
    /// Software (kernel `CLOCK_REALTIME`) stamps only.
    Software,
}

/// Runner configuration.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    /// Interface name (capability queries and hardware setup).
    pub iface: String,
    /// IPv4 address of that interface.
    pub local_addr: Ipv4Addr,
    /// The port configuration.
    pub port: PortConfig,
    /// Timestamping mode.
    pub timestamping: TimestampMode,
    /// A `CLOCK_REALTIME` jump above this (beyond slew) discards the batch.
    pub realtime_step_threshold_ns: u64,
    /// `SCHED_FIFO` priority for the thread (`None` = leave as is). Failure
    /// is reported through [`SharedStatus::rt_degraded`], never silent.
    pub rt_priority: Option<u8>,
    /// CPU to pin the thread to.
    pub cpu: Option<usize>,
    /// DSCP of event messages (provisional, see `provisional::DSCP_EVENT`).
    pub dscp_event: u8,
    /// DSCP of general messages.
    pub dscp_general: u8,
    /// Loop our own multicast back (needed only for single-host tests).
    pub multicast_loop: bool,
    /// How long to wait for a kernel TX timestamp before using the send time.
    pub tx_stamp_wait_ns: u64,
    /// Upper bound on one `ppoll` sleep.
    pub max_poll_ns: u64,
}

impl RunnerConfig {
    /// Defaults for `iface`/`local_addr`.
    #[must_use]
    pub fn new(iface: impl Into<String>, local_addr: Ipv4Addr, port: PortConfig) -> Self {
        RunnerConfig {
            iface: iface.into(),
            local_addr,
            port,
            timestamping: TimestampMode::Auto,
            realtime_step_threshold_ns: 2_000_000,
            rt_priority: None,
            cpu: None,
            dscp_event: DSCP_EVENT,
            dscp_general: DSCP_GENERAL,
            multicast_loop: false,
            tx_stamp_wait_ns: 20_000_000,
            max_poll_ns: 100_000_000,
        }
    }
}

/// A `Copy` event for the control plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunnerEvent {
    /// Local monotonic time of the event.
    pub at_mono_ns: u64,
    /// What happened.
    pub kind: RunnerEventKind,
}

/// Kinds of [`RunnerEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerEventKind {
    /// The port changed state.
    StateChanged {
        /// Old state.
        from: PortState,
        /// New state.
        to: PortState,
    },
    /// A port event.
    Port(PortEvent),
    /// A step of the realtime clock was detected; a batch was discarded.
    RealtimeStep,
}

/// A live command for a running runner, sent with [`RunnerHandle::send`].
///
/// Commands travel through a lock-free SPSC ring and are applied by the PTP
/// thread at the top of its loop (within one wake-up, so typically well under
/// a millisecond); the loop never allocates or locks to apply them. Each one
/// is counted in [`SharedStatus::commands_applied`] or, if it could not be
/// carried out, [`SharedStatus::commands_rejected`]; the resulting role and
/// advertised clock are mirrored in `SharedStatus` (`role`, `local_stratum`,
/// `local_preferred`, `publish_guard_hold_ns`, `hardware_timestamping`).
///
/// Role and clock-attribute changes go through the state machine
/// ([`PortCommand`]): the election is re-run, media time is never stepped by
/// the command, and the overlay epoch changes only if the servo later has to
/// step (ADR 0005).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerCommand {
    /// Leader / follower / auto ([`PortCommand::SetRole`]).
    SetRole(Role),
    /// The BMC dataset knobs we advertise ([`PortCommand::SetLocalClock`]).
    SetLocalClock(LocalClock),
    /// Publish guard on/off and tuning ([`PortCommand::SetPublishGuard`]).
    SetPublishGuard {
        /// Jump size that triggers the hold, ns.
        threshold_ns: u64,
        /// Hold time, ns; 0 = guard off.
        hold_ns: u64,
    },
    /// Timestamping mode. Software and hardware stamping are switched on the
    /// existing sockets (the socket option is changed and the NIC filter is
    /// widened, never narrowed): no rebind, so the overlay, the epoch and
    /// the media mapping are untouched. The PHC descriptor is opened when the
    /// runner is created, so the switch itself does not allocate. Requesting
    /// `Hardware` on an interface that cannot stamp PTPv1 is rejected and the
    /// current mode stays.
    SetTimestamping(TimestampMode),
}

impl RunnerCommand {
    fn port_command(self) -> Option<PortCommand> {
        match self {
            RunnerCommand::SetRole(r) => Some(PortCommand::SetRole(r)),
            RunnerCommand::SetLocalClock(l) => Some(PortCommand::SetLocalClock(l)),
            RunnerCommand::SetPublishGuard {
                threshold_ns,
                hold_ns,
            } => Some(PortCommand::SetPublishGuard {
                threshold_ns,
                hold_ns,
            }),
            RunnerCommand::SetTimestamping(_) => None,
        }
    }
}

/// Why [`RunnerHandle::send`] failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandError {
    /// The command ring is full (the runner thread is not keeping up).
    Full,
    /// The runner thread has stopped.
    Stopped,
}

impl core::fmt::Display for CommandError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CommandError::Full => f.write_str("runner command queue full"),
            CommandError::Stopped => f.write_str("runner stopped"),
        }
    }
}

impl std::error::Error for CommandError {}

/// What a stopped runner leaves behind.
#[derive(Debug, Clone, Copy)]
pub struct RunnerFinal {
    /// The last overlay snapshot, as published at shutdown: state
    /// `FreeRunning`, mapping, rate and epoch unchanged.
    pub snapshot: MediaClockSnapshot,
    /// The port counters at shutdown.
    pub stats: PortStats,
}

const COMMAND_RING: usize = 64;
const EVENT_RING: usize = 256;
const RX_BATCH: usize = 64;
/// Minimum sleep after a timer stayed due despite being run (a bug guard).
const MIN_STUCK_SLEEP_NS: u64 = 1_000_000;

struct IoSink<'a, P: ClockPublisher> {
    event: &'a PtpSocket,
    general: &'a PtpSocket,
    publisher: &'a mut P,
    status: &'a SharedStatus,
    tx: &'a mut TxSlots,
    events: &'a mut rtrb::Producer<RunnerEvent>,
    dropped_events: &'a mut u64,
}

impl<P: ClockPublisher> IoSink<'_, P> {
    fn emit(&mut self, kind: RunnerEventKind) {
        let ev = RunnerEvent {
            at_mono_ns: mono_ns(),
            kind,
        };
        if self.events.push(ev).is_err() {
            *self.dropped_events += 1;
        }
    }
}

impl<P: ClockPublisher> ActionSink for IoSink<'_, P> {
    fn action(&mut self, a: Action<'_>) {
        match a {
            Action::Send {
                channel,
                data,
                tx_token,
            } => {
                let before = mono_ns();
                let (sock, is_event) = match channel {
                    Channel::Event => (self.event, true),
                    Channel::General => (self.general, false),
                };
                match sock.send(data) {
                    Ok(()) => {
                        if is_event {
                            self.tx.record(tx_token, before);
                        }
                    }
                    Err(_) => {
                        self.status.send_would_block.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Action::ClockUpdate(s) => self.publisher.publish(&s),
            Action::WakeAt(_) => {}
            Action::StateChanged { from, to } => {
                self.emit(RunnerEventKind::StateChanged { from, to });
            }
            Action::Event(e) => self.emit(RunnerEventKind::Port(e)),
        }
    }
}

/// A configured, not yet running PTP runner.
#[derive(Debug)]
pub struct Runner<P: ClockPublisher + Send + 'static> {
    cfg: RunnerConfig,
    event: PtpSocket,
    general: PtpSocket,
    phc: Option<Phc>,
    caps: Option<TsCaps>,
    hw_active: bool,
    publisher: P,
    status: Arc<SharedStatus>,
}

/// A running runner thread.
#[derive(Debug)]
pub struct RunnerHandle {
    thread: Option<JoinHandle<RunnerFinal>>,
    stop: Arc<AtomicBool>,
    wake: Arc<EventFd>,
    status: Arc<SharedStatus>,
    events: rtrb::Consumer<RunnerEvent>,
    commands: rtrb::Producer<RunnerCommand>,
}

impl<P: ClockPublisher + Send + 'static> Runner<P> {
    /// Opens the sockets, detects the timestamping capability and prepares the
    /// runner.
    ///
    /// # Errors
    /// Socket, multicast, hardware-timestamping or (for
    /// [`TimestampMode::Hardware`]) capability failures.
    pub fn new(cfg: RunnerConfig, publisher: P) -> io::Result<Self> {
        let caps = query_ts_caps(&cfg.iface).ok();
        let hw_usable = caps.is_some_and(|c| c.hardware_usable());
        let status = Arc::new(SharedStatus::default());
        let mut want_hw = match cfg.timestamping {
            TimestampMode::Software => false,
            TimestampMode::Auto => hw_usable,
            TimestampMode::Hardware => {
                if !hw_usable {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "hardware timestamping requested but the interface cannot stamp PTPv1 or has no PHC",
                    ));
                }
                true
            }
        };
        // The PHC is opened whenever the NIC could use it, even in software mode,
        // so a later live switch to hardware stamping needs no allocation.
        let phc = if hw_usable {
            caps.and_then(|c| c.phc_index)
                .and_then(|i| Phc::open(i).ok())
        } else {
            None
        };
        if want_hw && phc.is_none() {
            if cfg.timestamping == TimestampMode::Hardware {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cannot open the PHC device",
                ));
            }
            want_hw = false;
        }
        let sc = |dscp, hardware| SocketConfig {
            iface: cfg.iface.clone(),
            local_addr: cfg.local_addr,
            dscp,
            multicast_loop: cfg.multicast_loop,
            hardware,
        };
        let event = PtpSocket::open(&sc(cfg.dscp_event, want_hw), true)?;
        let general = PtpSocket::open(&sc(cfg.dscp_general, false), false)?;
        if let Some(c) = caps.filter(|_| want_hw) {
            if let Err(e) = enable_hw_timestamping(event.fd().as_raw_fd(), &cfg.iface, &c) {
                if cfg.timestamping == TimestampMode::Hardware {
                    return Err(e);
                }
                want_hw = false; // fall back to software stamps
                event.set_hardware(false)?;
            }
        }
        status
            .hardware_timestamping
            .store(u64::from(want_hw), Ordering::Relaxed);
        Ok(Runner {
            cfg,
            event,
            general,
            phc,
            caps,
            hw_active: want_hw,
            publisher,
            status,
        })
    }

    /// The shared status block.
    #[must_use]
    pub fn status(&self) -> Arc<SharedStatus> {
        Arc::clone(&self.status)
    }

    /// Starts the thread.
    ///
    /// # Errors
    /// The thread or its wake-up descriptor could not be created.
    pub fn spawn(self) -> io::Result<RunnerHandle> {
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(EventFd::from_flags(EfdFlags::EFD_NONBLOCK).map_err(io::Error::from)?);
        let (producer, consumer) = rtrb::RingBuffer::new(EVENT_RING);
        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::new(COMMAND_RING);
        let status = Arc::clone(&self.status);
        let (stop2, wake2) = (Arc::clone(&stop), Arc::clone(&wake));
        let thread = std::thread::Builder::new()
            .name("tessera-ptp".into())
            .spawn(move || {
                // A panic on the PTP thread must not leave the overlay showing
                // a stale state: abort the process (ADR 0004) so the service
                // manager restarts it.
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.run(&stop2, &wake2, producer, cmd_rx)
                }));
                r.unwrap_or_else(|_| std::process::abort())
            })?;
        Ok(RunnerHandle {
            thread: Some(thread),
            stop,
            wake,
            status,
            events: consumer,
            commands: cmd_tx,
        })
    }

    fn apply_thread_settings(&self) {
        if let Some(prio) = self.cfg.rt_priority {
            let param = libc::sched_param {
                sched_priority: libc::c_int::from(prio),
            };
            // SAFETY: `param` is a valid `sched_param`; pid 0 is the calling
            // thread. Failure (no privilege/RLIMIT_RTPRIO) is reported below.
            let rc = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &raw const param) };
            if rc != 0 {
                self.status.rt_degraded.store(1, Ordering::Relaxed);
            }
        }
        if let Some(cpu) = self.cfg.cpu {
            let mut set = nix::sched::CpuSet::new();
            if set.set(cpu).is_err()
                || nix::sched::sched_setaffinity(nix::unistd::Pid::from_raw(0), &set).is_err()
            {
                self.status.rt_degraded.store(1, Ordering::Relaxed);
            }
        }
    }

    fn run(
        self,
        stop: &AtomicBool,
        wake: &EventFd,
        mut events: rtrb::Producer<RunnerEvent>,
        mut commands: rtrb::Consumer<RunnerCommand>,
    ) -> RunnerFinal {
        self.apply_thread_settings();
        let mut port: Port<8> = Port::new(self.cfg.port, mono_ns());
        let mut conv = RealtimeToMono::new(self.cfg.realtime_step_threshold_ns);
        let mut tx = TxSlots::new();
        let mut dropped_events = 0u64;
        let mut buf = [0u8; 2048];
        let status = Arc::clone(&self.status);
        let mut hw = self.hw_active;
        let Runner {
            event,
            general,
            phc,
            caps,
            mut publisher,
            cfg,
            ..
        } = self;
        {
            let mut sink = IoSink {
                event: &event,
                general: &general,
                publisher: &mut publisher,
                status: &status,
                tx: &mut tx,
                events: &mut events,
                dropped_events: &mut dropped_events,
            };
            port.start(mono_ns(), &mut sink);
        }
        let ev_fd = event.fd();
        let gen_fd = general.fd();
        let wake_fd = wake.as_fd();
        let mut deadline_stuck = false;
        while !stop.load(Ordering::Relaxed) {
            // Sleep until the next timer, a packet, an error-queue entry or a stop.
            let now = mono_ns();
            let mut sleep = port
                .next_deadline()
                .map_or(cfg.max_poll_ns, |d| d.saturating_sub(now))
                .min(cfg.max_poll_ns);
            if deadline_stuck {
                // A timer that stays due after being run would spin a
                // SCHED_FIFO thread; back off (and count it) instead.
                sleep = sleep.max(MIN_STUCK_SLEEP_NS);
            }
            let mut fds = [
                PollFd::new(ev_fd, PollFlags::POLLIN | PollFlags::POLLERR),
                PollFd::new(gen_fd, PollFlags::POLLIN),
                PollFd::new(wake_fd, PollFlags::POLLIN),
            ];
            let to = TimeSpec::new((sleep / 1_000_000_000) as _, (sleep % 1_000_000_000) as _);
            let _ = ppoll(&mut fds, Some(to), None);
            status.loops.fetch_add(1, Ordering::Relaxed);
            if fds
                .get(2)
                .and_then(PollFd::revents)
                .is_some_and(|e| e.contains(PollFlags::POLLIN))
            {
                let _ = wake.read(); // consume the wake-up (commands or stop)
            }

            // Live commands: applied here, between packets, on this thread.
            {
                let mut sink = IoSink {
                    event: &event,
                    general: &general,
                    publisher: &mut publisher,
                    status: &status,
                    tx: &mut tx,
                    events: &mut events,
                    dropped_events: &mut dropped_events,
                };
                while let Ok(cmd) = commands.pop() {
                    let ok = match cmd.port_command() {
                        Some(pc) => {
                            port.apply_command(mono_ns(), pc, &mut sink);
                            true
                        }
                        None => match cmd {
                            RunnerCommand::SetTimestamping(mode) => change_timestamping(
                                mode,
                                caps.as_ref(),
                                phc.is_some(),
                                &event,
                                &cfg.iface,
                                &mut hw,
                                sink.tx,
                            ),
                            _ => false,
                        },
                    };
                    let counter = if ok {
                        &status.commands_applied
                    } else {
                        &status.commands_rejected
                    };
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                status
                    .hardware_timestamping
                    .store(u64::from(hw), Ordering::Relaxed);
            }

            let ok = conv.refresh();
            if !ok {
                status.realtime_steps.fetch_add(1, Ordering::Relaxed);
            }
            let phc_pair = if hw {
                phc.as_ref().and_then(|p| p.read_pair().ok())
            } else {
                None
            };
            let to_mono = |s: &Stamps| -> Option<u64> {
                if hw {
                    if let (Some(h), Some((real_mid, phc_mid))) = (s.hardware_ns, phc_pair) {
                        let real = i128::from(h) + i128::from(real_mid) - i128::from(phc_mid);
                        return conv.convert(u64::try_from(real).ok()?);
                    }
                }
                conv.convert(s.software_ns?)
            };
            let mut sink = IoSink {
                event: &event,
                general: &general,
                publisher: &mut publisher,
                status: &status,
                tx: &mut tx,
                events: &mut events,
                dropped_events: &mut dropped_events,
            };
            if !ok {
                sink.emit(RunnerEventKind::RealtimeStep);
            }

            for _ in 0..RX_BATCH {
                match event.recv(&mut buf, false) {
                    Ok(Some(Received::Data { len, stamps })) => {
                        let rx = to_mono(&stamps);
                        if rx.is_none() {
                            status.rx_timestamp_missing.fetch_add(1, Ordering::Relaxed);
                        }
                        let data = buf.get(..len).unwrap_or(&[]);
                        port.handle_packet(mono_ns(), data, rx, &mut sink);
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
            for _ in 0..RX_BATCH {
                match event.recv(&mut buf, true) {
                    Ok(Some(Received::TxStamp { id, len, stamps })) => {
                        let key = buf.get(..len).and_then(find_event_key);
                        if let Some(slot) = sink.tx.take(key, id) {
                            if let Some(tok) = slot.token {
                                let ts = to_mono(&stamps);
                                if ts.is_some() {
                                    status.tx_timestamps.fetch_add(1, Ordering::Relaxed);
                                }
                                port.handle_tx_timestamp(mono_ns(), tok, ts, &mut sink);
                            }
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
            for _ in 0..RX_BATCH {
                match general.recv(&mut buf, false) {
                    Ok(Some(Received::Data { len, .. })) => {
                        let data = buf.get(..len).unwrap_or(&[]);
                        port.handle_packet(mono_ns(), data, None, &mut sink);
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
            // TX stamps that never arrived: use the send time (counted).
            let now = mono_ns();
            while let Some(slot) = sink.tx.take_expired(now, cfg.tx_stamp_wait_ns) {
                if let Some(tok) = slot.token {
                    status
                        .tx_timestamp_fallbacks
                        .fetch_add(1, Ordering::Relaxed);
                    port.handle_tx_timestamp(now, tok, Some(slot.sent_mono), &mut sink);
                }
            }
            deadline_stuck = false;
            if port.next_deadline().is_some_and(|d| now >= d) {
                port.handle_timeout(now, &mut sink);
                if port.next_deadline().is_some_and(|d| d <= now) {
                    deadline_stuck = true;
                    status.deadline_stuck.fetch_add(1, Ordering::Relaxed);
                }
            }
            status.update(&port);
        }
        // Graceful stop: one final overlay state (FreeRunning, mapping and
        // epoch unchanged) so clients never keep seeing a stale Locked/Leader.
        let now = mono_ns();
        {
            let mut sink = IoSink {
                event: &event,
                general: &general,
                publisher: &mut publisher,
                status: &status,
                tx: &mut tx,
                events: &mut events,
                dropped_events: &mut dropped_events,
            };
            port.stop(now, &mut sink);
        }
        status.update(&port);
        RunnerFinal {
            snapshot: port.snapshot(now),
            stats: *port.stats(),
        }
    }
}

/// Switches the timestamping mode of the running event socket. Returns false
/// (and changes nothing) if the requested mode is not possible.
fn change_timestamping(
    mode: TimestampMode,
    caps: Option<&TsCaps>,
    phc_present: bool,
    event: &PtpSocket,
    iface: &str,
    hw: &mut bool,
    tx: &mut TxSlots,
) -> bool {
    let usable = phc_present && caps.is_some_and(TsCaps::hardware_usable);
    let want = match mode {
        TimestampMode::Software => false,
        TimestampMode::Auto => usable,
        TimestampMode::Hardware => {
            if !usable {
                return false;
            }
            true
        }
    };
    if want == *hw {
        return true;
    }
    if want {
        let Some(c) = caps else {
            return false;
        };
        if enable_hw_timestamping(event.fd().as_raw_fd(), iface, c).is_err()
            || event.set_hardware(true).is_err()
        {
            return false;
        }
    } else if event.set_hardware(false).is_err() {
        return false;
    }
    *hw = want;
    // Stamps still in flight belong to the other time base.
    tx.clear();
    true
}

impl RunnerHandle {
    /// The shared status block.
    #[must_use]
    pub fn status(&self) -> &Arc<SharedStatus> {
        &self.status
    }

    /// Pops the oldest pending event.
    pub fn pop_event(&mut self) -> Option<RunnerEvent> {
        self.events.pop().ok()
    }

    /// Sends a live command to the runner thread (lock-free; never blocks).
    ///
    /// # Errors
    /// [`CommandError::Full`] if the ring of 64 pending commands is full,
    /// [`CommandError::Stopped`] if the thread has exited.
    pub fn send(&mut self, cmd: RunnerCommand) -> Result<(), CommandError> {
        if self.thread.as_ref().is_none_or(JoinHandle::is_finished) {
            return Err(CommandError::Stopped);
        }
        self.commands.push(cmd).map_err(|_| CommandError::Full)?;
        let _ = self.wake.write(1);
        Ok(())
    }

    /// Stops the thread gracefully and waits for it. The runner publishes one
    /// final overlay state (`FreeRunning`, mapping and epoch unchanged) before
    /// it exits; the returned [`RunnerFinal`] repeats it. `None` only if the
    /// thread was already gone.
    pub fn stop(mut self) -> Option<RunnerFinal> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Option<RunnerFinal> {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.wake.write(1);
        self.thread.take().and_then(|t| t.join().ok())
    }
}

impl Drop for RunnerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
