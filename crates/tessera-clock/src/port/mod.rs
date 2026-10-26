//! The PTPv1 port state machine (sans-IO).
//!
//! States follow IEEE 1588-2002 §9.2 for a single-port ordinary clock:
//! INITIALIZING, LISTENING, UNCALIBRATED, SLAVE, MASTER, PASSIVE. Inputs are
//! `handle_packet` (bytes + RX timestamp + now), `handle_tx_timestamp` and
//! `handle_timeout`; outputs are [`Action`]s given to a caller-supplied
//! [`ActionSink`] (no heap, so the steady-state path is allocation-free).
//!
//! All timestamps are local monotonic nanoseconds; converting kernel
//! timestamps is the platform layer's job.
//!
//! Provisional behaviour (UNKNOWN for Dante, see `PROGRESS.md`): election
//! rules, sync/Delay_Req intervals, one- vs two-step, Follow_Up matching,
//! the values written in our messages. All are in [`PortConfig`].

mod delay;
mod follower;
mod leader;

use crate::action::{Action, ActionSink, Channel, PortEvent};
use crate::bmc::{ClockDataset, ForeignMaster, ForeignMasters, QualifyParams};
use crate::command::PortCommand;
use crate::config::{PortConfig, Role, SubdomainPolicy};
use crate::media_clock::{ClockState, MediaClock, MediaClockSnapshot, gm_identity_from_uuid};
use crate::servo::Servo;
use crate::time::{LocalNs, MediaNs, interval_ns};
use crate::wire::{MAX_TX_LEN, Message, ParseErrorKind, PortIdentity, SyncBody};
use delay::DelayFilter;

pub use crate::action::TxToken;

/// Slots for Syncs awaiting their Follow_Up (or vice versa).
const PENDING_SYNCS: usize = 4;

/// IEEE 1588-2002 port states (§9.2). DISABLED and FAULTY are not modelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortState {
    /// Before [`Port::start`].
    Initializing,
    /// Waiting for a qualified leader (or for the listen timeout).
    Listening,
    /// A parent is selected but the servo is not calibrated yet.
    Uncalibrated,
    /// Following the parent.
    Slave,
    /// Leading.
    Master,
    /// Another, better clock leads and we must not (stratum 1 clocks only).
    Passive,
}

/// Counters for status/metrics (ADR 0009). Plain `u64`s; the platform layer
/// mirrors them into atomics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortStats {
    /// Datagrams offered to `handle_packet`.
    pub rx_packets: u64,
    /// Datagrams that failed to parse.
    pub parse_errors: u64,
    /// Datagrams that were not PTPv1 (e.g. PTPv2).
    pub not_v1: u64,
    /// Messages dropped for a foreign subdomain.
    pub wrong_subdomain: u64,
    /// Our own multicast loopback.
    pub own_packets: u64,
    /// Management messages (ignored).
    pub management_ignored: u64,
    /// Syncs received.
    pub sync_rx: u64,
    /// Follow_Ups received.
    pub follow_up_rx: u64,
    /// Delay_Resps received.
    pub delay_resp_rx: u64,
    /// Delay_Reqs received.
    pub delay_req_rx: u64,
    /// Event messages without an RX timestamp (unusable for timing).
    pub missing_rx_timestamp: u64,
    /// Syncs sent.
    pub sync_tx: u64,
    /// Follow_Ups sent.
    pub follow_up_tx: u64,
    /// Delay_Reqs sent.
    pub delay_req_tx: u64,
    /// Delay_Resps sent.
    pub delay_resp_tx: u64,
    /// Offset samples fed to the servo.
    pub samples: u64,
    /// Observations held back while waiting for the first path delay.
    pub samples_waiting_for_delay: u64,
    /// Samples dropped as out of order.
    pub samples_out_of_order: u64,
    /// Samples skipped by the outlier filter.
    pub samples_rejected: u64,
    /// Media-clock steps.
    pub steps: u64,
    /// Path-delay measurements accepted.
    pub delay_samples: u64,
    /// Path-delay measurements dropped (negative or implausible).
    pub delay_rejected: u64,
    /// TX timestamps that never arrived.
    pub tx_timestamp_timeouts: u64,
    /// Follow_Ups sent from the send time because no TX timestamp arrived.
    pub tx_timestamp_fallbacks: u64,
    /// Parent selections.
    pub parent_changes: u64,
    /// Parent timeouts.
    pub parent_lost: u64,
    /// Wire timestamps with nanoseconds >= 10^9 (dropped).
    pub bad_timestamps: u64,
}

/// The parent (the leader being followed).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Parent {
    pub(crate) source: PortIdentity,
    pub(crate) dataset: ClockDataset,
    pub(crate) body: SyncBody,
    pub(crate) last_rx: LocalNs,
}

/// A PTPv1 port. `N` is the foreign-master table capacity (spec:4: 8).
#[derive(Debug)]
pub struct Port<const N: usize = 8> {
    pub(crate) cfg: PortConfig,
    pub(crate) state: PortState,
    pub(crate) clock: MediaClock,
    pub(crate) servo: Servo,
    pub(crate) foreign: ForeignMasters<N>,
    pub(crate) parent: Option<Parent>,
    // follower
    pub(crate) pending: [follower::PendingSync; PENDING_SYNCS],
    pub(crate) pending_next: usize,
    pub(crate) pairs: [Option<follower::SyncPair>; 2],
    pub(crate) last_sample_t2: LocalNs,
    pub(crate) dreq: Option<follower::PendingDelayReq>,
    pub(crate) dreq_seq: u16,
    pub(crate) next_dreq_at: Option<LocalNs>,
    pub(crate) delay: DelayFilter,
    /// Sync observations held back for the current parent while waiting for
    /// its first path delay (per parent; reset on every parent change).
    pub(crate) samples_waiting: u8,
    // leader
    pub(crate) sync_seq: u16,
    pub(crate) next_sync_at: Option<LocalNs>,
    pub(crate) tx_pending: [Option<leader::PendingSyncTx>; 2],
    // timers and publication
    pub(crate) listen_deadline: Option<LocalNs>,
    pub(crate) holdover_deadline: Option<LocalNs>,
    pub(crate) free_running: bool,
    pub(crate) next_bmc_at: Option<LocalNs>,
    pub(crate) next_publish_at: Option<LocalNs>,
    pub(crate) gate_release: Option<LocalNs>,
    pub(crate) last_published: Option<MediaClockSnapshot>,
    pub(crate) dirty: bool,
    pub(crate) last_locked: bool,
    rng: u64,
    pub(crate) stats: PortStats,
}

impl<const N: usize> Port<N> {
    /// Creates a port in INITIALIZING; call [`Port::start`] to begin.
    #[must_use]
    pub fn new(cfg: PortConfig, now: LocalNs) -> Self {
        let initial = cfg.initial_media_ns.unwrap_or(now);
        // The mapping is anchored where `initial` was read, not where the
        // port happens to be built.
        let anchor = cfg.initial_local_ns.map_or(now, |t| t.min(now));
        let mut clock = MediaClock::new(anchor, initial);
        clock.set_rate(anchor, cfg.initial_rate_ppb);
        Port {
            cfg,
            state: PortState::Initializing,
            clock,
            servo: Servo::new(cfg.servo),
            foreign: ForeignMasters::new(),
            parent: None,
            pending: [follower::PendingSync::EMPTY; PENDING_SYNCS],
            pending_next: 0,
            pairs: [None; 2],
            last_sample_t2: 0,
            dreq: None,
            dreq_seq: 0,
            next_dreq_at: None,
            delay: DelayFilter::new(),
            samples_waiting: 0,
            sync_seq: 0,
            next_sync_at: None,
            tx_pending: [None; 2],
            listen_deadline: None,
            holdover_deadline: None,
            free_running: false,
            next_bmc_at: None,
            next_publish_at: None,
            gate_release: None,
            last_published: None,
            dirty: false,
            last_locked: false,
            rng: cfg.rng_seed | 1,
            stats: PortStats::default(),
        }
    }

    /// INITIALIZING -> LISTENING (or MASTER for [`Role::Leader`]).
    pub fn start<S: ActionSink + ?Sized>(&mut self, now: LocalNs, sink: &mut S) {
        if self.state != PortState::Initializing {
            return;
        }
        self.set_state(PortState::Listening, sink);
        self.listen_deadline = Some(now.saturating_add(self.listen_ns()));
        self.next_bmc_at = Some(now.saturating_add(self.sync_interval_ns()));
        self.next_publish_at = Some(now);
        self.dirty = true;
        if self.cfg.role == Role::Leader {
            self.enter_master(now, sink);
        }
        self.finish(now, sink);
    }

    /// Applies a live command (role, advertised clock attributes, publish
    /// guard) and re-runs the election; see [`PortCommand`]. Never steps media
    /// time; the epoch changes only if the servo later steps.
    pub fn apply_command<S: ActionSink + ?Sized>(
        &mut self,
        now: LocalNs,
        cmd: PortCommand,
        sink: &mut S,
    ) {
        match cmd {
            PortCommand::SetRole(role) => {
                if self.cfg.role != role {
                    self.cfg.role = role;
                    if self.state != PortState::Initializing {
                        self.reevaluate_role(now, sink);
                    }
                }
            }
            PortCommand::SetLocalClock(local) => {
                if self.cfg.local != local {
                    self.cfg.local = local;
                    if self.state != PortState::Initializing {
                        self.run_bmc(now, sink);
                    }
                }
            }
            PortCommand::SetPublishGuard {
                threshold_ns,
                hold_ns,
            } => {
                self.cfg.publish_guard_threshold_ns = threshold_ns;
                self.cfg.publish_guard_hold_ns = hold_ns;
                if hold_ns == 0 && self.gate_release.take().is_some() {
                    self.dirty = true; // release whatever was held
                }
            }
        }
        if self.state != PortState::Initializing {
            self.finish(now, sink);
        }
    }

    /// Stops the port: a leader stops sending, the state becomes
    /// INITIALIZING (all later input is ignored) and one final snapshot is
    /// published with state `FreeRunning` and the mapping, rate and epoch
    /// unchanged, so that consumers never keep seeing a stale `Locked` or
    /// `Leader`. A held (guarded) snapshot is released.
    pub fn stop<S: ActionSink + ?Sized>(&mut self, now: LocalNs, sink: &mut S) {
        if self.state == PortState::Initializing {
            return;
        }
        self.leave_master();
        self.set_state(PortState::Initializing, sink);
        self.clock.set_state(ClockState::FreeRunning);
        self.gate_release = None;
        self.publish(now, sink);
    }

    /// A role change on a running port.
    fn reevaluate_role<S: ActionSink + ?Sized>(&mut self, now: LocalNs, sink: &mut S) {
        match self.cfg.role {
            Role::Leader => self.enter_master(now, sink),
            Role::Follower | Role::Auto => {
                if self.cfg.role == Role::Follower && self.state == PortState::Master {
                    // Demotion: stop leading, keep the mapping and its rate.
                    self.leave_master();
                    self.set_state(PortState::Listening, sink);
                    self.listen_deadline = Some(now.saturating_add(self.listen_ns()));
                    self.holdover_deadline = Some(now.saturating_add(self.cfg.holdover_ns));
                    self.free_running = false;
                    self.dirty = true;
                }
                if self.cfg.role == Role::Auto
                    && self.state == PortState::Listening
                    && self.listen_deadline.is_none()
                {
                    // A node that could not lead until now waits one listen
                    // period for a better master before it leads.
                    self.listen_deadline = Some(now.saturating_add(self.listen_ns()));
                }
                self.run_bmc(now, sink);
            }
        }
    }

    /// Processes one received datagram. `rx_ts` is the wire RX timestamp in
    /// local monotonic ns (needed for Sync and Delay_Req; `None` if the
    /// platform could not obtain one).
    pub fn handle_packet<S: ActionSink + ?Sized>(
        &mut self,
        now: LocalNs,
        data: &[u8],
        rx_ts: Option<LocalNs>,
        sink: &mut S,
    ) {
        if self.state == PortState::Initializing {
            return;
        }
        self.stats.rx_packets += 1;
        match Message::parse(data) {
            Err(e) => {
                if matches!(e.kind, ParseErrorKind::NotPtpV1(_)) {
                    self.stats.not_v1 += 1;
                } else {
                    self.stats.parse_errors += 1;
                }
            }
            Ok(m) => self.dispatch(now, &m, rx_ts, sink),
        }
        self.finish(now, sink);
    }

    /// Delivers the TX timestamp of a message sent with a [`TxToken`], or
    /// `None` if the platform failed to obtain one.
    pub fn handle_tx_timestamp<S: ActionSink + ?Sized>(
        &mut self,
        now: LocalNs,
        token: TxToken,
        ts: Option<LocalNs>,
        sink: &mut S,
    ) {
        if token.is_sync {
            self.leader_tx_timestamp(now, token.sequence_id, ts, sink);
        } else {
            self.follower_tx_timestamp(token.sequence_id, ts);
        }
        self.finish(now, sink);
    }

    /// Runs every timer that is due. Call when `now` reaches the last
    /// [`Action::WakeAt`].
    pub fn handle_timeout<S: ActionSink + ?Sized>(&mut self, now: LocalNs, sink: &mut S) {
        if self.state == PortState::Initializing {
            return;
        }
        self.expire_foreign(now);
        self.timers_state(now, sink);
        self.timers_follower(now, sink);
        self.timers_leader(now, sink);
        if self.next_bmc_at.is_some_and(|t| now >= t) {
            self.next_bmc_at = Some(now.saturating_add(self.sync_interval_ns()));
            self.run_bmc(now, sink);
        }
        if self.holdover_deadline.is_some_and(|t| now >= t) {
            self.holdover_deadline = None;
            self.free_running = true;
        }
        if self.gate_release.is_some_and(|t| now >= t) {
            self.dirty = true;
        }
        if self.next_publish_at.is_some_and(|t| now >= t) {
            self.dirty = true;
        }
        self.finish(now, sink);
    }

    /// The earliest time at which [`Port::handle_timeout`] has work to do.
    #[must_use]
    pub fn next_deadline(&self) -> Option<LocalNs> {
        let mut d: Option<LocalNs> = None;
        let mut add = |t: Option<LocalNs>| {
            if let Some(t) = t {
                d = Some(d.map_or(t, |x| x.min(t)));
            }
        };
        if self.state == PortState::Initializing {
            return None;
        }
        add(self.listen_deadline);
        add(self.holdover_deadline);
        add(self.next_bmc_at);
        add(self.next_publish_at);
        add(self.gate_release);
        add(self.follower_deadline());
        add(self.leader_deadline());
        d
    }

    /// Current port state.
    #[must_use]
    pub fn state(&self) -> PortState {
        self.state
    }

    /// The media clock model.
    #[must_use]
    pub fn clock(&self) -> &MediaClock {
        &self.clock
    }

    /// The media clock snapshot as it would be published at `now`.
    #[must_use]
    pub fn snapshot(&self, now: LocalNs) -> MediaClockSnapshot {
        self.clock.snapshot(now)
    }

    /// Media time at local time `t`.
    #[must_use]
    pub fn media_ns_at(&self, t: LocalNs) -> MediaNs {
        self.clock.media_ns_at(t)
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> &PortStats {
        &self.stats
    }

    /// The servo.
    #[must_use]
    pub fn servo(&self) -> &Servo {
        &self.servo
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &PortConfig {
        &self.cfg
    }

    /// The identity of the parent port, if following.
    #[must_use]
    pub fn parent(&self) -> Option<PortIdentity> {
        self.parent.map(|p| p.source)
    }

    /// The filtered one-way path delay in ns, once measured.
    #[must_use]
    pub fn path_delay_ns(&self) -> Option<i64> {
        self.delay.value()
    }

    /// Foreign masters currently tracked.
    #[must_use]
    pub fn foreign_master_count(&self) -> usize {
        self.foreign.len()
    }

    // ---------------------------------------------------------------- helpers

    pub(crate) fn sync_interval_ns(&self) -> u64 {
        interval_ns(self.cfg.sync_interval_log2)
    }

    /// Interval used for timeouts: the configured one, widened to what a
    /// sender advertises if that is sane (`-8..=8` log2 s).
    pub(crate) fn effective_interval_ns(&self, advertised: i8) -> u64 {
        let cfg = self.sync_interval_ns();
        if (-8..=8).contains(&advertised) {
            cfg.max(interval_ns(advertised))
        } else {
            cfg
        }
    }

    fn listen_ns(&self) -> u64 {
        self.sync_interval_ns()
            .saturating_mul(u64::from(self.cfg.listen_intervals))
    }

    pub(crate) fn window_ns(&self, advertised: i8) -> u64 {
        self.effective_interval_ns(advertised)
            .saturating_mul(u64::from(self.cfg.foreign_master_window))
    }

    pub(crate) fn rand_below(&mut self, bound: u64) -> u64 {
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        let r = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        if bound == 0 { 0 } else { r % bound }
    }

    pub(crate) fn own_dataset(&self) -> ClockDataset {
        let l = &self.cfg.local;
        ClockDataset {
            preferred: l.preferred,
            stratum: l.stratum,
            identifier: l.identifier,
            variance: l.variance,
            is_boundary_clock: l.is_boundary_clock,
            uuid: self.cfg.identity.uuid,
            steps_removed: 0,
        }
    }

    pub(crate) fn set_state<S: ActionSink + ?Sized>(&mut self, to: PortState, sink: &mut S) {
        if self.state != to {
            let from = self.state;
            self.state = to;
            sink.action(Action::StateChanged { from, to });
        }
    }

    pub(crate) fn send<S: ActionSink + ?Sized>(
        &self,
        sink: &mut S,
        channel: Channel,
        msg: &Message<'_>,
        tx_token: Option<TxToken>,
    ) {
        let mut buf = [0u8; MAX_TX_LEN];
        if let Ok(n) = msg.encode(&mut buf) {
            if let Some(data) = buf.get(..n) {
                sink.action(Action::Send {
                    channel,
                    data,
                    tx_token,
                });
            }
        }
    }

    // --------------------------------------------------------------- dispatch

    fn dispatch<S: ActionSink + ?Sized>(
        &mut self,
        now: LocalNs,
        m: &Message<'_>,
        rx_ts: Option<LocalNs>,
        sink: &mut S,
    ) {
        let header = m.header();
        if self.cfg.subdomain_policy == SubdomainPolicy::Strict
            && header.subdomain != self.cfg.subdomain
        {
            self.stats.wrong_subdomain += 1;
            return;
        }
        if header.source.same_port(&self.cfg.identity) {
            self.stats.own_packets += 1;
            return;
        }
        match m {
            Message::Sync { header, body } => self.on_sync(now, header, body, rx_ts, sink),
            Message::DelayReq { header, .. } => self.on_delay_req(now, header, rx_ts, sink),
            Message::FollowUp { header, body } => self.on_follow_up(now, header, body, sink),
            Message::DelayResp { header, body } => self.on_delay_resp(header, body),
            Message::Management { .. } => self.stats.management_ignored += 1,
        }
    }

    fn on_sync<S: ActionSink + ?Sized>(
        &mut self,
        now: LocalNs,
        header: &crate::wire::Header,
        body: &SyncBody,
        rx_ts: Option<LocalNs>,
        sink: &mut S,
    ) {
        self.stats.sync_rx += 1;
        let params = QualifyParams {
            window_ns: self.window_ns(body.sync_interval),
            threshold: self.cfg.foreign_master_threshold,
            min_spacing_ns: self.effective_interval_ns(body.sync_interval) / 2,
        };
        let protect = self.parent.map(|p| p.source);
        if self
            .foreign
            .update(now, &params, protect.as_ref(), header, body)
            .is_none()
        {
            return; // duplicate or table failure
        }
        self.run_bmc(now, sink);
        let from_parent = self
            .parent
            .is_some_and(|p| p.source.same_port(&header.source));
        if from_parent && matches!(self.state, PortState::Uncalibrated | PortState::Slave) {
            self.follower_sync(now, header, body, rx_ts, sink);
        }
    }

    fn expire_foreign(&mut self, now: LocalNs) {
        let cfg_interval = self.sync_interval_ns();
        let win = u64::from(self.cfg.foreign_master_window);
        self.foreign.expire(now, |r| {
            let adv = r.body.sync_interval;
            let i = if (-8..=8).contains(&adv) {
                cfg_interval.max(interval_ns(adv))
            } else {
                cfg_interval
            };
            i.saturating_mul(win)
        });
    }

    // -------------------------------------------------------------------- BMC

    /// Re-evaluates the best master and the resulting state.
    pub(crate) fn run_bmc<S: ActionSink + ?Sized>(&mut self, now: LocalNs, sink: &mut S) {
        if self.state == PortState::Initializing || self.cfg.role == Role::Leader {
            return;
        }
        let best = self.foreign.best(self.cfg.bmc).copied();
        let Some(best) = best else {
            if self.state == PortState::Passive {
                self.set_state(PortState::Listening, sink);
                self.listen_deadline = Some(now.saturating_add(self.listen_ns()));
            }
            return;
        };
        let own = self.own_dataset();
        let own_better = self.cfg.role == Role::Auto && (self.cfg.bmc)(&own, &best.dataset).is_lt();
        if own_better {
            if self.state != PortState::Master {
                self.enter_master(now, sink);
            }
        } else if self.cfg.role == Role::Auto && self.cfg.local.stratum <= 1 {
            // A stratum-1 clock never follows (std Figure 27): PASSIVE.
            if self.state != PortState::Passive {
                self.leave_master();
                self.drop_parent_state();
                self.set_state(PortState::Passive, sink);
                self.listen_deadline = None;
            }
        } else if self
            .parent
            .is_some_and(|p| p.source.same_port(&best.sender))
            && matches!(self.state, PortState::Uncalibrated | PortState::Slave)
        {
            // Same parent: keep following (its dataset may have changed).
            if let Some(p) = self.parent.as_mut() {
                p.dataset = best.dataset;
            }
        } else {
            self.select_parent(now, &best, sink);
        }
    }

    fn select_parent<S: ActionSink + ?Sized>(
        &mut self,
        now: LocalNs,
        rec: &ForeignMaster,
        sink: &mut S,
    ) {
        self.leave_master();
        self.drop_parent_state();
        self.parent = Some(Parent {
            source: rec.sender,
            dataset: rec.dataset,
            body: rec.body,
            last_rx: now,
        });
        self.servo.reset();
        self.last_locked = false;
        self.listen_deadline = None;
        self.holdover_deadline = None;
        self.free_running = false;
        self.clock
            .set_gm_identity(gm_identity_from_uuid(&rec.dataset.uuid));
        self.stats.parent_changes += 1;
        self.set_state(PortState::Uncalibrated, sink);
        sink.action(Action::Event(PortEvent::ParentSelected {
            gm_uuid: rec.dataset.uuid,
        }));
        self.dirty = true;
    }

    /// When the clock stops being disciplined (leading, holdover), run it at
    /// the frequency the servo learned rather than the instantaneous rate,
    /// which includes the proportional term. Continuous: no epoch change.
    pub(crate) fn freeze_learned_rate(&mut self, now: LocalNs) {
        if let Some(f) = self.servo.frequency_ppb() {
            let at = now.max(self.clock.anchor_mono_ns());
            self.clock.set_rate(at, f);
            self.dirty = true;
        }
    }

    /// Clears everything that belongs to the previous parent.
    fn drop_parent_state(&mut self) {
        self.parent = None;
        self.pending = [follower::PendingSync::EMPTY; PENDING_SYNCS];
        self.pending_next = 0;
        self.pairs = [None; 2];
        self.last_sample_t2 = 0;
        self.dreq = None;
        self.next_dreq_at = None;
        self.delay.reset();
        self.samples_waiting = 0;
    }

    fn timers_state<S: ActionSink + ?Sized>(&mut self, now: LocalNs, sink: &mut S) {
        match self.state {
            PortState::Listening => {
                if self.listen_deadline.is_some_and(|t| now >= t) {
                    self.listen_deadline = None;
                    if self.cfg.role == Role::Follower {
                        if self.holdover_deadline.is_none() {
                            self.free_running = true;
                        }
                    } else {
                        self.enter_master(now, sink);
                    }
                }
            }
            PortState::Uncalibrated | PortState::Slave => {
                if let Some(p) = self.parent {
                    let timeout = self
                        .effective_interval_ns(p.body.sync_interval)
                        .saturating_mul(u64::from(self.cfg.sync_receipt_timeout));
                    if now.saturating_sub(p.last_rx) >= timeout {
                        self.lose_parent(now, sink);
                    }
                }
            }
            _ => {}
        }
    }

    fn lose_parent<S: ActionSink + ?Sized>(&mut self, now: LocalNs, sink: &mut S) {
        if let Some(p) = self.parent {
            self.foreign.remove(&p.source);
        }
        let had_time = self.servo.is_calibrated();
        self.freeze_learned_rate(now);
        self.drop_parent_state();
        self.stats.parent_lost += 1;
        self.set_state(PortState::Listening, sink);
        self.listen_deadline = Some(now.saturating_add(self.listen_ns()));
        if had_time {
            // Holdover keeps the mapping, so the epoch does not change (ADR 0005).
            self.holdover_deadline = Some(now.saturating_add(self.cfg.holdover_ns));
            self.free_running = false;
        }
        sink.action(Action::Event(PortEvent::ParentLost));
        self.dirty = true;
    }

    // ----------------------------------------------------------- publication

    /// Derives the media-clock state from the port state.
    fn desired_clock_state(&self) -> ClockState {
        match self.state {
            PortState::Initializing => ClockState::FreeRunning,
            PortState::Master => ClockState::Leader,
            PortState::Uncalibrated => ClockState::Acquiring,
            PortState::Slave => {
                if self.servo.is_locked() {
                    ClockState::Locked
                } else {
                    ClockState::Acquiring
                }
            }
            PortState::Listening | PortState::Passive => {
                if self.holdover_deadline.is_some() {
                    ClockState::Holdover
                } else if self.free_running {
                    ClockState::FreeRunning
                } else {
                    ClockState::Listening
                }
            }
        }
    }

    fn finish<S: ActionSink + ?Sized>(&mut self, now: LocalNs, sink: &mut S) {
        if matches!(self.state, PortState::Uncalibrated | PortState::Slave) {
            let locked = self.servo.is_locked();
            if locked != self.last_locked {
                self.last_locked = locked;
                sink.action(Action::Event(PortEvent::LockChanged { locked }));
            }
        }
        let want = self.desired_clock_state();
        if self.clock.state() != want {
            self.clock.set_state(want);
            self.dirty = true;
        }
        if self.dirty {
            self.publish(now, sink);
        }
        if let Some(t) = self.next_deadline() {
            sink.action(Action::WakeAt(t));
        }
    }

    fn publish<S: ActionSink + ?Sized>(&mut self, now: LocalNs, sink: &mut S) {
        let snap = self.clock.snapshot(now);
        if self.cfg.publish_guard_hold_ns > 0 {
            if let Some(last) = self.last_published {
                let a = i128::from(snap.media_ns_at(now));
                let b = i128::from(last.media_ns_at(now));
                let jump =
                    (a - b).unsigned_abs() >= u128::from(self.cfg.publish_guard_threshold_ns);
                match self.gate_release {
                    Some(t) if now < t => {
                        self.next_publish_at =
                            Some(now.saturating_add(self.cfg.publish_interval_ns));
                        return;
                    }
                    Some(_) => {}
                    None if jump => {
                        self.gate_release =
                            Some(now.saturating_add(self.cfg.publish_guard_hold_ns));
                        self.next_publish_at =
                            Some(now.saturating_add(self.cfg.publish_interval_ns));
                        return;
                    }
                    None => {}
                }
            }
        }
        self.gate_release = None;
        self.dirty = false;
        self.last_published = Some(snap);
        self.next_publish_at = Some(now.saturating_add(self.cfg.publish_interval_ns));
        sink.action(Action::ClockUpdate(snap));
    }
}
