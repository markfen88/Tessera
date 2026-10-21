//! Message bodies (spec:2.3-2.5).

use super::EncodeError;
use super::io::{ParseError, Reader, Writer};
use crate::time::Timestamp;

fn read_ts(r: &mut Reader<'_>) -> Result<Timestamp, ParseError> {
    Ok(Timestamp {
        seconds: r.u32()?,
        nanoseconds: r.u32()?,
    })
}

fn write_ts(w: &mut Writer<'_>, t: Timestamp) -> Result<(), EncodeError> {
    w.u32(t.seconds)?;
    w.u32(t.nanoseconds)
}

fn flag(v: bool) -> u8 {
    u8::from(v)
}

/// Body of Sync and Delay_Req: 84 bytes at offsets 40..124 (spec:2.3,
/// std Table 8, field names confirmed against the Wireshark `ptp.sdr.*`
/// list). Confidence per field is in the spec table; all `likely` unless
/// noted. A Delay_Req sends `origin_timestamp` as zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncBody {
    /// originTimestamp (40). Sync: departure time, valid only when
    /// PTP_ASSIST is clear. Delay_Req: zero.
    pub origin_timestamp: Timestamp,
    /// epochNumber (48). Read, ignored; written 0. `guess`.
    pub epoch_number: u16,
    /// currentUtcOffset, seconds (50). Meaningful only if `utc_reasonable`.
    pub current_utc_offset: i16,
    /// grandmasterCommunicationTechnology (53).
    pub gm_comm_tech: u8,
    /// grandmasterClockUuid (54): MAC of the grandmaster.
    pub gm_uuid: [u8; 6],
    /// grandmasterPortId (60).
    pub gm_port_id: u16,
    /// grandmasterSequenceId (62).
    pub gm_sequence_id: u16,
    /// grandmasterClockStratum (67): lower is better; 255 = slave-only.
    /// Dante's values UNKNOWN.
    pub gm_stratum: u8,
    /// grandmasterClockIdentifier (68): ASCII, e.g. `ATOM`, `GPS\0`, `NTP\0`,
    /// `HAND`, `DFLT`.
    pub gm_identifier: [u8; 4],
    /// grandmasterClockVariance (74): scaled log variance, lower is better.
    pub gm_variance: i16,
    /// grandmasterPreferred (77). Mapping from the vendor's "Preferred
    /// Leader" is `guess`.
    pub gm_preferred: bool,
    /// grandmasterIsBoundaryClock (79).
    pub gm_is_boundary_clock: bool,
    /// syncInterval (83), log2 seconds. Delay_Req from the reference carries
    /// 0x7F (`guess`).
    pub sync_interval: i8,
    /// localClockVariance (86).
    pub local_clock_variance: i16,
    /// localStepsRemoved (90): hops from the grandmaster.
    pub local_steps_removed: u16,
    /// localClockStratum (95).
    pub local_clock_stratum: u8,
    /// localClockIdentifier (96).
    pub local_clock_identifier: [u8; 4],
    /// parentCommunicationTechnology (101).
    pub parent_comm_tech: u8,
    /// parentUuid (102): MAC of the sender's parent.
    pub parent_uuid: [u8; 6],
    /// parentPortField (110).
    pub parent_port_id: u16,
    /// estimatedMasterVariance (114), valid with PARENT_STATS.
    pub estimated_master_variance: i16,
    /// estimatedMasterDrift (116), valid with PARENT_STATS.
    pub estimated_master_drift: i32,
    /// utcReasonable (123).
    pub utc_reasonable: bool,
}

impl SyncBody {
    pub(crate) fn read(r: &mut Reader<'_>) -> Result<Self, ParseError> {
        let origin_timestamp = read_ts(r)?;
        let epoch_number = r.u16()?;
        let current_utc_offset = r.i16()?;
        r.skip(1)?;
        let gm_comm_tech = r.u8()?;
        let gm_uuid = r.take::<6>()?;
        let gm_port_id = r.u16()?;
        let gm_sequence_id = r.u16()?;
        r.skip(3)?;
        let gm_stratum = r.u8()?;
        let gm_identifier = r.take::<4>()?;
        r.skip(2)?;
        let gm_variance = r.i16()?;
        r.skip(1)?;
        let gm_preferred = r.u8()? != 0;
        r.skip(1)?;
        let gm_is_boundary_clock = r.u8()? != 0;
        r.skip(3)?;
        let sync_interval = r.i8()?;
        r.skip(2)?;
        let local_clock_variance = r.i16()?;
        r.skip(2)?;
        let local_steps_removed = r.u16()?;
        r.skip(3)?;
        let local_clock_stratum = r.u8()?;
        let local_clock_identifier = r.take::<4>()?;
        r.skip(1)?;
        let parent_comm_tech = r.u8()?;
        let parent_uuid = r.take::<6>()?;
        r.skip(2)?;
        let parent_port_id = r.u16()?;
        r.skip(2)?;
        let estimated_master_variance = r.i16()?;
        let estimated_master_drift = r.i32()?;
        r.skip(3)?;
        let utc_reasonable = r.u8()? != 0;
        Ok(SyncBody {
            origin_timestamp,
            epoch_number,
            current_utc_offset,
            gm_comm_tech,
            gm_uuid,
            gm_port_id,
            gm_sequence_id,
            gm_stratum,
            gm_identifier,
            gm_variance,
            gm_preferred,
            gm_is_boundary_clock,
            sync_interval,
            local_clock_variance,
            local_steps_removed,
            local_clock_stratum,
            local_clock_identifier,
            parent_comm_tech,
            parent_uuid,
            parent_port_id,
            estimated_master_variance,
            estimated_master_drift,
            utc_reasonable,
        })
    }

    pub(crate) fn write(&self, w: &mut Writer<'_>) -> Result<(), EncodeError> {
        write_ts(w, self.origin_timestamp)?;
        w.u16(self.epoch_number)?;
        w.i16(self.current_utc_offset)?;
        w.zeros(1)?;
        w.u8(self.gm_comm_tech)?;
        w.put(&self.gm_uuid)?;
        w.u16(self.gm_port_id)?;
        w.u16(self.gm_sequence_id)?;
        w.zeros(3)?;
        w.u8(self.gm_stratum)?;
        w.put(&self.gm_identifier)?;
        w.zeros(2)?;
        w.i16(self.gm_variance)?;
        w.zeros(1)?;
        w.u8(flag(self.gm_preferred))?;
        w.zeros(1)?;
        w.u8(flag(self.gm_is_boundary_clock))?;
        w.zeros(3)?;
        w.i8(self.sync_interval)?;
        w.zeros(2)?;
        w.i16(self.local_clock_variance)?;
        w.zeros(2)?;
        w.u16(self.local_steps_removed)?;
        w.zeros(3)?;
        w.u8(self.local_clock_stratum)?;
        w.put(&self.local_clock_identifier)?;
        w.zeros(1)?;
        w.u8(self.parent_comm_tech)?;
        w.put(&self.parent_uuid)?;
        w.zeros(2)?;
        w.u16(self.parent_port_id)?;
        w.zeros(2)?;
        w.i16(self.estimated_master_variance)?;
        w.i32(self.estimated_master_drift)?;
        w.zeros(3)?;
        w.u8(flag(self.utc_reasonable))
    }
}

/// Follow_Up body: 12 bytes at 40..52 (spec:2.4, `likely`; the parser layout
/// of the reference matches the standard, its serialiser does not -- the
/// standard layout is used).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FollowUpBody {
    /// associatedSequenceId (42): the Sync's sequence id.
    pub associated_sequence_id: u16,
    /// preciseOriginTimestamp (44): exact departure time of the Sync.
    pub precise_origin_timestamp: Timestamp,
}

impl FollowUpBody {
    pub(crate) fn read(r: &mut Reader<'_>) -> Result<Self, ParseError> {
        r.skip(2)?;
        let associated_sequence_id = r.u16()?;
        let precise_origin_timestamp = read_ts(r)?;
        Ok(FollowUpBody {
            associated_sequence_id,
            precise_origin_timestamp,
        })
    }

    pub(crate) fn write(&self, w: &mut Writer<'_>) -> Result<(), EncodeError> {
        w.zeros(2)?;
        w.u16(self.associated_sequence_id)?;
        write_ts(w, self.precise_origin_timestamp)
    }
}

/// Delay_Resp body: 20 bytes at 40..60 (spec:2.5, `likely`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DelayRespBody {
    /// delayReceiptTimestamp (40): the leader's receive time of the Delay_Req.
    pub delay_receipt_timestamp: Timestamp,
    /// requestingSourceCommunicationTechnology (49).
    pub requesting_comm_tech: u8,
    /// requestingSourceUuid (50).
    pub requesting_uuid: [u8; 6],
    /// requestingSourcePortId (56).
    pub requesting_port_id: u16,
    /// requestingSourceSequenceId (58): sequence id of the Delay_Req answered.
    pub requesting_sequence_id: u16,
}

impl DelayRespBody {
    pub(crate) fn read(r: &mut Reader<'_>) -> Result<Self, ParseError> {
        let delay_receipt_timestamp = read_ts(r)?;
        r.skip(1)?;
        let requesting_comm_tech = r.u8()?;
        let requesting_uuid = r.take::<6>()?;
        let requesting_port_id = r.u16()?;
        let requesting_sequence_id = r.u16()?;
        Ok(DelayRespBody {
            delay_receipt_timestamp,
            requesting_comm_tech,
            requesting_uuid,
            requesting_port_id,
            requesting_sequence_id,
        })
    }

    pub(crate) fn write(&self, w: &mut Writer<'_>) -> Result<(), EncodeError> {
        write_ts(w, self.delay_receipt_timestamp)?;
        w.zeros(1)?;
        w.u8(self.requesting_comm_tech)?;
        w.put(&self.requesting_uuid)?;
        w.u16(self.requesting_port_id)?;
        w.u16(self.requesting_sequence_id)
    }
}
