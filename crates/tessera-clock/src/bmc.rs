//! Best-master selection: datasets, the comparison and the foreign-master
//! table.
//!
//! PTPv1 has no Announce message: every Sync carries the grandmaster's
//! description (spec:1, 2.3), so each Sync from a sender updates that
//! sender's foreign-master record. The comparison implemented here is the
//! IEEE 1588-2002 ordering as described in spec:5.3 -- preferred flag, then
//! stratum, then identifier, then variance, then tie-breakers -- which is a
//! public-standard model (`guess` for Dante). It is *not* what Dante hardware
//! is known to do (UNKNOWN, capture question P1-Q4) and is therefore a
//! function pointer in [`crate::config::PortConfig::bmc`] so it can be
//! replaced without touching the state machine.

use core::cmp::Ordering;

use crate::time::LocalNs;
use crate::wire::{Flags, Header, PortIdentity, SyncBody};

/// The attributes the comparison looks at (spec:2.3 grandmaster block).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClockDataset {
    /// grandmasterPreferred.
    pub preferred: bool,
    /// grandmasterClockStratum (lower is better).
    pub stratum: u8,
    /// grandmasterClockIdentifier.
    pub identifier: [u8; 4],
    /// grandmasterClockVariance (lower is better).
    pub variance: i16,
    /// grandmasterIsBoundaryClock.
    pub is_boundary_clock: bool,
    /// grandmasterClockUuid.
    pub uuid: [u8; 6],
    /// Hops from the grandmaster as seen at the sender (localStepsRemoved).
    pub steps_removed: u16,
}

impl ClockDataset {
    /// The grandmaster block of a Sync body.
    #[must_use]
    pub fn from_sync(body: &SyncBody) -> Self {
        ClockDataset {
            preferred: body.gm_preferred,
            stratum: body.gm_stratum,
            identifier: body.gm_identifier,
            variance: body.gm_variance,
            is_boundary_clock: body.gm_is_boundary_clock,
            uuid: body.gm_uuid,
            steps_removed: body.local_steps_removed,
        }
    }
}

/// A dataset comparison: `Less` means the first argument is the better clock.
pub type BmcCompare = fn(&ClockDataset, &ClockDataset) -> Ordering;

/// Rank of a clock identifier by expected accuracy (std Table 7 as
/// recalled -- `guess`): lower is better; unknown identifiers rank last.
fn identifier_rank(id: &[u8; 4]) -> u8 {
    match id {
        b"ATOM" => 0,
        b"GPS\0" => 1,
        b"NTP\0" => 2,
        b"HAND" => 3,
        b"INIT" => 4,
        b"DFLT" => 5,
        _ => 6,
    }
}

/// IEEE 1588-2002 style comparison (spec:5.3, `guess` as a model for Dante).
#[must_use]
pub fn compare_std2002(a: &ClockDataset, b: &ClockDataset) -> Ordering {
    if a.uuid == b.uuid {
        // Same grandmaster seen through different paths: fewer hops wins.
        return a.steps_removed.cmp(&b.steps_removed);
    }
    // Between two preferred clocks only the lower MAC decides (spec:5.4
    // item 1, documented election rule).
    if a.preferred && b.preferred {
        return a.uuid.cmp(&b.uuid);
    }
    // Preferred beats not preferred.
    b.preferred
        .cmp(&a.preferred)
        .then(a.stratum.cmp(&b.stratum))
        .then(identifier_rank(&a.identifier).cmp(&identifier_rank(&b.identifier)))
        .then_with(|| {
            if identifier_rank(&a.identifier) == 6 {
                a.identifier.cmp(&b.identifier)
            } else {
                Ordering::Equal
            }
        })
        .then(a.variance.cmp(&b.variance))
        // An ordinary clock beats a boundary clock (guess).
        .then(a.is_boundary_clock.cmp(&b.is_boundary_clock))
        .then(a.steps_removed.cmp(&b.steps_removed))
        .then(a.uuid.cmp(&b.uuid))
}

/// One foreign master: what a sender has been advertising in its Syncs.
#[derive(Debug, Clone, Copy)]
pub struct ForeignMaster {
    /// The sending port.
    pub sender: PortIdentity,
    /// Its grandmaster dataset.
    pub dataset: ClockDataset,
    /// The last Sync body received (used to fill our Delay_Req).
    pub body: SyncBody,
    /// The last header flags.
    pub flags: Flags,
    /// Local time of the last Sync.
    pub last_rx: LocalNs,
    /// Sequence id of the last Sync.
    pub last_seq: u16,
    /// Local time of the last Sync that counted towards qualification.
    pub last_counted: LocalNs,
    /// Syncs counted towards qualification.
    pub count: u8,
    /// True once `count` reached the threshold.
    pub qualified: bool,
    /// This sender was already reported as a preferred clock heard while we
    /// lead (spec:5.4 item 1). Cleared when the record restarts after a
    /// silence and when the port starts to lead again.
    pub rival_reported: bool,
}

/// A Sync this many sequence ids behind the sender's last one, or fewer, is
/// stale (a duplicate or a reordered datagram, spec:4 "stale sequence number
/// (wrapping compare)"). A larger step back is the sender restarting its
/// sequence: tessera's own leader does so when its port restarts on an
/// interface change, and its followers must keep their lock across that
/// (ADR 0005 amendment), so such a Sync is accepted.
const STALE_SPAN: u16 = 16;

/// `seq` is stale against the sender's `last` sequence id (wrapping).
fn seq_stale(seq: u16, last: u16) -> bool {
    last.wrapping_sub(seq) < STALE_SPAN
}

/// Qualification parameters for [`ForeignMasters::update`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualifyParams {
    /// How long a record may be silent before its count restarts.
    pub window_ns: u64,
    /// Counted Syncs inside the window that qualify a master.
    pub threshold: u8,
    /// Syncs closer together than this count once (a burst is one Sync).
    pub min_spacing_ns: u64,
}

/// Fixed-capacity foreign-master table. The capacity is a const generic
/// (default 8, spec:4); when full, only unqualified non-parent records are
/// replaced (stalest first).
#[derive(Debug, Clone, Copy)]
pub struct ForeignMasters<const N: usize> {
    recs: [Option<ForeignMaster>; N],
}

impl<const N: usize> Default for ForeignMasters<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> ForeignMasters<N> {
    /// An empty table.
    #[must_use]
    pub const fn new() -> Self {
        ForeignMasters { recs: [None; N] }
    }

    /// Records a Sync. See [`QualifyParams`]. Returns the updated record, or
    /// `None` if the datagram was a duplicate or stale (equal to or at most
    /// `STALE_SPAN - 1` behind the previous Sync's sequence id from that
    /// sender, in wrapping arithmetic, spec:4) or the table is full of
    /// records that may not be evicted. After a silence longer than the
    /// window the sender starts afresh, whatever its sequence id.
    ///
    /// Eviction (table full, unknown sender): only an *unqualified* record
    /// that is not `protect` (the current parent) can be replaced, the
    /// stalest first. A qualified master or the parent is never displaced by
    /// a newcomer, so a burst of fresh identities cannot flush the table.
    pub fn update(
        &mut self,
        now: LocalNs,
        p: &QualifyParams,
        protect: Option<&PortIdentity>,
        header: &Header,
        body: &SyncBody,
    ) -> Option<&ForeignMaster> {
        let idx = self.recs.iter().position(|r| {
            r.as_ref()
                .is_some_and(|r| r.sender.same_port(&header.source))
        });
        let idx = match idx {
            Some(i) => i,
            None => self.free_slot(protect)?,
        };
        let slot = self.recs.get_mut(idx)?;
        let (count, qualified, last_counted, rival_reported) = match slot {
            Some(r) => {
                let silent = now.saturating_sub(r.last_rx);
                if silent < p.window_ns && seq_stale(header.sequence_id, r.last_seq) {
                    return None;
                }
                if silent > p.window_ns {
                    (1, false, now, false)
                } else if now.saturating_sub(r.last_counted) >= p.min_spacing_ns {
                    (
                        r.count.saturating_add(1),
                        r.qualified,
                        now,
                        r.rival_reported,
                    )
                } else {
                    // A burst: does not count towards qualification.
                    (r.count, r.qualified, r.last_counted, r.rival_reported)
                }
            }
            None => (1, false, now, false),
        };
        let qualified = qualified || count >= p.threshold;
        *slot = Some(ForeignMaster {
            sender: header.source,
            dataset: ClockDataset::from_sync(body),
            body: *body,
            flags: header.flags,
            last_rx: now,
            last_seq: header.sequence_id,
            last_counted,
            count,
            qualified,
            rival_reported,
        });
        slot.as_ref()
    }

    fn free_slot(&self, protect: Option<&PortIdentity>) -> Option<usize> {
        if let Some(i) = self.recs.iter().position(Option::is_none) {
            return Some(i);
        }
        self.recs
            .iter()
            .enumerate()
            .filter_map(|(i, r)| r.as_ref().map(|r| (i, r)))
            .filter(|(_, r)| !r.qualified && !protect.is_some_and(|p| r.sender.same_port(p)))
            .min_by_key(|(_, r)| r.last_rx)
            .map(|(i, _)| i)
    }

    /// Drops records silent for longer than `window_of(record)`.
    pub fn expire(&mut self, now: LocalNs, window_of: impl Fn(&ForeignMaster) -> u64) {
        for slot in &mut self.recs {
            if slot
                .as_ref()
                .is_some_and(|r| now.saturating_sub(r.last_rx) > window_of(r))
            {
                *slot = None;
            }
        }
    }

    /// Removes the record of `sender`.
    pub fn remove(&mut self, sender: &PortIdentity) {
        for slot in &mut self.recs {
            if slot.as_ref().is_some_and(|r| r.sender.same_port(sender)) {
                *slot = None;
            }
        }
    }

    /// The record of `sender`.
    #[must_use]
    pub fn get(&self, sender: &PortIdentity) -> Option<&ForeignMaster> {
        self.recs
            .iter()
            .flatten()
            .find(|r| r.sender.same_port(sender))
    }

    /// Every record.
    pub fn iter(&self) -> impl Iterator<Item = &ForeignMaster> {
        self.recs.iter().flatten()
    }

    /// Marks the record of `sender` as reported (see
    /// [`ForeignMaster::rival_reported`]); `true` if it was not marked yet.
    pub fn mark_rival_reported(&mut self, sender: &PortIdentity) -> bool {
        self.recs
            .iter_mut()
            .flatten()
            .find(|r| r.sender.same_port(sender))
            .is_some_and(|r| !core::mem::replace(&mut r.rival_reported, true))
    }

    /// Clears every report mark.
    pub fn clear_rival_reported(&mut self) {
        for r in self.recs.iter_mut().flatten() {
            r.rival_reported = false;
        }
    }

    /// Number of records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.recs.iter().flatten().count()
    }

    /// True if there are no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The best qualified record under `cmp`; ties are broken by sender
    /// uuid and port so the result is deterministic.
    #[must_use]
    pub fn best(&self, cmp: BmcCompare) -> Option<&ForeignMaster> {
        self.recs
            .iter()
            .flatten()
            .filter(|r| r.qualified)
            .min_by(|a, b| {
                cmp(&a.dataset, &b.dataset)
                    .then(a.sender.uuid.cmp(&b.sender.uuid))
                    .then(a.sender.port_id.cmp(&b.sender.port_id))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Flags;

    fn ds(pref: bool, stratum: u8, id: &[u8; 4], var: i16, uuid: u8) -> ClockDataset {
        ClockDataset {
            preferred: pref,
            stratum,
            identifier: *id,
            variance: var,
            is_boundary_clock: false,
            uuid: [uuid; 6],
            steps_removed: 0,
        }
    }

    #[test]
    fn ordering_preferred_stratum_identifier_variance_uuid() {
        let c = compare_std2002;
        let base = ds(false, 4, b"DFLT", 0, 2);
        assert_eq!(c(&ds(true, 255, b"DFLT", 100, 9), &base), Ordering::Less);
        assert_eq!(c(&ds(false, 3, b"DFLT", 100, 9), &base), Ordering::Less);
        assert_eq!(c(&ds(false, 4, b"GPS\0", 100, 9), &base), Ordering::Less);
        assert_eq!(c(&ds(false, 4, b"DFLT", -1, 9), &base), Ordering::Less);
        assert_eq!(c(&ds(false, 4, b"DFLT", 0, 1), &base), Ordering::Less);
        assert_eq!(c(&base, &base), Ordering::Equal);
        // Unknown identifiers rank after DFLT.
        assert_eq!(c(&base, &ds(false, 4, b"ZZZZ", 0, 1)), Ordering::Less);
    }

    #[test]
    fn between_preferred_clocks_only_the_lower_mac_decides() {
        let c = compare_std2002;
        let low = ds(true, 0x2A, b"DFLT", 0, 1);
        let high = ds(true, 0x29, b"DFLT", -100, 9);
        assert_eq!(c(&low, &high), Ordering::Less);
        assert_eq!(c(&high, &low), Ordering::Greater);
    }

    #[test]
    fn same_grandmaster_prefers_fewer_hops() {
        let mut a = ds(false, 4, b"DFLT", 0, 2);
        let mut b = a;
        a.steps_removed = 1;
        b.steps_removed = 3;
        assert_eq!(compare_std2002(&a, &b), Ordering::Less);
    }

    fn hdr(uuid: u8, seq: u16) -> Header {
        Header {
            version_network: 1,
            subdomain: [0; 16],
            source: PortIdentity {
                comm_tech: 1,
                uuid: [uuid; 6],
                port_id: 1,
            },
            sequence_id: seq,
            flags: Flags(0),
        }
    }

    fn body(stratum: u8, gm: u8) -> SyncBody {
        SyncBody {
            gm_stratum: stratum,
            gm_uuid: [gm; 6],
            gm_identifier: *b"DFLT",
            ..SyncBody::default()
        }
    }

    const W: u64 = 4_000_000_000;

    fn qp() -> QualifyParams {
        QualifyParams {
            window_ns: W,
            threshold: 2,
            min_spacing_ns: 500_000_000,
        }
    }

    fn up<const N: usize>(
        t: &mut ForeignMasters<N>,
        now: u64,
        protect: Option<&PortIdentity>,
        h: &Header,
        b: &SyncBody,
    ) -> Option<bool> {
        t.update(now, &qp(), protect, h, b).map(|r| r.qualified)
    }

    #[test]
    fn qualification_needs_spaced_syncs_in_window_and_dups_are_dropped() {
        let mut t = ForeignMasters::<4>::new();
        assert_eq!(up(&mut t, 0, None, &hdr(1, 1), &body(4, 1)), Some(false));
        assert_eq!(
            up(&mut t, 0, None, &hdr(1, 1), &body(4, 1)),
            None,
            "duplicate"
        );
        // A second Sync in the same burst (different sequence id, 1 ms later)
        // does not count.
        assert_eq!(
            up(&mut t, 1_000_000, None, &hdr(1, 2), &body(4, 1)),
            Some(false)
        );
        assert!(t.best(compare_std2002).is_none());
        // Half an interval later it does.
        assert_eq!(
            up(&mut t, 600_000_000, None, &hdr(1, 3), &body(4, 1)),
            Some(true)
        );
        assert_eq!(t.best(compare_std2002).unwrap().sender.uuid, [1; 6]);
        // Silence longer than the window removes the record.
        t.expire(10_000_000_000, |_| W);
        assert!(t.is_empty());
        assert_eq!(
            up(&mut t, 20_000_000_000, None, &hdr(1, 4), &body(4, 1)),
            Some(false)
        );
    }

    #[test]
    fn stale_sequence_ids_are_dropped_with_wrapping_compare() {
        let mut t = ForeignMasters::<4>::new();
        assert_eq!(up(&mut t, 0, None, &hdr(1, 10), &body(4, 1)), Some(false));
        // An older (reordered) Sync is dropped like a duplicate (spec:4).
        assert_eq!(up(&mut t, 600_000_000, None, &hdr(1, 9), &body(4, 1)), None);
        assert_eq!(t.get(&hdr(1, 0).source).unwrap().last_seq, 10);
        // Later ids count, across the 16-bit wrap.
        let mut t = ForeignMasters::<4>::new();
        up(&mut t, 0, None, &hdr(1, 0xFFFF), &body(4, 1));
        assert_eq!(
            up(&mut t, 600_000_000, None, &hdr(1, 0), &body(4, 1)),
            Some(true),
            "0 follows 0xFFFF"
        );
        // Stale reaches back STALE_SPAN - 1 ids ...
        let mut t = ForeignMasters::<4>::new();
        up(&mut t, 0, None, &hdr(1, 5000), &body(4, 1));
        let back = 5000 - (STALE_SPAN - 1);
        assert_eq!(
            up(&mut t, 600_000_000, None, &hdr(1, back), &body(4, 1)),
            None
        );
        // ... a larger step back is the sender restarting its sequence (a
        // tessera leader restarted on an interface change): it counts at once
        // and the record stays qualified, so its followers keep their lock.
        up(&mut t, 600_000_000, None, &hdr(1, 5001), &body(4, 1));
        assert_eq!(
            up(&mut t, 1_200_000_000, None, &hdr(1, 0), &body(4, 1)),
            Some(true)
        );
        assert_eq!(t.get(&hdr(1, 0).source).unwrap().last_seq, 0);
        // After a silence longer than the window any id starts afresh.
        assert_eq!(
            up(&mut t, 1_200_000_000 + W + 1, None, &hdr(1, 0), &body(4, 1)),
            Some(false),
            "a fresh record after the silence"
        );
    }

    #[test]
    fn the_rival_mark_is_kept_per_record_and_reset_by_a_silence() {
        let mut t = ForeignMasters::<4>::new();
        let s = hdr(1, 0).source;
        up(&mut t, 0, None, &hdr(1, 0), &body(4, 1));
        assert!(t.mark_rival_reported(&s));
        assert!(!t.mark_rival_reported(&s), "once");
        up(&mut t, 600_000_000, None, &hdr(1, 1), &body(4, 1));
        assert!(t.get(&s).unwrap().rival_reported, "kept across Syncs");
        t.clear_rival_reported();
        assert!(t.mark_rival_reported(&s));
        up(&mut t, 600_000_000 + W + 1, None, &hdr(1, 2), &body(4, 1));
        assert!(!t.get(&s).unwrap().rival_reported, "a new episode");
        assert!(!t.mark_rival_reported(&hdr(2, 0).source), "unknown sender");
    }

    #[test]
    fn best_picks_lowest_stratum_and_evicts_only_unqualified_records() {
        let mut t = ForeignMasters::<2>::new();
        for (i, (u, s)) in [(1u8, 5u8), (2, 3)].into_iter().enumerate() {
            for k in 0..2u16 {
                let now = (i as u64) * 100_000_000 + u64::from(k) * 1_000_000_000;
                up(&mut t, now, None, &hdr(u, k), &body(s, u));
            }
        }
        assert_eq!(t.best(compare_std2002).unwrap().sender.uuid, [2; 6]);
        // Both records are qualified: a newcomer is dropped, nothing evicted.
        assert_eq!(
            up(&mut t, 3_000_000_000, None, &hdr(3, 0), &body(1, 3)),
            None
        );
        assert_eq!(t.len(), 2);
        assert!(t.get(&hdr(1, 0).source).is_some() && t.get(&hdr(2, 0).source).is_some());
    }

    #[test]
    fn a_burst_of_identities_cannot_flush_the_parent_or_qualified_records() {
        let mut t = ForeignMasters::<4>::new();
        let parent = hdr(1, 0).source;
        for k in 0..2u16 {
            up(
                &mut t,
                u64::from(k) * 1_000_000_000,
                Some(&parent),
                &hdr(1, k),
                &body(4, 1),
            );
        }
        assert!(t.get(&parent).unwrap().qualified);
        // 20 fresh identities, two back-to-back Syncs each, better stratum.
        for id in 10..30u8 {
            for k in 0..2u16 {
                up(
                    &mut t,
                    2_000_000_000 + u64::from(k) * 1_000,
                    Some(&parent),
                    &hdr(id, k),
                    &body(1, id),
                );
            }
        }
        assert!(t.get(&parent).is_some(), "the parent survives");
        assert_eq!(
            t.best(compare_std2002).unwrap().sender.uuid,
            [1; 6],
            "no burst identity qualified"
        );
        assert!(t.len() <= 4);
    }
}
