//! Follower behaviour: offset/delay computation (spec:5.1, std §11.3).
//!
//! * Sync: one-step (`origin_timestamp` is t1) or two-step (PTP_ASSIST; t1
//!   comes from the Follow_Up). A Follow_Up that overtakes its Sync is kept
//!   and matched when the Sync arrives (spec:5.1 step 3).
//! * Delay_Req/Delay_Resp measure the path: `2d = (t2-t1) + (t4-t3) -
//!   rate*(t3-t2)`; the last term compensates for the drift of the local
//!   clock between the Sync and the Delay_Req (`rate` is the media clock's
//!   current rate), which would otherwise bias the mean delay by up to
//!   ppm x interval. The delay is median filtered.
//! * Offset per Sync: `e = media(t2) - (t1 + d + asymmetry/2)`.

use super::{Port, PortState};
use crate::action::{Action, ActionSink, Channel, PortEvent, TxToken};
use crate::config::FollowUpMatch;
use crate::servo::ServoOutput;
use crate::time::{LocalNs, Timestamp, add_signed, diff_ns, interval_ns};
use crate::wire::{DelayRespBody, Flags, FollowUpBody, Header, Message, SyncBody};

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingSync {
    valid: bool,
    seq: u16,
    t1: Option<u64>,
    t2: Option<u64>,
}

impl PendingSync {
    pub(crate) const EMPTY: PendingSync = PendingSync {
        valid: false,
        seq: 0,
        t1: None,
        t2: None,
    };
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SyncPair {
    t1: u64,
    t2: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingDelayReq {
    seq: u16,
    t3: Option<u64>,
    t4: Option<u64>,
    sent_at: LocalNs,
}

impl<const N: usize> Port<N> {
    fn in_follow_state(&self) -> bool {
        matches!(self.state, PortState::Uncalibrated | PortState::Slave)
    }

    fn is_from_parent(&self, h: &Header) -> bool {
        self.in_follow_state() && self.parent.is_some_and(|p| p.source.same_port(&h.source))
    }

    fn slot_index(&self, seq: u16) -> Option<usize> {
        self.pending.iter().position(|s| s.valid && s.seq == seq)
    }

    fn slot_for(&mut self, seq: u16) -> usize {
        if let Some(i) = self.slot_index(seq) {
            return i;
        }
        let i = self.pending_next % self.pending.len();
        self.pending_next = (i + 1) % self.pending.len();
        if let Some(s) = self.pending.get_mut(i) {
            *s = PendingSync {
                valid: true,
                seq,
                t1: None,
                t2: None,
            };
        }
        i
    }

    pub(super) fn follower_sync<S: ActionSink + ?Sized>(
        &mut self,
        now: LocalNs,
        header: &Header,
        body: &SyncBody,
        rx_ts: Option<LocalNs>,
        sink: &mut S,
    ) {
        if let Some(p) = self.parent.as_mut() {
            p.last_rx = now;
            p.body = *body;
        }
        let Some(t2) = rx_ts else {
            self.stats.missing_rx_timestamp += 1;
            return;
        };
        let idx = self.slot_for(header.sequence_id);
        let assist = header.flags.contains(Flags::ASSIST);
        let t1 = if assist {
            None
        } else {
            match body.origin_timestamp.to_nanos() {
                Some(t1) => Some(t1),
                None => {
                    self.stats.bad_timestamps += 1;
                    if let Some(s) = self.pending.get_mut(idx) {
                        s.valid = false;
                    }
                    return;
                }
            }
        };
        if let Some(s) = self.pending.get_mut(idx) {
            s.t2 = Some(t2);
            if t1.is_some() {
                s.t1 = t1;
            }
        }
        self.try_complete(idx, now, sink);
    }

    pub(super) fn on_follow_up<S: ActionSink + ?Sized>(
        &mut self,
        now: LocalNs,
        header: &Header,
        body: &FollowUpBody,
        sink: &mut S,
    ) {
        self.stats.follow_up_rx += 1;
        if !self.is_from_parent(header) {
            return;
        }
        let assoc = body.associated_sequence_id;
        let hdr = header.sequence_id;
        let key = match self.cfg.follow_up_match {
            FollowUpMatch::Associated => assoc,
            FollowUpMatch::HeaderSequence => hdr,
            FollowUpMatch::Either => {
                if self.slot_index(assoc).is_none() && self.slot_index(hdr).is_some() {
                    hdr
                } else {
                    assoc
                }
            }
        };
        let Some(t1) = body.precise_origin_timestamp.to_nanos() else {
            self.stats.bad_timestamps += 1;
            return;
        };
        let idx = self.slot_for(key);
        if let Some(s) = self.pending.get_mut(idx) {
            if s.t1.is_some() {
                return; // duplicate
            }
            s.t1 = Some(t1);
        }
        self.try_complete(idx, now, sink);
    }

    fn try_complete<S: ActionSink + ?Sized>(&mut self, idx: usize, now: LocalNs, sink: &mut S) {
        let Some(s) = self.pending.get_mut(idx) else {
            return;
        };
        if let (true, Some(t1), Some(t2)) = (s.valid, s.t1, s.t2) {
            s.valid = false;
            self.on_sample(now, t1, t2, sink);
        }
    }

    /// One complete (t1, t2) Sync observation.
    fn on_sample<S: ActionSink + ?Sized>(&mut self, now: LocalNs, t1: u64, t2: u64, sink: &mut S) {
        if t2 <= self.last_sample_t2 {
            self.stats.samples_out_of_order += 1;
            return;
        }
        self.last_sample_t2 = t2;
        self.pairs[1] = self.pairs[0];
        self.pairs[0] = Some(SyncPair { t1, t2 });
        self.stats.samples += 1;

        if self.next_dreq_at.is_none() {
            self.next_dreq_at = Some(now.saturating_add(self.sync_interval_ns() / 4));
        }
        if !self.delay.is_valid() && self.samples_waiting < self.cfg.max_samples_without_delay {
            self.samples_waiting += 1;
            self.stats.samples_waiting_for_delay += 1;
            return;
        }
        // Rate and step changes are applied at the time of processing (never
        // before the sample), so media time cannot go backwards.
        let at = now.max(self.clock.anchor_mono_ns());
        let delay = self.delay.value().unwrap_or(self.cfg.initial_delay_ns);
        let leader_at_t2 = add_signed(t1, delay.saturating_add(self.cfg.path_asymmetry_ns / 2));
        let e = diff_ns(self.clock.media_ns_at(t2), leader_at_t2);
        match self.servo.sample(t2, e, self.clock.rate_ppb()) {
            ServoOutput::Rejected => self.stats.samples_rejected += 1,
            ServoOutput::Hold => {}
            ServoOutput::Rate { rate_ppb } => {
                self.clock.set_rate(at, rate_ppb);
                self.dirty = true;
            }
            ServoOutput::Step {
                correction_ns,
                rate_ppb,
            } => {
                self.clock.step(at, correction_ns);
                self.clock.set_rate(at, rate_ppb);
                self.stats.steps += 1;
                self.dirty = true;
                sink.action(Action::Event(PortEvent::Stepped { correction_ns }));
            }
        }
        // The servo only starts once a path delay is known (or the wait for one
        // was given up), so a calibrated servo is enough for SLAVE.
        if self.state == PortState::Uncalibrated && self.servo.is_calibrated() {
            self.set_state(PortState::Slave, sink);
        }
    }

    // ------------------------------------------------------------ Delay_Req

    fn send_delay_req<S: ActionSink + ?Sized>(&mut self, now: LocalNs, sink: &mut S) {
        let mean = interval_ns(self.cfg.delay_req_interval_log2);
        // Until a few measurements exist, ask once per sync interval so the
        // median filter fills quickly; afterwards at the randomised mean rate.
        let wait = if self.delay.len() < usize::from(self.cfg.delay_req_startup_burst) {
            self.sync_interval_ns()
        } else {
            self.rand_below(mean.saturating_mul(2).saturating_add(1))
        };
        self.next_dreq_at = Some(now.saturating_add(wait.max(1_000_000)));
        if self.dreq.is_some() {
            return;
        }
        let Some(parent) = self.parent else {
            return;
        };
        let seq = self.dreq_seq;
        self.dreq_seq = seq.wrapping_add(1);
        let l = &self.cfg.local;
        let pb = &parent.body;
        let body = SyncBody {
            origin_timestamp: Timestamp::default(),
            epoch_number: 0,
            current_utc_offset: 0,
            gm_comm_tech: pb.gm_comm_tech,
            gm_uuid: pb.gm_uuid,
            gm_port_id: pb.gm_port_id,
            gm_sequence_id: pb.gm_sequence_id,
            gm_stratum: pb.gm_stratum,
            gm_identifier: pb.gm_identifier,
            gm_variance: pb.gm_variance,
            gm_preferred: pb.gm_preferred,
            gm_is_boundary_clock: pb.gm_is_boundary_clock,
            sync_interval: self.cfg.delay_req_sync_interval_field,
            local_clock_variance: l.variance,
            local_steps_removed: pb.local_steps_removed.saturating_add(1),
            local_clock_stratum: l.stratum,
            local_clock_identifier: l.identifier,
            parent_comm_tech: parent.source.comm_tech,
            parent_uuid: parent.source.uuid,
            parent_port_id: parent.source.port_id,
            estimated_master_variance: 0,
            estimated_master_drift: 0,
            utc_reasonable: false,
        };
        let header = Header {
            version_network: crate::provisional::VERSION_NETWORK,
            subdomain: self.cfg.subdomain,
            source: self.cfg.identity,
            sequence_id: seq,
            flags: Flags(0),
        };
        self.dreq = Some(PendingDelayReq {
            seq,
            t3: None,
            t4: None,
            sent_at: now,
        });
        self.stats.delay_req_tx += 1;
        self.send(
            sink,
            Channel::Event,
            &Message::DelayReq { header, body },
            Some(TxToken {
                is_sync: false,
                sequence_id: seq,
            }),
        );
    }

    pub(super) fn follower_tx_timestamp(&mut self, seq: u16, ts: Option<LocalNs>) {
        let Some(d) = self.dreq.as_mut() else {
            return;
        };
        if d.seq != seq {
            return;
        }
        match ts {
            None => {
                self.dreq = None;
                self.stats.tx_timestamp_timeouts += 1;
            }
            Some(t3) => {
                d.t3 = Some(t3);
                self.maybe_complete_delay();
            }
        }
    }

    pub(super) fn on_delay_resp(&mut self, header: &Header, body: &DelayRespBody) {
        self.stats.delay_resp_rx += 1;
        if !self.is_from_parent(header) {
            return;
        }
        let me = &self.cfg.identity;
        if body.requesting_uuid != me.uuid || body.requesting_port_id != me.port_id {
            return;
        }
        let Some(d) = self.dreq.as_mut() else {
            return;
        };
        if d.seq != body.requesting_sequence_id || d.t4.is_some() {
            return;
        }
        let Some(t4) = body.delay_receipt_timestamp.to_nanos() else {
            self.stats.bad_timestamps += 1;
            return;
        };
        d.t4 = Some(t4);
        self.maybe_complete_delay();
    }

    fn maybe_complete_delay(&mut self) {
        let Some(d) = self.dreq else {
            return;
        };
        let (Some(t3), Some(t4)) = (d.t3, d.t4) else {
            return;
        };
        self.dreq = None;
        // Latest Sync observation taken before the Delay_Req left.
        let pair = self.pairs.iter().flatten().find(|p| p.t2 <= t3).copied();
        let Some(pair) = pair else {
            self.stats.delay_rejected += 1;
            return;
        };
        let ms = i128::from(diff_ns(pair.t2, pair.t1));
        let sm = i128::from(diff_ns(t4, t3));
        let drift = i128::from(self.clock.rate_ppb()) * i128::from(t3 - pair.t2) / 1_000_000_000;
        let d2 = ms + sm - drift;
        let d = d2 / 2;
        if d < 0 || d > i128::from(self.cfg.max_path_delay_ns) {
            self.stats.delay_rejected += 1;
            return;
        }
        self.delay.push(d as i64);
        self.stats.delay_samples += 1;
    }

    pub(super) fn timers_follower<S: ActionSink + ?Sized>(&mut self, now: LocalNs, sink: &mut S) {
        if !self.in_follow_state() {
            return;
        }
        if let Some(d) = self.dreq {
            let age = now.saturating_sub(d.sent_at);
            if d.t3.is_none() && age >= self.cfg.tx_timestamp_timeout_ns {
                self.stats.tx_timestamp_timeouts += 1;
                self.dreq = None;
            } else if age >= self.cfg.delay_resp_timeout_ns {
                self.dreq = None;
            }
        }
        if self.next_dreq_at.is_some_and(|t| now >= t) {
            self.send_delay_req(now, sink);
        }
    }

    pub(super) fn follower_deadline(&self) -> Option<LocalNs> {
        if !self.in_follow_state() {
            return None;
        }
        let mut d: Option<LocalNs> = self.next_dreq_at;
        let mut add = |t: LocalNs| d = Some(d.map_or(t, |x| x.min(t)));
        if let Some(p) = self.parent {
            let timeout = self
                .effective_interval_ns(p.body.sync_interval)
                .saturating_mul(u64::from(self.cfg.sync_receipt_timeout));
            add(p.last_rx.saturating_add(timeout));
        }
        if let Some(r) = self.dreq {
            add(r.sent_at.saturating_add(if r.t3.is_none() {
                self.cfg.tx_timestamp_timeout_ns
            } else {
                self.cfg.delay_resp_timeout_ns
            }));
        }
        d
    }
}
