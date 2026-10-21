//! Live commands for a running port.
//!
//! A command changes the configuration of a port that is already running and
//! goes through the state machine like any other event: the best master is
//! re-evaluated immediately. Applying a command never steps media time, and
//! the clock epoch changes only if the servo later has to step (ADR 0005):
//! a leader demotion keeps the mapping and the rate (the old leader becomes a
//! follower of the new one and slews any difference below the step
//! threshold), and a promotion starts leading from the current mapping.

use crate::config::{LocalClock, Role};

/// A command that [`Port::apply_command`](crate::port::Port::apply_command)
/// understands. `Copy`, so it can travel through a lock-free queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortCommand {
    /// Changes [`Role`](crate::config::Role).
    ///
    /// * to `Follower`: a leader stops sending Syncs at once and listens; the
    ///   media clock keeps running on its mapping (reported as `Holdover`)
    ///   until a parent is selected.
    /// * to `Leader`: the port leads immediately, whatever it hears.
    /// * to `Auto`: the best-master comparison decides.
    SetRole(Role),
    /// Replaces the clock attributes this node advertises (the BMC dataset
    /// knobs: preferred flag, stratum, identifier, variance, boundary-clock
    /// flag, UTC fields) and re-runs the election. PTPv1 has no priority1 /
    /// priority2 fields (OQ-CLK-1); "priority" is expressed through these
    /// (lower stratum wins, a preferred clock beats any non-preferred one).
    SetLocalClock(LocalClock),
    /// Sets the publish guard (spec:8.2): a snapshot that differs from the
    /// last published one by at least `threshold_ns` is held back for
    /// `hold_ns`; `hold_ns == 0` disables the guard (and releases anything
    /// currently held).
    SetPublishGuard {
        /// Jump size that triggers the hold, ns.
        threshold_ns: u64,
        /// Hold time, ns; 0 = guard off.
        hold_ns: u64,
    },
}
