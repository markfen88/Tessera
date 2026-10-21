//! Every wire-facing or behavioural value that is not `confirmed`.
//!
//! All of these are *defaults* for configuration (see [`crate::config`]);
//! nothing else in the crate hard-codes them. They are provisional pending
//! lab captures (`PROGRESS.md`, "Open spec questions"). Source tags:
//! `spec:` = `spec/protocol/clock-ptp.md` section; `std:` = IEEE 1588-2002.

/// versionPTP of every message. spec:2.1, std §13.3 -- `likely`.
pub const VERSION_PTP: u16 = 1;
/// versionNetwork written in every message. spec:2.1 -- `likely`.
pub const VERSION_NETWORK: u16 = 1;

/// Default subdomain `"_DFLT"`, NUL padded to 16 bytes. spec:1,2.1 --
/// `likely` for the default; whether Dante hardware uses other subdomains is
/// UNKNOWN (capture question P1-Q1).
pub const DEFAULT_SUBDOMAIN: [u8; 16] = *b"_DFLT\0\0\0\0\0\0\0\0\0\0\0";

/// sourceCommunicationTechnology for UDP/IPv4. spec:2.1 -- `likely`.
pub const COMM_TECH_UDP_IPV4: u8 = 1;

/// Default-subdomain multicast group 224.0.1.129, used for event AND general
/// messages in both directions. spec:3, std Annex D -- `likely`.
pub const MULTICAST_GROUP_V4: [u8; 4] = [224, 0, 1, 129];
/// UDP event port. spec:3, std Annex D -- `likely`.
pub const EVENT_PORT: u16 = 319;
/// UDP general port. spec:3, std Annex D -- `likely`.
pub const GENERAL_PORT: u16 = 320;

/// DSCP for event messages. `docs/lab.md` quotes vendor QoS guidance (CS7 =
/// 56) "to be verified"; the spec says the reference sets none -- `guess`.
pub const DSCP_EVENT: u8 = 56;
/// DSCP for general messages (CS1 = 8, `docs/lab.md`) -- `guess`.
pub const DSCP_GENERAL: u8 = 8;

/// syncInterval field value written in Delay_Req (log2 s). The reference's
/// author notes "looks like Dante does it this way" -- spec:2.3 -- `guess`.
pub const DELAY_REQ_SYNC_INTERVAL_FIELD: i8 = 0x7F;

/// Stratum a slave-only node advertises in its own fields. spec:2.3 -- `likely`
/// (255 = slave-only, std Table 7).
pub const FOLLOWER_STRATUM: u8 = 255;
/// Stratum a leader-capable node advertises. 1588-2002 uses 4 for a clock
/// that is not a primary or secondary reference; Dante's values are UNKNOWN
/// (P1-Q3). Chosen so that, by the standard ordering, any stratum 1..=3
/// device wins over us -- `guess`.
pub const LEADER_STRATUM: u8 = 4;
/// Clock identifier of a free-running clock. std Table 7 "DFLT" -- `likely`.
pub const IDENTIFIER_DFLT: [u8; 4] = *b"DFLT";
/// Default clock variance (1588-2002 scaled log variance). Recollection of
/// the value used by public PTPv1 implementations -- `guess`.
pub const DEFAULT_VARIANCE: i16 = -4000;

/// Sync interval, log2 seconds (0 = 1 s). std §7.5 Table 18 -- `guess`
/// (spec:4 notes the previous draft's 2 s was unverified).
pub const SYNC_INTERVAL_LOG2: i8 = 0;
/// Missed sync intervals before the parent is declared lost. The standard's
/// PTP_SYNC_RECEIPT_TIMEOUT is 2 (spec:4, `guess`); 3 is used so that one
/// lost Sync (a gap of two intervals plus jitter) does not drop the parent.
///
pub const SYNC_RECEIPT_TIMEOUT: u8 = 3;
/// Sync intervals a LISTENING leader-capable port waits before it becomes
/// MASTER. Larger than the standard's receipt timeout because qualification
/// needs [`FOREIGN_MASTER_THRESHOLD`] Syncs -- `guess`.
pub const LISTEN_INTERVALS: u8 = 4;
/// Mean Delay_Req interval, log2 seconds (2 = 4 s). std: 2^k sync intervals,
/// randomised in [0, 2x] -- spec:4 -- `guess`.
pub const DELAY_REQ_INTERVAL_LOG2: i8 = 2;
/// Syncs a foreign master must send inside the window to qualify. spec:4,
/// std §9.3 -- `likely`.
pub const FOREIGN_MASTER_THRESHOLD: u8 = 2;
/// Foreign-master qualification window in sync intervals. spec:4 -- `likely`.
pub const FOREIGN_MASTER_WINDOW: u8 = 4;
/// Default capacity of the foreign-master table. spec:4 (reference: 8) --
/// `likely`. It is the const-generic parameter of [`crate::port::Port`].
pub const FOREIGN_MASTER_CAPACITY: usize = 8;

/// Whether the leader sends two-step Syncs (PTP_ASSIST + Follow_Up). What
/// Dante hardware accepts from a leader is UNKNOWN (spec:12) -- `guess`.
pub const LEADER_TWO_STEP: bool = true;
