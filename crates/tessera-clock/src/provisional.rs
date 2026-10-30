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

/// DSCP for event messages (Sync, Delay_Req): CS7, as the hardware leader
/// marks its Syncs and software followers their Delay_Reqs. spec:5.4 item 5
/// -- `confirmed`.
pub const DSCP_EVENT: u8 = 56;
/// DSCP for general messages (Follow_Up, Delay_Resp): EF, as the hardware
/// leader marks them; CS1 is listed as reserved. spec:5.4 item 5 --
/// `confirmed`.
pub const DSCP_GENERAL: u8 = 46;

/// Stratum a slave-only node advertises in its own fields. spec:2.3 -- `likely`
/// (255 = slave-only, std Table 7).
pub const FOLLOWER_STRATUM: u8 = 255;
/// Stratum a leader-capable node advertises while not preferred. The only
/// hardware leader value seen is 0x29; ours must be numerically greater (we
/// lose elections to real hardware, ADR 0005) and not 0xFF (follower only).
/// spec:5.4 item 2 -- PROVISIONAL until capture C2, no meaning in its bits.
pub const LEADER_STRATUM: u8 = 0x2A;
/// Clock identifier of a free-running clock. std Table 7 "DFLT" -- `likely`.
pub const IDENTIFIER_DFLT: [u8; 4] = *b"DFLT";
/// Default clock variance (1588-2002 scaled log variance). Recollection of
/// the value used by public PTPv1 implementations -- `guess`.
pub const DEFAULT_VARIANCE: i16 = -4000;

/// Sync interval, log2 seconds (-2 = 250 ms, four Syncs per second), as the
/// only leader seen. spec:5.4 item 3 -- `confirmed` for that leader.
pub const SYNC_INTERVAL_LOG2: i8 = -2;
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
