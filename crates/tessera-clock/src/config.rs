//! Port configuration. Defaults come from [`crate::provisional`] (standard
//! values or documented guesses); nothing here is Dante-confirmed.

use crate::bmc::{BmcCompare, compare_std2002};
use crate::provisional as p;
use crate::servo::ServoConfig;
use crate::wire::PortIdentity;

/// What the node is allowed to be (`[clock].role` in `docs/configuration.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Slave-only: never transmits Sync, follows the best leader (default;
    /// "we lose elections to real hardware", ADR 0005).
    Follower,
    /// Leader-capable: the best-master comparison decides.
    Auto,
    /// Always the leader, whatever it hears (lab and isolated networks). It
    /// never yields, also not to a preferred clock (spec:5.4: item 1's
    /// yielding applies to role auto only); hearing one while leading is
    /// reported ([`crate::action::PortEvent::PreferredRival`]).
    Leader,
}

/// How received subdomains are treated (spec:1: the reference does not check
/// it; the standard requires a match).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubdomainPolicy {
    /// Ignore messages whose subdomain differs from ours (standard).
    Strict,
    /// Accept any subdomain (what the reference follower does, `likely`).
    AcceptAny,
}

/// How a Follow_Up is matched to its Sync (spec:2.4: the reference uses the
/// header sequence id, the standard the associatedSequenceId).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowUpMatch {
    /// associatedSequenceId only (standard).
    Associated,
    /// Header sequenceId only (reference follower).
    HeaderSequence,
    /// Either; the associated id is preferred.
    Either,
}

/// Whether the leader puts the departure time in the Sync or in a Follow_Up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// PTP_ASSIST + Follow_Up with the TX timestamp (accurate with timestamping).
    TwoStep,
    /// Origin timestamp = media time at the moment of sending (software
    /// estimate; no Follow_Up).
    OneStep,
}

/// The attributes this node advertises about its own clock (spec:2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalClock {
    /// grandmasterPreferred when leading.
    pub preferred: bool,
    /// Stratum (255 = slave-only).
    pub stratum: u8,
    /// Clock identifier.
    pub identifier: [u8; 4],
    /// Scaled log variance.
    pub variance: i16,
    /// Boundary-clock flag.
    pub is_boundary_clock: bool,
    /// currentUtcOffset when `utc_reasonable`.
    pub utc_offset: i16,
    /// Whether `utc_offset` is meaningful.
    pub utc_reasonable: bool,
}

impl LocalClock {
    /// Slave-only values (stratum 255).
    #[must_use]
    pub fn follower() -> Self {
        LocalClock {
            preferred: false,
            stratum: p::FOLLOWER_STRATUM,
            identifier: p::IDENTIFIER_DFLT,
            variance: p::DEFAULT_VARIANCE,
            is_boundary_clock: false,
            utc_offset: 0,
            utc_reasonable: false,
        }
    }

    /// Leader-capable values (stratum [`p::LEADER_STRATUM`]).
    #[must_use]
    pub fn leader_capable() -> Self {
        LocalClock {
            stratum: p::LEADER_STRATUM,
            ..Self::follower()
        }
    }
}

/// Configuration of one PTP port.
#[derive(Debug, Clone, Copy)]
pub struct PortConfig {
    /// Our port identity (uuid normally a NIC MAC).
    pub identity: PortIdentity,
    /// Subdomain, NUL padded to 16 bytes.
    pub subdomain: [u8; 16],
    /// Subdomain filtering.
    pub subdomain_policy: SubdomainPolicy,
    /// Role.
    pub role: Role,
    /// Advertised clock attributes.
    pub local: LocalClock,
    /// Sync interval (log2 s): used when leading and as the expectation when
    /// following. A larger interval advertised by a parent widens timeouts.
    pub sync_interval_log2: i8,
    /// Sync intervals without a Sync before the parent is lost.
    pub sync_receipt_timeout: u8,
    /// Sync intervals a leader-capable port listens before leading.
    pub listen_intervals: u8,
    /// Mean Delay_Req interval (log2 s); each interval is uniform in [0, 2x].
    pub delay_req_interval_log2: i8,
    /// Number of path-delay measurements taken at one per sync interval after
    /// a parent is selected, before the randomised mean interval applies.
    pub delay_req_startup_burst: u8,
    /// Value written into the syncInterval field of our Delay_Req; `None`
    /// copies the parent's last Sync, as followers seen do (spec:5.4 item 4).
    pub delay_req_sync_interval_field: Option<i8>,
    /// Syncs needed to qualify a foreign master.
    pub foreign_master_threshold: u8,
    /// Qualification window in sync intervals.
    pub foreign_master_window: u8,
    /// Dataset comparison.
    pub bmc: BmcCompare,
    /// Leader Sync mode.
    pub sync_mode: SyncMode,
    /// Follow_Up matching.
    pub follow_up_match: FollowUpMatch,
    /// How long to wait for a TX timestamp (leader: then send the Follow_Up
    /// from the send time; follower: abandon the Delay_Req).
    pub tx_timestamp_timeout_ns: u64,
    /// How long to wait for a Delay_Resp.
    pub delay_resp_timeout_ns: u64,
    /// Largest plausible one-way path delay; larger measurements are dropped.
    pub max_path_delay_ns: i64,
    /// Path delay assumed when no Delay_Req/Resp exchange has completed.
    pub initial_delay_ns: i64,
    /// Sync observations held back (not given to the servo) while waiting for
    /// the first path-delay measurement; after this many the servo starts
    /// with [`PortConfig::initial_delay_ns`] (a leader that never answers
    /// Delay_Req must not block synchronisation forever).
    pub max_samples_without_delay: u8,
    /// Known path asymmetry `d(leader->follower) - d(follower->leader)`.
    /// End-to-end measurement cannot detect it; the offset error is half of it.
    pub path_asymmetry_ns: i64,
    /// Media time at construction (a leader passes a TAI reading; `None` =
    /// the local monotonic time, i.e. a clock with offset 0).
    pub initial_media_ns: Option<u64>,
    /// The local monotonic time `initial_media_ns` was read at (`None`: the
    /// time the port is constructed). A port that continues an earlier
    /// mapping is built on a new thread some time after the mapping was read,
    /// longer when the host is busy; anchoring the clock at the instant of the
    /// reading keeps media time exactly continuous whatever that takes, where
    /// anchoring it at construction would step it back by the start-up time
    /// and leave the followers' servos to work that step off. Not after
    /// construction: a later time is taken as the construction time.
    pub initial_local_ns: Option<u64>,
    /// Rate offset (ppb) of the media clock at construction. A leader that
    /// has no discipline runs at this rate against the monotonic clock
    /// (default 0); lab tests set a non-zero value so followers must slew.
    pub initial_rate_ppb: i64,
    /// Servo.
    pub servo: ServoConfig,
    /// How long the media clock extrapolates after losing the leader before
    /// reporting `FreeRunning`.
    pub holdover_ns: u64,
    /// Republish the clock at least this often (overlay liveness).
    pub publish_interval_ns: u64,
    /// Publish guard (spec:8.2): a new snapshot whose media time at "now"
    /// differs from the last published one by at least this much is held back
    /// for [`PortConfig::publish_guard_hold_ns`].
    pub publish_guard_threshold_ns: u64,
    /// Publish-guard hold time. 0 (default) disables holding: consumers
    /// already re-anchor on the epoch change.
    pub publish_guard_hold_ns: u64,
    /// Seed of the Delay_Req interval randomiser.
    pub rng_seed: u64,
}

impl PortConfig {
    fn base(identity: PortIdentity, role: Role, local: LocalClock) -> Self {
        PortConfig {
            identity,
            subdomain: p::DEFAULT_SUBDOMAIN,
            subdomain_policy: SubdomainPolicy::Strict,
            role,
            local,
            sync_interval_log2: p::SYNC_INTERVAL_LOG2,
            sync_receipt_timeout: p::SYNC_RECEIPT_TIMEOUT,
            listen_intervals: p::LISTEN_INTERVALS,
            delay_req_interval_log2: p::DELAY_REQ_INTERVAL_LOG2,
            delay_req_startup_burst: 6,
            delay_req_sync_interval_field: None,
            foreign_master_threshold: p::FOREIGN_MASTER_THRESHOLD,
            foreign_master_window: p::FOREIGN_MASTER_WINDOW,
            bmc: compare_std2002,
            sync_mode: if p::LEADER_TWO_STEP {
                SyncMode::TwoStep
            } else {
                SyncMode::OneStep
            },
            follow_up_match: FollowUpMatch::Either,
            tx_timestamp_timeout_ns: 100_000_000,
            delay_resp_timeout_ns: 1_000_000_000,
            max_path_delay_ns: 50_000_000,
            initial_delay_ns: 0,
            max_samples_without_delay: 6,
            path_asymmetry_ns: 0,
            initial_media_ns: None,
            initial_local_ns: None,
            initial_rate_ppb: 0,
            servo: ServoConfig::default(),
            holdover_ns: 60_000_000_000,
            publish_interval_ns: 1_000_000_000,
            publish_guard_threshold_ns: 1_000_000,
            publish_guard_hold_ns: 0,
            rng_seed: u64::from_be_bytes([
                0,
                0,
                identity.uuid[0],
                identity.uuid[1],
                identity.uuid[2],
                identity.uuid[3],
                identity.uuid[4],
                identity.uuid[5],
            ]) ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// A slave-only port (the default role).
    #[must_use]
    pub fn follower(uuid: [u8; 6]) -> Self {
        Self::base(Self::identity(uuid), Role::Follower, LocalClock::follower())
    }

    /// A leader-capable port: the comparison decides ([`Role::Auto`]).
    #[must_use]
    pub fn auto(uuid: [u8; 6]) -> Self {
        Self::base(
            Self::identity(uuid),
            Role::Auto,
            LocalClock::leader_capable(),
        )
    }

    /// A port that always leads ([`Role::Leader`]).
    #[must_use]
    pub fn leader(uuid: [u8; 6]) -> Self {
        Self::base(
            Self::identity(uuid),
            Role::Leader,
            LocalClock::leader_capable(),
        )
    }

    fn identity(uuid: [u8; 6]) -> PortIdentity {
        PortIdentity {
            comm_tech: p::COMM_TECH_UDP_IPV4,
            uuid,
            port_id: 1,
        }
    }
}
