//! The output side of the sans-IO core.

use crate::media_clock::MediaClockSnapshot;
use crate::time::LocalNs;

/// Which UDP port (and DSCP class) a message uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Event messages (Sync, Delay_Req): port 319, need wire timestamps.
    Event,
    /// General messages (Follow_Up, Delay_Resp, Management): port 320.
    General,
}

/// Identifies a transmitted event message whose TX timestamp the port wants.
/// The driver returns it with the timestamp via
/// [`Port::handle_tx_timestamp`](crate::port::Port::handle_tx_timestamp).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TxToken {
    /// Message kind: `true` for Sync, `false` for Delay_Req.
    pub is_sync: bool,
    /// Sequence id of the message.
    pub sequence_id: u16,
}

/// Things worth telling the control plane about (`Copy`, ring-friendly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortEvent {
    /// A new parent (leader) was selected.
    ParentSelected {
        /// Its grandmaster uuid.
        gm_uuid: [u8; 6],
    },
    /// The parent timed out.
    ParentLost,
    /// This node became the leader.
    BecameLeader,
    /// Lock status of the servo changed.
    LockChanged {
        /// New status.
        locked: bool,
    },
    /// The media clock stepped (epoch incremented).
    Stepped {
        /// The applied correction in ns.
        correction_ns: i64,
    },
}

/// One thing the port wants done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action<'a> {
    /// Transmit `data` to the subdomain's multicast group on `channel`'s
    /// port. If `tx_token` is set, capture the wire TX timestamp and return it.
    Send {
        /// Port/class.
        channel: Channel,
        /// The UDP payload; valid only for the duration of the call.
        data: &'a [u8],
        /// Request a TX timestamp.
        tx_token: Option<TxToken>,
    },
    /// Call `handle_timeout` at (or after) this local monotonic time. The
    /// latest `WakeAt` supersedes earlier ones.
    WakeAt(LocalNs),
    /// Publish a new media-clock snapshot (see
    /// [`ClockPublisher`](crate::media_clock::ClockPublisher)).
    ClockUpdate(MediaClockSnapshot),
    /// The port state changed.
    StateChanged {
        /// Old state.
        from: crate::port::PortState,
        /// New state.
        to: crate::port::PortState,
    },
    /// Informational event.
    Event(PortEvent),
}

/// Receives the port's actions. Implemented for any `FnMut(Action<'_>)`.
pub trait ActionSink {
    /// Handles one action.
    fn action(&mut self, action: Action<'_>);
}

impl<F> ActionSink for F
where
    F: FnMut(Action<'_>),
{
    fn action(&mut self, action: Action<'_>) {
        self(action);
    }
}
