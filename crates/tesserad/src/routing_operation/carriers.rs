//! Explicit, boot/uid/snapshot-bound retired-carrier review on the state actor.
//! No sockets, ARC command, content match or automatic ownership transfer.
use super::{actor::Actor, recovery::bounded};
use crate::runtime_host;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use tessera_control_types::{
    CommandError, ReceiverExpectation, Reply, RuntimeCarrierRecoveryInspection,
    RuntimeCarrierRecoveryRow,
};
use tessera_state::{StateEngine, engine::RuntimeCarrierReview};

const TTL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Release,
    NextRun,
}

struct Review {
    uid: u32,
    created: Instant,
    snapshot: RuntimeCarrierReview,
    purpose: Purpose,
}

pub(super) struct Reviews {
    namespace: String,
    serial: u64,
    rows: BTreeMap<String, Review>,
}

impl Reviews {
    pub(super) fn new(namespace: [u8; 16]) -> Self {
        Self {
            namespace: namespace.iter().map(|b| format!("{b:02x}")).collect(),
            serial: 0,
            rows: BTreeMap::new(),
        }
    }
    fn issue(
        &mut self,
        uid: u32,
        snapshot: RuntimeCarrierReview,
        purpose: Purpose,
    ) -> Result<ReceiverExpectation, CommandError> {
        self.rows
            .retain(|_, r| (r.uid != uid || r.purpose != purpose) && r.created.elapsed() < TTL);
        if self.rows.len() >= 32 {
            return Err(CommandError::RateLimited);
        }
        self.serial = self
            .serial
            .checked_add(1)
            .ok_or(CommandError::RateLimited)?;
        let token = ReceiverExpectation(format!("{}-c{:016x}", self.namespace, self.serial));
        self.rows.insert(
            token.0.clone(),
            Review {
                uid,
                created: Instant::now(),
                snapshot,
                purpose,
            },
        );
        Ok(token)
    }
    fn consume(
        &mut self,
        uid: u32,
        token: &ReceiverExpectation,
        purpose: Purpose,
    ) -> Result<RuntimeCarrierReview, CommandError> {
        if token.0.len() > 64
            || self
                .rows
                .get(&token.0)
                .is_none_or(|r| r.uid != uid || r.purpose != purpose)
        {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        let row = self.rows.remove(&token.0).expect("checked review");
        if row.created.elapsed() >= TTL {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        Ok(row.snapshot)
    }
}

impl Actor {
    pub(crate) fn grant_epoch(&self) -> &str {
        &self.carriers.namespace
    }

    pub(crate) fn grant_start_ready(&self, engine: &StateEngine) -> Result<(), CommandError> {
        if engine.intent().runtime_carrier_grant.is_some() {
            self.retired()?;
            if self.table.busy() {
                return Err(CommandError::RateLimited);
            }
        }
        Ok(())
    }
    pub(crate) fn inspect_carriers(
        &mut self,
        uid: u32,
        engine: &StateEngine,
        runtime: &runtime_host::Authority,
    ) -> Result<Reply, CommandError> {
        let snapshot = runtime.carrier_review(engine)?;
        let configured_carriers = snapshot.configured.clone();
        bounded(&(
            &snapshot.flows,
            &snapshot.origins,
            &snapshot.binding,
            &snapshot.saved,
            &configured_carriers,
        ))?;
        let common = self
            .table
            .busy()
            .then(|| "A receiver operation is still unresolved".to_owned())
            .or_else(|| self.retired().err().map(|e| e.to_string()));
        let unavailable = snapshot.unavailable.clone().or_else(|| common.clone());
        let review = if unavailable.is_none() {
            Some(
                self.carriers
                    .issue(uid, snapshot.clone(), Purpose::Release)?,
            )
        } else {
            None
        };
        let next_run_unavailable = snapshot.grant_unavailable.clone().or(common);
        let next_run_review = if next_run_unavailable.is_none() {
            Some(
                self.carriers
                    .issue(uid, snapshot.clone(), Purpose::NextRun)?,
            )
        } else {
            None
        };
        let saved = snapshot
            .saved
            .iter()
            .map(|s| tessera_control_types::ReceiverRecoverySelection {
                rx: s.rx_channel,
                source: Some(tessera_control_types::ReceiverSelection::new(
                    &s.tx_device,
                    &s.tx_channel,
                )),
                raw_status: 0,
            })
            .collect();
        let carriers = snapshot
            .flows
            .into_iter()
            .enumerate()
            .map(|(index, carrier)| {
                let occurrence = u32::try_from(index).expect("bounded state occurrence");
                RuntimeCarrierRecoveryRow {
                    occurrence,
                    carrier,
                    origin_runtime: snapshot.origins[index].invocation.clone(),
                    releasable: snapshot.releasable.contains(&occurrence),
                }
            })
            .collect();
        Ok(Reply::RuntimeCarrierRecovery(
            RuntimeCarrierRecoveryInspection {
                current_runtime: snapshot.invocation,
                protection_revision: snapshot.protection_revision,
                carriers,
                review,
                unavailable,
                configured_carriers,
                saved,
                next_run_occurrences: snapshot.grant_occurrences,
                next_run_review,
                next_run_unavailable,
                next_run_authorized: engine.intent().runtime_carrier_grant.is_some(),
            },
        ))
    }

    pub(crate) fn release_carriers(
        &mut self,
        uid: u32,
        token: &ReceiverExpectation,
        engine: &mut StateEngine,
        runtime: &runtime_host::Authority,
    ) -> Result<Reply, CommandError> {
        // Consume before persistence; a lost reply never replays this release.
        let reviewed = self.carriers.consume(uid, token, Purpose::Release)?;
        self.retired()?;
        if self.table.busy() {
            return Err(CommandError::RateLimited);
        }
        if runtime.carrier_review(engine)? != reviewed {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        runtime.release_carriers(engine, &reviewed)?;
        self.table.invalidate_unused();
        self.carriers.rows.clear();
        Ok(Reply::Done)
    }

    pub(crate) fn grant_next_run(
        &mut self,
        uid: u32,
        token: &ReceiverExpectation,
        engine: &mut StateEngine,
        runtime: &runtime_host::Authority,
    ) -> Result<Reply, CommandError> {
        let reviewed = self.carriers.consume(uid, token, Purpose::NextRun)?;
        self.retired()?;
        if self.table.busy() {
            return Err(CommandError::RateLimited);
        }
        runtime.grant_next_run(engine, &reviewed, self.grant_epoch(), uid, &token.0)?;
        self.table.invalidate_unused();
        self.carriers.rows.clear();
        Ok(Reply::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot() -> RuntimeCarrierReview {
        RuntimeCarrierReview {
            invocation: "1".repeat(32),
            protection_revision: 3,
            binding: "reviewed".into(),
            flows: vec![],
            origins: vec![],
            releasable: vec![0],
            unavailable: None,
            saved: vec![],
            grant_occurrences: vec![0],
            grant_unavailable: None,
            configured: vec![],
        }
    }
    #[test]
    fn review_is_uid_boot_and_single_use_scoped_and_later_review_retires_the_old() {
        let mut a = Reviews::new([1; 16]);
        let mut b = Reviews::new([2; 16]);
        let first = a.issue(1000, snapshot(), Purpose::Release).unwrap();
        assert!(a.consume(1001, &first, Purpose::Release).is_err());
        assert!(b.consume(1000, &first, Purpose::Release).is_err());
        assert_eq!(
            a.consume(1000, &first, Purpose::Release).unwrap(),
            snapshot()
        );
        assert!(a.consume(1000, &first, Purpose::Release).is_err());
        let old = a.issue(1000, snapshot(), Purpose::Release).unwrap();
        let new = a.issue(1000, snapshot(), Purpose::Release).unwrap();
        assert!(a.consume(1000, &old, Purpose::Release).is_err());
        assert!(a.consume(1000, &new, Purpose::Release).is_ok());
    }
    #[test]
    fn expired_review_is_consumed_and_never_authorizes() {
        let mut table = Reviews::new([1; 16]);
        let token = table.issue(1000, snapshot(), Purpose::Release).unwrap();
        table.rows.get_mut(&token.0).unwrap().created = Instant::now() - TTL;
        assert!(table.consume(1000, &token, Purpose::Release).is_err());
        assert!(table.consume(1000, &token, Purpose::Release).is_err());
    }

    #[test]
    fn next_run_review_is_distinct_from_release_and_scoped_to_uid_and_epoch() {
        let mut table = Reviews::new([1; 16]);
        let mut other_boot = Reviews::new([2; 16]);
        let release = table.issue(1000, snapshot(), Purpose::Release).unwrap();
        let grant = table.issue(1000, snapshot(), Purpose::NextRun).unwrap();
        assert!(table.consume(1000, &grant, Purpose::Release).is_err());
        assert!(table.consume(1001, &grant, Purpose::NextRun).is_err());
        assert!(other_boot.consume(1000, &grant, Purpose::NextRun).is_err());
        assert!(table.consume(1000, &release, Purpose::Release).is_ok());
        assert!(table.consume(1000, &grant, Purpose::NextRun).is_ok());
        assert!(table.consume(1000, &grant, Purpose::NextRun).is_err());
        let expired = table.issue(1000, snapshot(), Purpose::NextRun).unwrap();
        table.rows.get_mut(&expired.0).unwrap().created = Instant::now() - TTL;
        assert!(table.consume(1000, &expired, Purpose::NextRun).is_err());
    }
}
