//! PTPv1 (IEEE 1588-2002) message codecs.
//!
//! Total parsers (ADR 0009): no input can make [`Message::parse`] panic, and
//! this module denies direct indexing so that stays true. Every layout below
//! cites `spec/protocol/clock-ptp.md` section 2 (`spec:2.x`), which was
//! cross-checked against IEEE 1588-2002 §13.3-13.6 (`std`), and the public
//! Wireshark field reference for field and flag *names* (`wireshark`). Nothing
//! is `confirmed`: there are no captures of Dante devices yet.
//!
//! * Receivers recognise PTPv1 by `versionPTP == 1` and accept a message by
//!   its `control` byte, not by `messageType` (spec:2.1 note a). `messageType`
//!   is derived on encode and ignored on parse.
//! * Reserved bytes are written as zero and ignored on parse.
//! * Trailing bytes beyond a message's fixed length are ignored; shorter
//!   input is [`ParseErrorKind::Truncated`] (spec:2).
//! * Booleans carried in a byte are `true` iff non-zero on parse and written
//!   as `0`/`1`.
//!
//! ADR 0002 lists wire codecs under `tessera-proto`; they live here until the
//! orchestrator decides whether to move them (they are PTP-only).
#![deny(clippy::indexing_slicing)]

mod body;
mod header;
mod io;

pub use body::{DelayRespBody, FollowUpBody, SyncBody};
pub use header::{Control, Flags, Header, PortIdentity};
pub use io::{EncodeError, ParseError, ParseErrorKind};

use io::{Reader, Writer};

/// Length of the common header (spec:2.1).
pub const HEADER_LEN: usize = 40;
/// Length of a Sync or Delay_Req message (spec:2.3).
pub const SYNC_LEN: usize = 124;
/// Length of a Follow_Up message (spec:2.4).
pub const FOLLOW_UP_LEN: usize = 52;
/// Length of a Delay_Resp message (spec:2.5).
pub const DELAY_RESP_LEN: usize = 60;
/// Largest message this crate itself transmits.
pub const MAX_TX_LEN: usize = SYNC_LEN;

/// A parsed PTPv1 message. `Management` borrows its payload from the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message<'a> {
    /// Sync (control 0, event). spec:2.3.
    Sync {
        /// Common header.
        header: Header,
        /// Body.
        body: SyncBody,
    },
    /// Delay_Req (control 1, event). Same body as Sync. spec:2.3.
    DelayReq {
        /// Common header.
        header: Header,
        /// Body.
        body: SyncBody,
    },
    /// Follow_Up (control 2, general). spec:2.4.
    FollowUp {
        /// Common header.
        header: Header,
        /// Body.
        body: FollowUpBody,
    },
    /// Delay_Resp (control 3, general). spec:2.5.
    DelayResp {
        /// Common header.
        header: Header,
        /// Body.
        body: DelayRespBody,
    },
    /// Management (control 4). The body layout is not sourced (spec:2.6,
    /// `guess`), so it is carried as opaque bytes.
    Management {
        /// Common header.
        header: Header,
        /// Everything after the 40-byte header.
        payload: &'a [u8],
    },
}

impl<'a> Message<'a> {
    /// Parses one UDP payload.
    ///
    /// # Errors
    /// [`ParseError`] with the offending byte offset if the input is not a
    /// well-formed PTPv1 message this crate knows.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ParseError> {
        let mut r = Reader::new(bytes);
        let (header, control) = Header::read(&mut r)?;
        Ok(match control {
            Control::Sync => Message::Sync {
                header,
                body: SyncBody::read(&mut r)?,
            },
            Control::DelayReq => Message::DelayReq {
                header,
                body: SyncBody::read(&mut r)?,
            },
            Control::FollowUp => Message::FollowUp {
                header,
                body: FollowUpBody::read(&mut r)?,
            },
            Control::DelayResp => Message::DelayResp {
                header,
                body: DelayRespBody::read(&mut r)?,
            },
            Control::Management => Message::Management {
                header,
                payload: r.rest(),
            },
        })
    }

    /// The common header.
    #[must_use]
    pub fn header(&self) -> &Header {
        match self {
            Message::Sync { header, .. }
            | Message::DelayReq { header, .. }
            | Message::FollowUp { header, .. }
            | Message::DelayResp { header, .. }
            | Message::Management { header, .. } => header,
        }
    }

    /// The `control` value of this message.
    #[must_use]
    pub fn control(&self) -> Control {
        match self {
            Message::Sync { .. } => Control::Sync,
            Message::DelayReq { .. } => Control::DelayReq,
            Message::FollowUp { .. } => Control::FollowUp,
            Message::DelayResp { .. } => Control::DelayResp,
            Message::Management { .. } => Control::Management,
        }
    }

    /// Encoded length in bytes.
    #[must_use]
    pub fn wire_len(&self) -> usize {
        match self {
            Message::Sync { .. } | Message::DelayReq { .. } => SYNC_LEN,
            Message::FollowUp { .. } => FOLLOW_UP_LEN,
            Message::DelayResp { .. } => DELAY_RESP_LEN,
            Message::Management { payload, .. } => HEADER_LEN.saturating_add(payload.len()),
        }
    }

    /// Encodes into `out`, returning the number of bytes written.
    ///
    /// # Errors
    /// [`EncodeError::BufferTooSmall`] if `out` is shorter than
    /// [`Message::wire_len`]; nothing is written in that case.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, EncodeError> {
        let needed = self.wire_len();
        if out.len() < needed {
            return Err(EncodeError::BufferTooSmall { needed });
        }
        let mut w = Writer::new(out);
        self.header().write(&mut w, self.control())?;
        match self {
            Message::Sync { body, .. } | Message::DelayReq { body, .. } => body.write(&mut w)?,
            Message::FollowUp { body, .. } => body.write(&mut w)?,
            Message::DelayResp { body, .. } => body.write(&mut w)?,
            Message::Management { payload, .. } => w.put(payload)?,
        }
        Ok(w.len())
    }
}
