//! Matching kernel TX timestamps to the messages that were sent.
//!
//! A TX timestamp comes back on the error queue with the looped-back packet.
//! The packet itself identifies the message (PTP type and `sequenceId`), which
//! is robust against the kernel's per-socket key (`SOF_TIMESTAMPING_OPT_ID`)
//! getting out of step with our own count -- on some kernels a failed send
//! consumes a key, on others not. The key is kept only as a fallback.

use crate::action::TxToken;

/// Length of Sync and Delay_Req, the only event messages we send (spec:2.3).
const EVENT_LEN: usize = crate::wire::SYNC_LEN;

/// Identifies the PTPv1 event message at the tail of a looped-back packet:
/// `(is_sync, sequence_id)`. The packet may carry IP/UDP headers in front, so
/// the message is taken from the end.
pub(crate) fn find_event_key(pkt: &[u8]) -> Option<(bool, u16)> {
    let start = pkt.len().checked_sub(EVENT_LEN)?;
    let msg = pkt.get(start..)?;
    // versionPTP == 1 and control 0 (Sync) or 1 (Delay_Req).
    if msg.get(0..2)? != [0, 1] {
        return None;
    }
    let is_sync = match msg.get(32)? {
        0 => true,
        1 => false,
        _ => return None,
    };
    let seq = u16::from_be_bytes([*msg.get(30)?, *msg.get(31)?]);
    Some((is_sync, seq))
}

const TX_SLOTS: usize = 16;

#[derive(Clone, Copy, Debug)]
pub(crate) struct TxSlot {
    pub(crate) id: u32,
    pub(crate) token: Option<TxToken>,
    pub(crate) sent_mono: u64,
    pub(crate) used: bool,
}

/// Outstanding event-message sends awaiting their TX timestamp.
#[derive(Debug)]
pub(crate) struct TxSlots {
    slots: [TxSlot; TX_SLOTS],
    next_id: u32,
    head: usize,
}

impl TxSlots {
    pub(crate) fn new() -> Self {
        TxSlots {
            slots: [TxSlot {
                id: 0,
                token: None,
                sent_mono: 0,
                used: false,
            }; TX_SLOTS],
            next_id: 0,
            head: 0,
        }
    }

    /// Records a successful event send (the kernel numbers sends 0, 1, 2...).
    pub(crate) fn record(&mut self, token: Option<TxToken>, sent_mono: u64) {
        let id = self.next_id;
        self.next_id = id.wrapping_add(1);
        if let Some(s) = self.slots.get_mut(self.head) {
            *s = TxSlot {
                id,
                token,
                sent_mono,
                used: true,
            };
        }
        self.head = (self.head + 1) % TX_SLOTS;
    }

    /// Forgets every outstanding send (their stamps are in another time base).
    pub(crate) fn clear(&mut self) {
        for s in &mut self.slots {
            s.used = false;
        }
    }

    /// Takes the slot of a looped-back packet: by message identity when the
    /// packet is recognisable, else by the kernel key.
    pub(crate) fn take(&mut self, key: Option<(bool, u16)>, id: u32) -> Option<TxSlot> {
        let found = match key {
            Some((is_sync, seq)) => self.slots.iter_mut().find(|s| {
                s.used
                    && s.token
                        .is_some_and(|t| t.is_sync == is_sync && t.sequence_id == seq)
            }),
            None => self.slots.iter_mut().find(|s| s.used && s.id == id),
        }?;
        found.used = false;
        Some(*found)
    }

    /// Slots older than `wait_ns`, removed and returned one at a time.
    pub(crate) fn take_expired(&mut self, now: u64, wait_ns: u64) -> Option<TxSlot> {
        let s = self
            .slots
            .iter_mut()
            .find(|s| s.used && now.saturating_sub(s.sent_mono) >= wait_ns)?;
        s.used = false;
        Some(*s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Flags, Header, Message, PortIdentity, SyncBody};

    fn sync_bytes(seq: u16, delay_req: bool) -> [u8; EVENT_LEN] {
        let header = Header {
            version_network: 1,
            subdomain: crate::provisional::DEFAULT_SUBDOMAIN,
            source: PortIdentity::default(),
            sequence_id: seq,
            flags: Flags(0),
        };
        let body = SyncBody::default();
        let m = if delay_req {
            Message::DelayReq { header, body }
        } else {
            Message::Sync { header, body }
        };
        let mut b = [0u8; EVENT_LEN];
        m.encode(&mut b).unwrap();
        b
    }

    #[test]
    fn key_is_found_with_and_without_ip_udp_headers() {
        let m = sync_bytes(0x1234, false);
        assert_eq!(find_event_key(&m), Some((true, 0x1234)));
        let mut with_hdr = [0xAAu8; 28 + EVENT_LEN];
        with_hdr[28..].copy_from_slice(&m);
        assert_eq!(find_event_key(&with_hdr), Some((true, 0x1234)));
        assert_eq!(find_event_key(&sync_bytes(7, true)), Some((false, 7)));
        // Empty (TSONLY-style) and unrecognisable packets give no key.
        assert_eq!(find_event_key(&[]), None);
        assert_eq!(find_event_key(&[0u8; EVENT_LEN]), None);
    }

    fn tok(is_sync: bool, seq: u16) -> Option<TxToken> {
        Some(TxToken {
            is_sync,
            sequence_id: seq,
        })
    }

    #[test]
    fn matching_by_message_survives_a_key_that_is_out_of_step() {
        let mut s = TxSlots::new();
        // Our count says ids 0, 1 (Sync 10, Delay_Req 3), but the kernel's key
        // ran ahead by one because of an earlier failed send: it reports 1, 2.
        s.record(tok(true, 10), 100);
        s.record(tok(false, 3), 200);
        let a = s.take(Some((true, 10)), 1).expect("matched by identity");
        assert_eq!(a.sent_mono, 100);
        let b = s.take(Some((false, 3)), 2).expect("matched by identity");
        assert_eq!(b.sent_mono, 200);
        assert!(s.take(Some((true, 10)), 1).is_none(), "consumed");
    }

    #[test]
    fn fallback_to_the_key_and_expiry() {
        let mut s = TxSlots::new();
        s.record(tok(true, 1), 100);
        s.record(None, 150); // one-step Sync: sent without a token
        assert_eq!(s.take(None, 0).unwrap().token, tok(true, 1));
        assert!(s.take(Some((true, 99)), 0).is_none());
        assert!(s.take_expired(140, 100).is_none());
        let e = s.take_expired(260, 100).expect("expired");
        assert_eq!(e.sent_mono, 150);
        assert!(s.take_expired(10_000, 100).is_none());
    }
}
