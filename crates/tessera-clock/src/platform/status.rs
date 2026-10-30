//! Counters and status shared with the control plane through atomics.

use core::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use crate::media_clock::ClockState;
use crate::port::{Port, PortState, PortStats};

/// Lock-free status block written by the runner thread and read by anyone.
/// All fields are plain atomics (relaxed): values are independent gauges and
/// counters, not a consistent snapshot.
#[derive(Debug, Default)]
pub struct SharedStatus {
    /// [`PortState`] as a number, see [`SharedStatus::port_state`].
    pub port_state: AtomicU64,
    /// [`ClockState`] as a number, see [`SharedStatus::clock_state`].
    pub clock_state: AtomicU64,
    /// Media-clock epoch.
    pub epoch: AtomicU64,
    /// Media-clock rate offset, ppb.
    pub rate_ppb: AtomicI64,
    /// Last servo error, ns.
    pub offset_ns: AtomicI64,
    /// Filtered path delay, ns (`i64::MIN` = unknown).
    pub path_delay_ns: AtomicI64,
    /// Datagrams received.
    pub rx_packets: AtomicU64,
    /// Datagrams that failed to parse or were not PTPv1.
    pub rx_dropped: AtomicU64,
    /// Offset samples fed to the servo.
    pub samples: AtomicU64,
    /// Media-clock steps.
    pub steps: AtomicU64,
    /// Syncs sent.
    pub sync_tx: AtomicU64,
    /// Delay_Reqs sent.
    pub delay_req_tx: AtomicU64,
    /// Parent changes.
    pub parent_changes: AtomicU64,
    /// Parent timeouts.
    pub parent_lost: AtomicU64,
    /// Receive timestamps that were missing or could not be converted.
    pub rx_timestamp_missing: AtomicU64,
    /// TX timestamps taken from the error queue.
    pub tx_timestamps: AtomicU64,
    /// TX timestamps replaced by the send time (none arrived in time).
    pub tx_timestamp_fallbacks: AtomicU64,
    /// Realtime-clock steps detected (batches discarded).
    pub realtime_steps: AtomicU64,
    /// Timestamp batches discarded because no sufficiently narrow clock
    /// calibration bracket was available.
    pub timestamp_calibration_rejections: AtomicU64,
    /// Sends dropped because the socket buffer was full.
    pub send_would_block: AtomicU64,
    /// 1 if hardware timestamping is active.
    pub hardware_timestamping: AtomicU64,
    /// 1 if the requested real-time scheduling could not be applied.
    pub rt_degraded: AtomicU64,
    /// Times a timer stayed due right after being run (should stay 0).
    pub deadline_stuck: AtomicU64,
    /// Live commands applied.
    pub commands_applied: AtomicU64,
    /// Live commands that could not be carried out.
    pub commands_rejected: AtomicU64,
    /// Configured role: 0 follower, 1 auto, 2 leader.
    pub role: AtomicU64,
    /// Advertised stratum.
    pub local_stratum: AtomicU64,
    /// Advertised preferred flag (0/1).
    pub local_preferred: AtomicU64,
    /// While leading: the grandmaster uuid (MAC as a big-endian number) of a
    /// foreign clock that advertises itself as preferred
    /// ([`Port::preferred_rival`]); 0 = none.
    pub preferred_rival: AtomicU64,
    /// Publish-guard hold time, ns (0 = guard off).
    pub publish_guard_hold_ns: AtomicU64,
    /// Loop iterations (liveness).
    pub loops: AtomicU64,
}

impl SharedStatus {
    /// The port state.
    #[must_use]
    pub fn port_state(&self) -> PortState {
        match self.port_state.load(Ordering::Relaxed) {
            1 => PortState::Listening,
            2 => PortState::Uncalibrated,
            3 => PortState::Slave,
            4 => PortState::Master,
            5 => PortState::Passive,
            _ => PortState::Initializing,
        }
    }

    /// The media-clock state.
    #[must_use]
    pub fn clock_state(&self) -> ClockState {
        match self.clock_state.load(Ordering::Relaxed) {
            1 => ClockState::Listening,
            2 => ClockState::Acquiring,
            3 => ClockState::Locked,
            4 => ClockState::Holdover,
            5 => ClockState::Leader,
            _ => ClockState::FreeRunning,
        }
    }

    pub(crate) fn update<const N: usize>(&self, port: &Port<N>) {
        let r = Ordering::Relaxed;
        let (state, clock) = (port.state(), port.clock().state());
        let s: &PortStats = port.stats();
        let (epoch, rate) = (port.clock().epoch(), port.clock().rate_ppb());
        let (offset, delay) = (port.servo().last_error_ns(), port.path_delay_ns());
        self.port_state.store(
            match state {
                PortState::Initializing => 0,
                PortState::Listening => 1,
                PortState::Uncalibrated => 2,
                PortState::Slave => 3,
                PortState::Master => 4,
                PortState::Passive => 5,
            },
            r,
        );
        self.clock_state.store(
            match clock {
                ClockState::FreeRunning => 0,
                ClockState::Listening => 1,
                ClockState::Acquiring => 2,
                ClockState::Locked => 3,
                ClockState::Holdover => 4,
                ClockState::Leader => 5,
            },
            r,
        );
        let cfg = port.config();
        self.role.store(
            match cfg.role {
                crate::config::Role::Follower => 0,
                crate::config::Role::Auto => 1,
                crate::config::Role::Leader => 2,
            },
            r,
        );
        let local = port.advertised_local();
        self.local_stratum.store(u64::from(local.stratum), r);
        self.local_preferred.store(u64::from(local.preferred), r);
        self.preferred_rival.store(
            port.preferred_rival().map_or(0, |u| {
                u.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
            }),
            r,
        );
        self.publish_guard_hold_ns
            .store(cfg.publish_guard_hold_ns, r);
        self.epoch.store(epoch, r);
        self.rate_ppb.store(rate, r);
        self.offset_ns.store(offset, r);
        self.path_delay_ns.store(delay.unwrap_or(i64::MIN), r);
        self.rx_packets.store(s.rx_packets, r);
        self.rx_dropped.store(s.parse_errors + s.not_v1, r);
        self.samples.store(s.samples, r);
        self.steps.store(s.steps, r);
        self.sync_tx.store(s.sync_tx, r);
        self.delay_req_tx.store(s.delay_req_tx, r);
        self.parent_changes.store(s.parent_changes, r);
        self.parent_lost.store(s.parent_lost, r);
    }
}
