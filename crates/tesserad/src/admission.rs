//! Connection admission: global, per-uid and reserved-for-administrators caps.
//!
//! Without limits, any member of the socket group could open connections
//! until none are left and lock the administrators out. Rules:
//!
//! * each uid may hold at most `per_uid` connections;
//! * read-only peers may use at most `max_total - reserved` slots in total,
//!   so `reserved` slots can only be taken by administrators;
//! * administrators may use all `max_total` slots.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

/// Why a connection was not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The uid already holds its share.
    PerUid,
    /// No slot is available to this class of peer.
    Full,
}

#[derive(Debug, Default)]
struct Inner {
    total: usize,
    per_uid: HashMap<u32, usize>,
    /// The caps (changed by a reload; held slots are never taken away).
    max_total: usize,
    reserved: usize,
    per_uid_cap: usize,
}

/// The admission table.
#[derive(Debug)]
pub struct Admission {
    inner: Mutex<Inner>,
}

/// A held slot; released when dropped.
#[derive(Debug)]
pub struct Permit {
    owner: Arc<Admission>,
    uid: u32,
}

impl Admission {
    /// `max_total` slots, `reserved` of which are for administrators only,
    /// and at most `per_uid` per uid (all at least 1).
    #[must_use]
    pub fn new(max_total: usize, reserved: usize, per_uid: usize) -> Arc<Self> {
        let a = Arc::new(Self {
            inner: Mutex::new(Inner::default()),
        });
        a.set_caps(max_total, reserved, per_uid);
        a
    }

    /// Change the caps (a reload of `[control]` or `[audio]`). They apply to
    /// new connections; connections over a lowered cap stay open.
    pub fn set_caps(&self, max_total: usize, reserved: usize, per_uid: usize) {
        let max_total = max_total.max(1);
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.max_total = max_total;
        g.reserved = reserved.min(max_total - 1);
        g.per_uid_cap = per_uid.max(1);
    }

    /// The caps in effect: `(max_total, reserved, per_uid)`.
    #[must_use]
    pub fn caps(&self) -> (usize, usize, usize) {
        let g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        (g.max_total, g.reserved, g.per_uid_cap)
    }

    /// Try to take a slot for `uid`.
    ///
    /// # Errors
    /// [`Refusal`].
    pub fn admit(self: &Arc<Self>, uid: u32, admin: bool) -> Result<Permit, Refusal> {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let limit = if admin {
            g.max_total
        } else {
            g.max_total - g.reserved
        };
        if g.total >= limit {
            return Err(Refusal::Full);
        }
        let cap = g.per_uid_cap;
        let held = g.per_uid.entry(uid).or_insert(0);
        if *held >= cap {
            return Err(Refusal::PerUid);
        }
        *held += 1;
        g.total += 1;
        Ok(Permit {
            owner: Arc::clone(self),
            uid,
        })
    }

    /// Slots in use.
    #[must_use]
    pub fn in_use(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .total
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut g = self
            .owner
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        g.total = g.total.saturating_sub(1);
        if let Some(n) = g.per_uid.get_mut(&self.uid) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                g.per_uid.remove(&self.uid);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_uid_cap_applies_to_everyone() {
        let a = Admission::new(100, 0, 3);
        let held: Vec<_> = (0..3).map(|_| a.admit(7, false).unwrap()).collect();
        assert_eq!(a.admit(7, false).unwrap_err(), Refusal::PerUid);
        assert_eq!(a.admit(7, true).unwrap_err(), Refusal::PerUid);
        assert!(a.admit(8, false).is_ok());
        drop(held);
        assert!(a.admit(7, false).is_ok());
    }

    #[test]
    fn reserved_slots_are_for_administrators() {
        let a = Admission::new(4, 2, 10);
        let _r1 = a.admit(1, false).unwrap();
        let _r2 = a.admit(2, false).unwrap();
        assert_eq!(a.admit(3, false).unwrap_err(), Refusal::Full);
        let _a1 = a.admit(9, true).unwrap();
        let _a2 = a.admit(9, true).unwrap();
        assert_eq!(a.admit(9, true).unwrap_err(), Refusal::Full);
        assert_eq!(a.in_use(), 4);
    }

    #[test]
    fn lowered_caps_refuse_new_connections_and_never_close_held_ones() {
        let a = Admission::new(10, 2, 10);
        let held: Vec<_> = (0..6).map(|u| a.admit(u, false).unwrap()).collect();
        // Below what is in use, and a reserve larger than the total.
        a.set_caps(3, 50, 1);
        assert_eq!(a.caps(), (3, 2, 1), "reserved is cut to total - 1");
        assert_eq!(a.in_use(), 6, "held slots stay");
        assert_eq!(a.admit(99, true).unwrap_err(), Refusal::Full);
        drop(held);
        assert_eq!(a.in_use(), 0, "no underflow when they go");
        let _held = a.admit(1, false).unwrap();
        assert_eq!(a.admit(1, true).unwrap_err(), Refusal::PerUid);
        a.set_caps(0, 0, 0);
        assert_eq!(a.caps(), (1, 0, 1), "at least one slot");
    }

    #[test]
    fn degenerate_numbers_still_leave_room() {
        let a = Admission::new(0, 99, 0);
        assert!(a.admit(1, false).is_ok(), "one slot, nothing reserved");
    }

    #[test]
    fn dropping_permits_releases_slots_and_forgets_uids() {
        let a = Admission::new(5, 0, 5);
        {
            let _p: Vec<_> = (0..5).map(|u| a.admit(u, false).unwrap()).collect();
            assert_eq!(a.in_use(), 5);
        }
        assert_eq!(a.in_use(), 0);
        assert!(a.inner.lock().unwrap().per_uid.is_empty());
    }
}
