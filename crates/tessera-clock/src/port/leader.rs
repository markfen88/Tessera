//! Leader behaviour: not evidenced by any open implementation (spec:5.2,
//! `guess`); built from IEEE 1588-2002 §9.2/§11 alone.
//!
//! * Sync is multicast every sync interval on the event channel; two-step by
//!   default (PTP_ASSIST, zero origin time, Follow_Up carrying the TX
//!   timestamp converted to media time). If the TX timestamp does not arrive
//!   within `tx_timestamp_timeout_ns` the Follow_Up is sent from the send
//!   time instead (counted in `tx_timestamp_fallbacks`).
//! * Each Delay_Req is answered on the general channel with a Delay_Resp
//!   holding the RX time converted to media time.
//! * The leader's time is its own media clock, free-running at its current
//!   rate (frozen at the last estimate).

use super::{Port, PortState};
use crate::action::{Action, ActionSink, Channel, PortEvent, TxToken};
use crate::config::SyncMode;
use crate::media_clock::gm_identity_from_uuid;
use crate::time::{LocalNs, Timestamp};
use crate::wire::{DelayRespBody, Flags, FollowUpBody, Header, Message, SyncBody};

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingSyncTx {
    seq: u16,
    sent_at: LocalNs,
}

impl<const N: usize> Port<N> {
    pub(super) fn enter_master<S: ActionSink + ?Sized>(&mut self, now: LocalNs, sink: &mut S) {
        if self.state == PortState::Master {
            return;
        }
        self.freeze_learned_rate(now);
        self.drop_parent_state();
        self.servo.reset();
        self.last_locked = false;
        self.listen_deadline = None;
        self.holdover_deadline = None;
        self.free_running = false;
        self.clock
            .set_gm_identity(gm_identity_from_uuid(&self.cfg.identity.uuid));
        // A new leading episode: preferred rivals are reported afresh
        // (spec:5.4 item 1).
        self.foreign.clear_rival_reported();
        self.set_state(PortState::Master, sink);
        self.next_sync_at = Some(now);
        self.dirty = true;
        sink.action(Action::Event(PortEvent::BecameLeader));
    }

    pub(super) fn leave_master(&mut self) {
        self.next_sync_at = None;
        self.tx_pending = [None; 2];
    }

    fn header(&self, seq: u16, flags: Flags) -> Header {
        Header {
            version_network: crate::provisional::VERSION_NETWORK,
            subdomain: self.cfg.subdomain,
            source: self.cfg.identity,
            sequence_id: seq,
            flags,
        }
    }

    fn send_sync<S: ActionSink + ?Sized>(&mut self, now: LocalNs, sink: &mut S) {
        let seq = self.sync_seq;
        self.sync_seq = seq.wrapping_add(1);
        let two_step = self.cfg.sync_mode == SyncMode::TwoStep;
        let l = &self.cfg.local;
        let id = &self.cfg.identity;
        let mut flags = Flags(0);
        if two_step {
            flags = flags.with(Flags::ASSIST);
        }
        if l.is_boundary_clock {
            flags = flags.with(Flags::BOUNDARY_CLOCK);
        }
        let origin = if two_step {
            Timestamp::default()
        } else {
            Timestamp::from_nanos(self.clock.media_ns_at(now))
        };
        let body = SyncBody {
            origin_timestamp: origin,
            epoch_number: 0,
            current_utc_offset: l.utc_offset,
            gm_comm_tech: id.comm_tech,
            gm_uuid: id.uuid,
            gm_port_id: id.port_id,
            gm_sequence_id: seq,
            gm_stratum: l.stratum,
            gm_identifier: l.identifier,
            gm_variance: l.variance,
            gm_preferred: l.preferred,
            gm_is_boundary_clock: l.is_boundary_clock,
            sync_interval: self.cfg.sync_interval_log2,
            local_clock_variance: l.variance,
            local_steps_removed: 0,
            local_clock_stratum: l.stratum,
            local_clock_identifier: l.identifier,
            parent_comm_tech: id.comm_tech,
            parent_uuid: id.uuid,
            parent_port_id: id.port_id,
            estimated_master_variance: 0,
            estimated_master_drift: 0,
            utc_reasonable: l.utc_reasonable,
        };
        let header = self.header(seq, flags);
        let token = two_step.then_some(TxToken {
            is_sync: true,
            sequence_id: seq,
        });
        if two_step {
            // Keep the two most recent; the oldest is dropped.
            self.tx_pending[1] = self.tx_pending[0];
            self.tx_pending[0] = Some(PendingSyncTx { seq, sent_at: now });
        }
        self.stats.sync_tx += 1;
        self.send(sink, Channel::Event, &Message::Sync { header, body }, token);
    }

    fn send_follow_up<S: ActionSink + ?Sized>(&mut self, seq: u16, t: LocalNs, sink: &mut S) {
        let header = self.header(seq, Flags(0));
        let body = FollowUpBody {
            associated_sequence_id: seq,
            precise_origin_timestamp: Timestamp::from_nanos(self.clock.media_ns_at(t)),
        };
        self.stats.follow_up_tx += 1;
        self.send(
            sink,
            Channel::General,
            &Message::FollowUp { header, body },
            None,
        );
    }

    pub(super) fn leader_tx_timestamp<S: ActionSink + ?Sized>(
        &mut self,
        now: LocalNs,
        seq: u16,
        ts: Option<LocalNs>,
        sink: &mut S,
    ) {
        if self.state != PortState::Master {
            return;
        }
        let Some(i) = self
            .tx_pending
            .iter()
            .position(|p| p.is_some_and(|p| p.seq == seq))
        else {
            return;
        };
        let sent_at = self.tx_pending.get(i).copied().flatten().map(|p| p.sent_at);
        if let Some(slot) = self.tx_pending.get_mut(i) {
            *slot = None;
        }
        match (ts, sent_at) {
            (Some(t), _) => self.send_follow_up(seq, t, sink),
            (None, Some(t)) => {
                self.stats.tx_timestamp_fallbacks += 1;
                self.send_follow_up(seq, t, sink);
            }
            (None, None) => {}
        }
        let _ = now;
    }

    pub(super) fn on_delay_req<S: ActionSink + ?Sized>(
        &mut self,
        _now: LocalNs,
        req: &Header,
        rx_ts: Option<LocalNs>,
        sink: &mut S,
    ) {
        self.stats.delay_req_rx += 1;
        if self.state != PortState::Master {
            return;
        }
        let Some(rx) = rx_ts else {
            self.stats.missing_rx_timestamp += 1;
            return;
        };
        let header = self.header(req.sequence_id, Flags(0));
        let body = DelayRespBody {
            delay_receipt_timestamp: Timestamp::from_nanos(self.clock.media_ns_at(rx)),
            requesting_comm_tech: req.source.comm_tech,
            requesting_uuid: req.source.uuid,
            requesting_port_id: req.source.port_id,
            requesting_sequence_id: req.sequence_id,
        };
        self.stats.delay_resp_tx += 1;
        self.send(
            sink,
            Channel::General,
            &Message::DelayResp { header, body },
            None,
        );
    }

    pub(super) fn timers_leader<S: ActionSink + ?Sized>(&mut self, now: LocalNs, sink: &mut S) {
        if self.state != PortState::Master {
            return;
        }
        for i in 0..self.tx_pending.len() {
            let Some(p) = self.tx_pending.get(i).copied().flatten() else {
                continue;
            };
            if now.saturating_sub(p.sent_at) >= self.cfg.tx_timestamp_timeout_ns {
                self.stats.tx_timestamp_timeouts += 1;
                self.leader_tx_timestamp(now, p.seq, None, sink);
            }
        }
        if let Some(t) = self.next_sync_at {
            if now >= t {
                self.send_sync(now, sink);
                let step = self.sync_interval_ns();
                let next = t.saturating_add(step);
                self.next_sync_at = Some(if next <= now {
                    now.saturating_add(step)
                } else {
                    next
                });
            }
        }
    }

    pub(super) fn leader_deadline(&self) -> Option<LocalNs> {
        if self.state != PortState::Master {
            return None;
        }
        let mut d = self.next_sync_at;
        for p in self.tx_pending.iter().flatten() {
            let t = p.sent_at.saturating_add(self.cfg.tx_timestamp_timeout_ns);
            d = Some(d.map_or(t, |x| x.min(t)));
        }
        d
    }
}
