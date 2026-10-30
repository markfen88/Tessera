//! Bounded, boot-local single-use expectations. This table does not authenticate
//! callers or prove an external revision; the actor supplies/revalidates B.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tessera_control_types::{
    CommandError, EndpointRef, ReceiverChange, ReceiverExpectation, ReceiverOperationResult,
    ReceiverOperationState, ReceiverSelection,
};

const CAPACITY: usize = 512;
const TTL: Duration = Duration::from_secs(30);

/// Kernel-authenticated local caller, optionally delegating a named principal
/// through the trusted portal. A remote peer cannot choose a local UID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Principal {
    pub uid: u32,
    pub peer: Option<String>,
}
impl From<u32> for Principal {
    fn from(uid: u32) -> Self {
        Self { uid, peer: None }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Observation<B> {
    pub binding: B,
    pub receiver: EndpointRef,
    pub rx: u16,
    pub source: Option<ReceiverSelection>,
    pub raw_status: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct Ticket<B> {
    pub token: ReceiverExpectation,
    pub observation: Observation<B>,
    pub change: ReceiverChange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Unused,
    Pending,
    Dispatched,
    Finished(ReceiverOperationState),
}

#[derive(Debug)]
struct Record<B> {
    principal: Principal,
    created: Instant,
    observation: Observation<B>,
    change: Option<ReceiverChange>,
    phase: Phase,
}

impl<B> Record<B> {
    fn busy(&self) -> bool {
        matches!(
            self.phase,
            Phase::Pending
                | Phase::Dispatched
                | Phase::Finished(ReceiverOperationState::Unknown { .. })
        )
    }

    fn result(&self, token: ReceiverExpectation) -> ReceiverOperationResult {
        let state = match &self.phase {
            Phase::Unused | Phase::Pending | Phase::Dispatched => ReceiverOperationState::Pending,
            Phase::Finished(state) => state.clone(),
        };
        ReceiverOperationResult::new(
            token,
            self.observation.receiver.clone(),
            self.observation.rx,
            state,
        )
    }
}

#[derive(Debug)]
pub(crate) enum Admission<B> {
    New(Ticket<B>),
    Existing(ReceiverOperationResult),
}

#[derive(Debug)]
pub(crate) struct Table<B> {
    namespace: String,
    serial: u64,
    rows: BTreeMap<String, Record<B>>,
}

fn stale() -> CommandError {
    CommandError::RoutingSnapshotExpired
}

fn valid_change(change: &ReceiverChange) -> bool {
    let (previous, source) = match change {
        ReceiverChange::Connect { source } => (None, Some(source)),
        ReceiverChange::Replace { previous, source } => (Some(previous), Some(source)),
        ReceiverChange::Disconnect { previous } => (Some(previous), None),
        _ => return false,
    };
    previous.is_none_or(|p| p.tx_device.len() <= 255 && p.tx_channel.len() <= 255)
        && source.is_none_or(|s| {
            s.channel != 0 && s.endpoint.id.len() <= 128 && s.endpoint.generation.len() <= 128
        })
}

impl<B: Clone> Table<B> {
    pub(super) fn busy(&self) -> bool {
        self.rows.values().any(Record::busy)
    }

    pub(super) fn invalidate_unused(&mut self) {
        self.rows.retain(|_, row| row.phase != Phase::Unused);
    }

    pub(crate) fn new(namespace: String) -> Self {
        Self {
            namespace,
            serial: 0,
            rows: BTreeMap::new(),
        }
    }

    fn expire(&mut self, now: Instant) {
        self.rows
            .retain(|_, row| row.busy() || now.saturating_duration_since(row.created) < TTL);
    }

    pub(crate) fn issue(
        &mut self,
        principal: impl Into<Principal>,
        now: Instant,
        observation: Observation<B>,
    ) -> Result<ReceiverExpectation, CommandError> {
        self.expire(now);
        if self.rows.len() >= CAPACITY
            || observation.rx == 0
            || observation.receiver.id.len() > 128
            || observation.receiver.generation.len() > 128
            || observation
                .source
                .as_ref()
                .is_some_and(|s| s.tx_device.len() > 255 || s.tx_channel.len() > 255)
        {
            return Err(CommandError::RateLimited);
        }
        self.serial = self
            .serial
            .checked_add(1)
            .ok_or(CommandError::RateLimited)?;
        let token = ReceiverExpectation(format!("{}-{:016x}", self.namespace, self.serial));
        self.rows.insert(
            token.0.clone(),
            Record {
                principal: principal.into(),
                created: now,
                observation,
                change: None,
                phase: Phase::Unused,
            },
        );
        Ok(token)
    }

    pub(crate) fn accept(
        &mut self,
        principal: impl Into<Principal>,
        now: Instant,
        token: &ReceiverExpectation,
        change: ReceiverChange,
    ) -> Result<Admission<B>, CommandError> {
        self.expire(now);
        if token.0.len() > 64 || !valid_change(&change) {
            return Err(stale());
        }
        let principal = principal.into();
        let row = self
            .rows
            .get(&token.0)
            .filter(|r| r.principal == principal)
            .ok_or_else(stale)?;
        if row.phase != Phase::Unused {
            if row.change.as_ref() != Some(&change) {
                return Err(stale());
            }
            return Ok(Admission::Existing(row.result(token.clone())));
        }
        // Initial execution is globally serialized because one complete report
        // may contain both receive and transmit carriers. Unknown stays fenced;
        // expiry never becomes permission for an automatic repeat.
        if self.rows.values().any(Record::busy) {
            return Err(CommandError::RateLimited);
        }
        let row = self.rows.get_mut(&token.0).expect("checked record");
        let matches = match &change {
            ReceiverChange::Connect { .. } => row.observation.source.is_none(),
            ReceiverChange::Replace { previous, .. } | ReceiverChange::Disconnect { previous } => {
                row.observation.source.as_ref() == Some(previous)
            }
            _ => false,
        };
        row.change = Some(change.clone());
        if !matches {
            row.phase = Phase::Finished(ReceiverOperationState::Conflict {
                reason: "operation does not match the observed receiver selection".into(),
            });
            return Ok(Admission::Existing(row.result(token.clone())));
        }
        row.phase = Phase::Pending;
        Ok(Admission::New(Ticket {
            token: token.clone(),
            observation: row.observation.clone(),
            change,
        }))
    }

    pub(crate) fn status(
        &self,
        principal: &Principal,
        token: &ReceiverExpectation,
    ) -> Result<ReceiverOperationResult, CommandError> {
        let row = self
            .rows
            .get(&token.0)
            .filter(|r| &r.principal == principal && r.phase != Phase::Unused)
            .ok_or_else(stale)?;
        Ok(row.result(token.clone()))
    }

    pub(crate) fn retire(
        &mut self,
        principal: &Principal,
        token: &ReceiverExpectation,
    ) -> Result<Option<ReceiverOperationResult>, CommandError> {
        if token.0.len() > 64 {
            return Err(stale());
        }
        let Some(row) = self.rows.get_mut(&token.0) else {
            // Absent tokens cannot be admitted later. Busy/Unknown rows are
            // never expired; a new daemon cannot accept an old boot's token.
            return Ok(None);
        };
        if &row.principal != principal {
            return Err(CommandError::PermissionDenied);
        }
        if row.phase == Phase::Unused {
            row.phase = Phase::Finished(ReceiverOperationState::Refused {
                reason: "unused submission retired; no receiver mutation dispatched".into(),
            });
        }
        Ok(Some(row.result(token.clone())))
    }

    pub(crate) fn dispatch(&mut self, token: &ReceiverExpectation) -> Result<(), CommandError> {
        let row = self.rows.get_mut(&token.0).ok_or_else(stale)?;
        if row.phase != Phase::Pending {
            return Err(stale());
        }
        row.phase = Phase::Dispatched;
        Ok(())
    }

    pub(crate) fn retire_undispatched(
        &mut self,
        token: &ReceiverExpectation,
    ) -> Option<ReceiverOperationResult> {
        let row = self.rows.get_mut(&token.0)?;
        if row.phase != Phase::Pending {
            return None;
        }
        row.phase = Phase::Finished(ReceiverOperationState::Refused {
            reason: "worker retired before dispatch permission; no mutation sent".into(),
        });
        Some(row.result(token.clone()))
    }

    /// Retire only the reviewed episode and invalidate every unused pre-review
    /// expectation. Old completions can no longer finish a reconciled record.
    pub(crate) fn reconcile(&mut self, token: Option<&ReceiverExpectation>) {
        if let Some(token) = token {
            if let Some(row) = self.rows.get_mut(&token.0) {
                let original_unknown = match &row.phase {
                    Phase::Finished(ReceiverOperationState::Unknown { reason }) => reason.clone(),
                    _ => "original outcome remains unknown".into(),
                };
                row.phase =
                    Phase::Finished(ReceiverOperationState::Reconciled { original_unknown });
            }
        }
        self.rows.retain(|_, row| row.phase != Phase::Unused);
    }

    pub(crate) fn can_finish(&self, token: &ReceiverExpectation) -> bool {
        self.rows
            .get(&token.0)
            .is_some_and(|row| matches!(row.phase, Phase::Pending | Phase::Dispatched))
    }

    pub(crate) fn finish(
        &mut self,
        token: &ReceiverExpectation,
        state: ReceiverOperationState,
    ) -> Result<ReceiverOperationResult, CommandError> {
        if matches!(state, ReceiverOperationState::Pending) {
            return Err(stale());
        }
        let row = self.rows.get_mut(&token.0).ok_or_else(stale)?;
        if !matches!(row.phase, Phase::Pending | Phase::Dispatched) {
            return Err(stale());
        }
        if row.phase == Phase::Dispatched
            && !matches!(
                state,
                ReceiverOperationState::ReceiverConfirmed | ReceiverOperationState::Unknown { .. }
            )
        {
            return Err(stale());
        }
        row.phase = Phase::Finished(state);
        Ok(row.result(token.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_control_types::RoutingSourceRef;

    fn observed(source: Option<&str>) -> Observation<u64> {
        Observation {
            binding: 7,
            receiver: EndpointRef::new("rx", "generation"),
            rx: 2,
            source: source.map(|s| ReceiverSelection::new("Laptop", s)),
            raw_status: 8,
        }
    }
    fn replace() -> ReceiverChange {
        ReceiverChange::Replace {
            previous: ReceiverSelection::new("Laptop", "A"),
            source: RoutingSourceRef::new(EndpointRef::new("tx", "generation"), 2),
        }
    }

    #[test]
    fn retiring_unused_submission_blocks_late_dispatch_and_never_cancels_pending() {
        let now = Instant::now();
        let principal = Principal {
            uid: 42,
            peer: Some("portal".into()),
        };
        let other = Principal {
            uid: 42,
            peer: Some("other".into()),
        };
        let mut table = Table::new("boot".into());
        let token = table
            .issue(principal.clone(), now, observed(Some("A")))
            .unwrap();
        assert!(table.retire(&other, &token).is_err());
        assert!(matches!(
            table.retire(&principal, &token).unwrap().unwrap().state,
            ReceiverOperationState::Refused { .. }
        ));
        assert!(
            table
                .accept(principal.clone(), now, &token, replace())
                .is_err()
        );
        let token = table
            .issue(principal.clone(), now, observed(Some("A")))
            .unwrap();
        let _ = table
            .accept(principal.clone(), now, &token, replace())
            .unwrap();
        assert_eq!(
            table.retire(&principal, &token).unwrap().unwrap().state,
            ReceiverOperationState::Pending
        );
        table.dispatch(&token).unwrap();
        assert_eq!(
            table.retire(&principal, &token).unwrap().unwrap().state,
            ReceiverOperationState::Pending
        );
        assert!(
            table
                .retire(&principal, &ReceiverExpectation("previous-boot".into()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn gateway_principals_cannot_consume_or_inspect_each_others_expectations() {
        let now = Instant::now();
        let mut table = Table::new("boot".into());
        let alice = Principal {
            uid: 42,
            peer: Some("alice".into()),
        };
        let bob = Principal {
            uid: 42,
            peer: Some("bob".into()),
        };
        let token = table
            .issue(alice.clone(), now, observed(Some("A")))
            .unwrap();
        assert!(table.status(&alice, &token).is_err()); // unused is not an operation
        assert!(table.accept(bob.clone(), now, &token, replace()).is_err());
        assert!(table.accept(42, now, &token, replace()).is_err());
        assert!(matches!(
            table.accept(alice.clone(), now, &token, replace()).unwrap(),
            Admission::New(_)
        ));
        assert!(table.status(&bob, &token).is_err());
        assert_eq!(
            table.status(&alice, &token).unwrap().state,
            ReceiverOperationState::Pending
        );
    }

    #[test]
    fn duplicate_delivery_attaches_to_accounting_and_never_grants_second_dispatch() {
        let now = Instant::now();
        let mut table = Table::new("boot".into());
        let token = table.issue(42, now, observed(Some("A"))).unwrap();
        let Admission::New(ticket) = table.accept(42, now, &token, replace()).unwrap() else {
            panic!("missing ticket");
        };
        assert_eq!(ticket.token, token);
        assert_eq!(ticket.observation.binding, 7);
        assert_eq!(ticket.observation.raw_status, 8);
        assert_eq!(ticket.change, replace());
        assert!(matches!(
            table.accept(42, now, &token, replace()).unwrap(),
            Admission::Existing(ReceiverOperationResult {
                state: ReceiverOperationState::Pending,
                ..
            })
        ));
        table.dispatch(&token).unwrap();
        assert!(table.dispatch(&token).is_err());
        table
            .finish(&token, ReceiverOperationState::ReceiverConfirmed)
            .unwrap();
        assert!(matches!(
            table.accept(42, now, &token, replace()).unwrap(),
            Admission::Existing(ReceiverOperationResult {
                state: ReceiverOperationState::ReceiverConfirmed,
                ..
            })
        ));
        assert!(
            table
                .finish(
                    &token,
                    ReceiverOperationState::Unknown {
                        reason: "late result".into()
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn wrong_uid_boot_changed_previous_and_occupied_connect_never_get_permission() {
        let now = Instant::now();
        let mut table = Table::new("boot-a".into());
        let token = table.issue(42, now, observed(Some("A"))).unwrap();
        assert!(table.accept(43, now, &token, replace()).is_err());
        let mut other = Table::<u64>::new("boot-b".into());
        assert!(other.accept(42, now, &token, replace()).is_err());
        let connect = ReceiverChange::Connect {
            source: RoutingSourceRef::new(EndpointRef::new("tx", "generation"), 2),
        };
        assert!(matches!(
            table.accept(42, now, &token, connect.clone()).unwrap(),
            Admission::Existing(ReceiverOperationResult {
                state: ReceiverOperationState::Conflict { .. },
                ..
            })
        ));
        assert!(table.dispatch(&token).is_err());
        assert!(table.accept(42, now, &token, replace()).is_err());
        assert!(matches!(
            table.accept(42, now, &token, connect).unwrap(),
            Admission::Existing(_)
        ));
    }

    #[test]
    fn timeout_does_not_expire_unknown_into_permission_for_another_write() {
        let now = Instant::now();
        let mut table = Table::new("boot".into());
        let first = table.issue(42, now, observed(Some("A"))).unwrap();
        let second = table.issue(42, now, observed(Some("A"))).unwrap();
        table.accept(42, now, &first, replace()).unwrap();
        assert_eq!(
            table.accept(42, now, &second, replace()).unwrap_err(),
            CommandError::RateLimited
        );
        table.dispatch(&first).unwrap();
        table
            .finish(
                &first,
                ReceiverOperationState::Unknown {
                    reason: "lost reply".into(),
                },
            )
            .unwrap();
        let later = now + TTL * 10;
        let fresh = table.issue(42, later, observed(Some("A"))).unwrap();
        assert_eq!(
            table.accept(42, later, &fresh, replace()).unwrap_err(),
            CommandError::RateLimited
        );
        assert!(matches!(
            table.accept(42, later, &first, replace()).unwrap(),
            Admission::Existing(ReceiverOperationResult {
                state: ReceiverOperationState::Unknown { .. },
                ..
            })
        ));
        assert!(table.dispatch(&first).is_err());
    }

    #[test]
    fn reconcile_retires_only_the_episode_invalidates_unused_tokens_and_refuses_late_finish() {
        let now = Instant::now();
        let mut table = Table::new("boot".into());
        let first = table.issue(42, now, observed(Some("A"))).unwrap();
        let unused = table.issue(42, now, observed(Some("A"))).unwrap();
        table.accept(42, now, &first, replace()).unwrap();
        table.dispatch(&first).unwrap();
        table
            .finish(
                &first,
                ReceiverOperationState::Unknown {
                    reason: "original lost response".into(),
                },
            )
            .unwrap();
        table.reconcile(Some(&first));
        assert!(
            table
                .finish(&first, ReceiverOperationState::ReceiverConfirmed)
                .is_err()
        );
        assert!(table.accept(42, now, &unused, replace()).is_err());
        assert!(
            matches!(table.accept(42, now, &first, replace()).unwrap(), Admission::Existing(ReceiverOperationResult {
            state: ReceiverOperationState::Reconciled { original_unknown }, ..
        }) if original_unknown == "original lost response")
        );
        let fresh = table.issue(42, now, observed(Some("A"))).unwrap();
        assert!(matches!(
            table.accept(42, now, &fresh, replace()).unwrap(),
            Admission::New(_)
        ));
        table.reconcile(Some(&first));
        let next = table.issue(42, now, observed(Some("A"))).unwrap();
        assert_eq!(
            table.accept(42, now, &next, replace()).unwrap_err(),
            CommandError::RateLimited
        );
    }

    #[test]
    fn retired_pending_is_recoverable_but_dispatched_never_clears_without_review() {
        let now = Instant::now();
        let mut table = Table::new("boot".into());
        let pending = table.issue(42, now, observed(Some("A"))).unwrap();
        table.accept(42, now, &pending, replace()).unwrap();
        assert!(table.retire_undispatched(&pending).is_some());
        assert!(table.dispatch(&pending).is_err());
        let dispatched = table.issue(42, now, observed(Some("A"))).unwrap();
        table.accept(42, now, &dispatched, replace()).unwrap();
        table.dispatch(&dispatched).unwrap();
        assert!(table.retire_undispatched(&dispatched).is_none());
    }

    #[test]
    fn observations_expire_capacity_is_bounded_and_counter_cannot_wrap() {
        let now = Instant::now();
        let mut table = Table::new("boot".into());
        let first = table.issue(42, now, observed(Some("A"))).unwrap();
        for _ in 1..CAPACITY {
            table.issue(42, now, observed(Some("A"))).unwrap();
        }
        assert!(table.issue(42, now, observed(Some("A"))).is_err());
        assert!(table.accept(42, now + TTL, &first, replace()).is_err());
        table.serial = u64::MAX;
        assert!(table.issue(42, now + TTL, observed(None)).is_err());
        assert_eq!(table.serial, u64::MAX);
    }
}
