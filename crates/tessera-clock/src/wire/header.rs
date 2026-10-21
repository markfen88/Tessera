//! The 40-byte common header (spec:2.1, 2.2).

use super::EncodeError;
use super::io::{ParseError, ParseErrorKind, Reader, Writer};
use crate::provisional;

/// `control` field values (spec:2.1; std Table 4). `likely`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// 0: Sync.
    Sync,
    /// 1: Delay_Req.
    DelayReq,
    /// 2: Follow_Up.
    FollowUp,
    /// 3: Delay_Resp.
    DelayResp,
    /// 4: Management.
    Management,
}

impl Control {
    /// Wire value.
    #[must_use]
    pub fn to_u8(self) -> u8 {
        match self {
            Control::Sync => 0,
            Control::DelayReq => 1,
            Control::FollowUp => 2,
            Control::DelayResp => 3,
            Control::Management => 4,
        }
    }

    /// Parses a wire value.
    #[must_use]
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Control::Sync,
            1 => Control::DelayReq,
            2 => Control::FollowUp,
            3 => Control::DelayResp,
            4 => Control::Management,
            _ => return None,
        })
    }

    /// `messageType` byte: 1 = event (Sync, Delay_Req), 2 = general. spec:2.1.
    #[must_use]
    pub fn message_type(self) -> u8 {
        match self {
            Control::Sync | Control::DelayReq => 1,
            _ => 2,
        }
    }

    /// True for event messages (need wire timestamps, UDP port 319).
    #[must_use]
    pub fn is_event(self) -> bool {
        self.message_type() == 1
    }
}

/// The 16-bit flags field (offset 34; the flag bits all live in the low
/// byte, offset 35). spec:2.2 (`likely` for bit positions; names confirmed
/// by the Wireshark field list: `ptp.flags.li61`, `.li59`, `.boundary_clock`,
/// `.assist`, `.ext_sync`, `.parent_stats`, `.sync_burst`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags(pub u16);

impl Flags {
    /// PTP_LI61: leap second inserted at end of the current UTC day.
    pub const LI61: u16 = 1 << 0;
    /// PTP_LI59: leap second deleted.
    pub const LI59: u16 = 1 << 1;
    /// PTP_BOUNDARY_CLOCK: sender is a boundary clock.
    pub const BOUNDARY_CLOCK: u16 = 1 << 2;
    /// PTP_ASSIST: two-step; a Follow_Up carries the exact departure time.
    pub const ASSIST: u16 = 1 << 3;
    /// PTP_EXT_SYNC: external sync source.
    pub const EXT_SYNC: u16 = 1 << 4;
    /// PARENT_STATS: the parent statistics fields are valid.
    pub const PARENT_STATS: u16 = 1 << 5;
    /// PTP_SYNC_BURST: sender is bursting Syncs.
    pub const SYNC_BURST: u16 = 1 << 6;

    /// True if every bit of `mask` is set.
    #[must_use]
    pub fn contains(self, mask: u16) -> bool {
        self.0 & mask == mask
    }

    /// Returns the flags with `mask` set.
    #[must_use]
    pub fn with(self, mask: u16) -> Self {
        Flags(self.0 | mask)
    }
}

/// Identity of a PTP port: communication technology, EUI-48 uuid and port
/// number (spec:2.1; `likely`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PortIdentity {
    /// sourceCommunicationTechnology (1 = UDP/IPv4).
    pub comm_tech: u8,
    /// EUI-48 of the sending clock/port (MAC).
    pub uuid: [u8; 6],
    /// Port number.
    pub port_id: u16,
}

impl PortIdentity {
    /// True if uuid and port number are equal (technology ignored: the
    /// reference identifies peers by uuid + port, spec:2.5).
    #[must_use]
    pub fn same_port(&self, other: &PortIdentity) -> bool {
        self.uuid == other.uuid && self.port_id == other.port_id
    }
}

/// Common header fields. `versionPTP` is always 1 and `messageType` is
/// derived from the message kind, so neither is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// versionNetwork (written as 1; accepted unchecked, spec:2).
    pub version_network: u16,
    /// Subdomain name, NUL padded (spec:2.1).
    pub subdomain: [u8; 16],
    /// Sender (source) port identity.
    pub source: PortIdentity,
    /// Sequence id, per message type and per sender (wrapping).
    pub sequence_id: u16,
    /// Flags.
    pub flags: Flags,
}

impl Header {
    pub(crate) fn read(r: &mut Reader<'_>) -> Result<(Header, Control), ParseError> {
        let version = r.u16()?;
        if version != provisional::VERSION_PTP {
            return Err(ParseError {
                offset: 0,
                kind: ParseErrorKind::NotPtpV1(version),
            });
        }
        let version_network = r.u16()?;
        let subdomain = r.take::<16>()?;
        let _message_type = r.u8()?;
        let comm_tech = r.u8()?;
        let uuid = r.take::<6>()?;
        let port_id = r.u16()?;
        let sequence_id = r.u16()?;
        let control_off = r.pos();
        let control_raw = r.u8()?;
        let control = Control::from_u8(control_raw).ok_or(ParseError {
            offset: control_off,
            kind: ParseErrorKind::UnknownControl(control_raw),
        })?;
        r.skip(1)?;
        let flags = Flags(r.u16()?);
        r.skip(4)?;
        Ok((
            Header {
                version_network,
                subdomain,
                source: PortIdentity {
                    comm_tech,
                    uuid,
                    port_id,
                },
                sequence_id,
                flags,
            },
            control,
        ))
    }

    pub(crate) fn write(&self, w: &mut Writer<'_>, control: Control) -> Result<(), EncodeError> {
        w.u16(provisional::VERSION_PTP)?;
        w.u16(self.version_network)?;
        w.put(&self.subdomain)?;
        w.u8(control.message_type())?;
        w.u8(self.source.comm_tech)?;
        w.put(&self.source.uuid)?;
        w.u16(self.source.port_id)?;
        w.u16(self.sequence_id)?;
        w.u8(control.to_u8())?;
        w.zeros(1)?;
        w.u16(self.flags.0)?;
        w.zeros(4)
    }
}
