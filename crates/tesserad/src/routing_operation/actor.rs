//! Actor-owned operation admission. All methods run on the existing state actor.
use super::{
    Expected, Plan, RunResult,
    accounting::{Admission, Observation, Principal, Table, Ticket},
};
use crate::{routing_inventory::Inventory, routing_receiver::Target, runtime_host};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Instant;
use tessera_control_types::{
    CommandError, ReceiverChange, ReceiverExpectation, ReceiverOperationResult,
    ReceiverOperationState, Reply, RoutingReceiverPage,
};
use tessera_state::StateEngine;

#[derive(Debug, Clone)]
pub(crate) struct Binding {
    target: Target,
    context: (String, u64),
    expected: Arc<Vec<Expected>>,
    geometry: u16,
    /// Another device's receiver, written over its own ARC endpoint. It
    /// applies its own locks; no local recovery fence covers it.
    remote: bool,
}

#[derive(Debug)]
pub(crate) struct Job {
    pub ticket: Ticket<Binding>,
    pub plan: Plan,
    pub target: Target,
    pub deadline: Instant,
    pub exited: Arc<AtomicBool>,
}

#[derive(Debug)]
pub(crate) enum Start {
    New(Arc<Job>),
    Existing(ReceiverOperationResult),
}

pub(crate) struct Actor {
    pub(super) table: Table<Binding>,
    pub(super) active_job: Option<Arc<Job>>,
    pub(super) recovery: super::recovery::Reviews,
    pub(super) carriers: super::carriers::Reviews,
}
impl Actor {
    pub(crate) fn retire(
        &mut self,
        principal: &Principal,
        operation: &ReceiverExpectation,
    ) -> Result<Reply, CommandError> {
        self.table
            .retire(principal, operation)
            .map(|result| result.map_or(Reply::Done, Reply::ReceiverOperation))
    }
    pub(crate) fn status(
        &self,
        principal: &Principal,
        operation: &ReceiverExpectation,
    ) -> Result<Reply, CommandError> {
        self.table
            .status(principal, operation)
            .map(Reply::ReceiverOperation)
    }
    pub(crate) fn new() -> std::io::Result<Self> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
        Ok(Self {
            table: Table::new(bytes.iter().map(|b| format!("{b:02x}")).collect()),
            active_job: None,
            recovery: super::recovery::Reviews::new(bytes),
            carriers: super::carriers::Reviews::new(bytes),
        })
    }

    /// Recover a retired pre-dispatch worker even when its completion could
    /// not enter the full queue. Never infer retirement from a deadline alone.
    pub(crate) fn retire_undispatched(&mut self, engine: &StateEngine) {
        if engine.receiver_operation_uncertain() {
            return;
        }
        let Some(job) = &self.active_job else {
            return;
        };
        if job.exited.load(std::sync::atomic::Ordering::Acquire)
            && Instant::now() >= job.deadline + std::time::Duration::from_secs(3)
        {
            if let Some(result) = self.table.retire_undispatched(&job.ticket.token) {
                engine
                    .event_bus()
                    .publish(&tessera_control_types::Event::ReceiverOperationChanged { result });
            }
        }
    }

    pub(crate) fn observe(
        &mut self,
        principal: Principal,
        engine: &StateEngine,
        inventory: &mut Inventory,
        runtime: &runtime_host::Authority,
        target: &Target,
        page: &mut RoutingReceiverPage,
    ) {
        let eligible = (|| {
            if engine.receiver_operation_uncertain() {
                return Err(runtime_host::unavailable(
                    "an uncertain operation requires reconciliation",
                ));
            }
            let (remote, geometry) = match inventory.operation_geometry(engine, target)? {
                Some(count) => (false, count),
                // A remote receiver's count is that of one complete page.
                None if page.first == 1 && page.next_first.is_none() => {
                    (true, page.channels.iter().map(|c| c.id).max().unwrap_or(0))
                }
                None => {
                    return Err(runtime_host::unavailable(
                        "remote receivers with more than one page are not supported yet",
                    ));
                }
            };
            if !remote && subscriptions_locked(engine) {
                return Err(locked());
            }
            // The receiver device's access lock: this host's from the state,
            // another device's as it reports it (`device-lock.md` §9.2, §9.3
            // L14): a locked receiver gets no write.
            if !remote {
                engine.check_local_receiver_access()?;
            } else if page.access_locked == Some(true) {
                return Err(runtime_host::unavailable(
                    "the receiver device is locked against changes (device access lock); \
                     unlock it on that device first",
                ));
            }
            let context = if remote {
                ("remote".to_owned(), 0)
            } else {
                runtime.operation_context(engine)?
            };
            if page.channels.is_empty()
                || page.channels.len() > 255
                || page.channels.iter().any(|c| {
                    c.id > geometry
                        || c.source
                            .as_ref()
                            .is_some_and(|s| s.tx_device.len() > 255 || s.tx_channel.len() > 255)
                })
            {
                return Err(runtime_host::unavailable(
                    "receiver observation is outside qualified geometry",
                ));
            }
            let expected = Arc::new(
                page.channels
                    .iter()
                    .map(|c| Expected {
                        rx: c.id,
                        source: c.source.clone(),
                        raw_status: c.raw_status,
                    })
                    .collect(),
            );
            Ok(Binding {
                target: target.clone(),
                context,
                expected,
                geometry,
                remote,
            })
        })();
        match eligible {
            Ok(binding) => {
                page.fan_out = binding.remote || super::source_effects::fan_out();
                for row in &mut page.channels {
                    row.expectation = self
                        .table
                        .issue(
                            principal.clone(),
                            Instant::now(),
                            Observation {
                                binding: binding.clone(),
                                receiver: target.endpoint.clone(),
                                rx: row.id,
                                source: row.source.clone(),
                                raw_status: row.raw_status,
                            },
                        )
                        .ok();
                }
            }
            Err(error) => {
                for row in &mut page.channels {
                    row.expectation = None;
                }
                page.operation_unavailable = Some(error.to_string());
            }
        }
    }

    fn validate(
        engine: &StateEngine,
        inventory: &mut Inventory,
        runtime: &runtime_host::Authority,
        ticket: &Ticket<Binding>,
    ) -> Result<Option<tessera_control_types::ReceiverSelection>, CommandError> {
        // Recheck old expectations at admission and immediately before durable
        // dispatch permission. Completion/history must retain their original
        // outcome, even when this basis is unavailable.
        let binding = &ticket.observation.binding;
        // The local receiver's device access lock comes first (`device-lock.md`
        // §9.2 L5); a remote receiver applies its own.
        if !binding.remote {
            engine.check_local_receiver_access()?;
        }
        if binding
            .expected
            .iter()
            .find(|e| e.rx == ticket.observation.rx)
            .is_none_or(|e| {
                e.raw_status != ticket.observation.raw_status
                    || e.source != ticket.observation.source
            })
        {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        if !binding.remote && subscriptions_locked(engine) {
            return Err(locked());
        }
        let geometry = inventory.operation_geometry(engine, &binding.target)?;
        let stale = if binding.remote {
            geometry.is_some()
        } else {
            runtime.operation_context(engine)? != binding.context
                || geometry != Some(binding.geometry)
        };
        if stale {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        let desired = match &ticket.change {
            ReceiverChange::Connect { source } | ReceiverChange::Replace { source, .. } => {
                Some(inventory.operation_source(engine, source, !binding.remote)?)
            }
            ReceiverChange::Disconnect { .. } => None,
            _ => return Err(runtime_host::unavailable("unknown receiver change")),
        };
        if !binding.remote {
            super::source_effects::admit(
                &binding.expected,
                ticket.observation.rx,
                desired.as_ref(),
            )?;
        }
        Ok(desired)
    }

    pub(crate) fn start(
        &mut self,
        principal: Principal,
        token: &ReceiverExpectation,
        change: ReceiverChange,
        engine: &StateEngine,
        inventory: &mut Inventory,
        runtime: &runtime_host::Authority,
    ) -> Result<Start, CommandError> {
        let ticket = match self
            .table
            .accept(principal, Instant::now(), token, change)?
        {
            Admission::Existing(result) => return Ok(Start::Existing(result)),
            Admission::New(ticket) => ticket,
        };
        let desired = match Self::validate(engine, inventory, runtime, &ticket) {
            Ok(desired) => desired,
            Err(error) => {
                return Ok(Start::Existing(self.table.finish(
                    token,
                    ReceiverOperationState::Refused {
                        reason: error.to_string(),
                    },
                )?));
            }
        };
        let binding = &ticket.observation.binding;
        let plan = Plan {
            profile: binding.target.profile,
            rx_count: binding.geometry,
            rx: ticket.observation.rx,
            expected: binding.expected.as_ref().clone(),
            desired,
        };
        let job = Arc::new(Job {
            target: binding.target.clone(),
            ticket,
            plan,
            deadline: Instant::now() + std::time::Duration::from_secs(3),
            exited: Arc::new(AtomicBool::new(false)),
        });
        self.active_job = Some(job.clone());
        Ok(Start::New(job))
    }

    pub(crate) fn dispatch(
        &mut self,
        job: &Job,
        engine: &mut StateEngine,
        inventory: &mut Inventory,
        runtime: &runtime_host::Authority,
    ) -> Result<(), CommandError> {
        if Instant::now() >= job.deadline {
            return Err(CommandError::ReceiverReadTimeout);
        }
        if Self::validate(engine, inventory, runtime, &job.ticket)? != job.plan.desired {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        // Consume accounting before the durable write; no error or cancellation
        // after this point can reopen the token.
        self.table.dispatch(&job.ticket.token)?;
        if job.ticket.observation.binding.remote {
            // The remote receiver holds the state; there is no local fence.
            return Ok(());
        }
        let first = engine
            .model()
            .devices
            .first()
            .ok_or(CommandError::RoutingSnapshotExpired)?;
        engine.begin_receiver_mutation(tessera_control_types::ReceiverOperationContext {
            operation: job.ticket.token.clone(),
            revision: 1,
            phase: tessera_control_types::ReceiverOperationPhase::DispatchPermitted,
            receiver_identity: first.wire_device_id.to_hex(),
            device: first.key.clone(),
            interface: job.target.interface.clone(),
            address: job.target.source.to_string(),
            rx_channels: first.rx_channels,
            sample_rate: first.sample_rate,
            encoding: first.encoding,
            affected_rx: job.plan.rx,
            expected: job
                .plan
                .expected
                .iter()
                .map(|e| tessera_control_types::ReceiverRecoverySelection {
                    rx: e.rx,
                    source: e.source.clone(),
                    raw_status: e.raw_status,
                })
                .collect(),
            desired: job.plan.desired.clone(),
            runtime_incarnation: job.ticket.observation.binding.context.0.clone(),
            host_protection_revision: job.ticket.observation.binding.context.1,
        })
    }

    pub(crate) fn finish(
        &mut self,
        job: &Job,
        outcome: RunResult,
        engine: &mut StateEngine,
        inventory: &mut Inventory,
        runtime: &runtime_host::Authority,
    ) -> Result<Reply, CommandError> {
        use tessera_control::receiver::{Error, Outcome};
        if !self.table.can_finish(&job.ticket.token) {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        let state = match outcome {
            RunResult::Observed(Outcome::NamesMatch) => {
                // Completion validates receiver authority, not the continued
                // presence of a source advertisement that was already admitted.
                match (|| {
                    let binding = &job.ticket.observation.binding;
                    if binding.remote {
                        // The receiver it wrote to must still be the one resolved.
                        return match inventory.receiver_target(engine, &job.target.endpoint) {
                            Ok(target) if target == job.target => Ok(()),
                            _ => Err(CommandError::RoutingSnapshotExpired),
                        };
                    }
                    let current = runtime.recovery_target(engine)?;
                    let target = current.target;
                    let geometry = current.channels;
                    if runtime.operation_context(engine)? != binding.context
                        || geometry != binding.geometry
                        || target.interface != job.target.interface
                        || target.source != job.target.source
                        || target.peer != job.target.peer
                        || target.profile != job.target.profile
                    {
                        return Err(CommandError::RoutingSnapshotExpired);
                    }
                    engine.confirm_receiver_mutation(&job.ticket.token)
                })() {
                    Ok(()) => ReceiverOperationState::ReceiverConfirmed,
                    Err(error) => ReceiverOperationState::Unknown {
                        reason: error.to_string(),
                    },
                }
            }
            RunResult::Observed(Outcome::Unknown(error))
                if job.ticket.observation.binding.remote =>
            {
                ReceiverOperationState::Unknown {
                    reason: format!(
                        "the receiver did not confirm the change ({error:?}); it may be locked or changed elsewhere. Refresh before choosing again"
                    ),
                }
            }
            RunResult::Observed(Outcome::Unknown(error)) => ReceiverOperationState::Unknown {
                reason: format!("{error:?}; no retry was sent"),
            },
            RunResult::Observed(Outcome::Refused(Error::SelectionChanged)) => {
                ReceiverOperationState::Conflict {
                    reason: "receiver selections changed; refresh before choosing again".into(),
                }
            }
            RunResult::Observed(Outcome::Refused(error)) => ReceiverOperationState::Refused {
                reason: format!("{error:?}; no mutation sent"),
            },
            RunResult::Stopped => ReceiverOperationState::Refused {
                reason: "operation worker exited before completing".into(),
            },
            RunResult::Refused(error) => ReceiverOperationState::Refused {
                reason: error.to_string(),
            },
        };
        let state = if !job.ticket.observation.binding.remote
            && engine.receiver_operation_uncertain()
            && !matches!(state, ReceiverOperationState::Unknown { .. })
        {
            ReceiverOperationState::Unknown {
                reason: "dispatch permission was consumed; reconcile before another operation"
                    .into(),
            }
        } else {
            state
        };
        let result = self.table.finish(&job.ticket.token, state)?;
        engine
            .event_bus()
            .publish(&tessera_control_types::Event::ReceiverOperationChanged {
                result: result.clone(),
            });
        Ok(Reply::ReceiverOperation(result))
    }
}

/// This host's receiver refuses subscription changes by configuration.
fn subscriptions_locked(engine: &StateEngine) -> bool {
    engine
        .config()
        .config
        .control
        .locked
        .contains(&tessera_state::config::ControlLock::Subscriptions)
}

fn locked() -> CommandError {
    runtime_host::unavailable(
        "routing on this receiver is locked (`[control] locked = [\"subscriptions\"]`)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_control_types::{
        DeviceKey, EndpointRef, ReceiverOperationContext, ReceiverOperationPhase,
        ReceiverRecoverySelection, ReceiverSelection, RoutingSourceRef,
    };

    #[test]
    fn post_dispatch_worker_panic_becomes_unknown_and_late_success_cannot_clear_it() {
        let root =
            std::env::temp_dir().join(format!("tessera-operation-panic-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let (mut engine, _) = StateEngine::start(
            tessera_state::Environment::new("panic-test"),
            tessera_state::Paths::new(
                tessera_state::ConfigPaths::new(root.join("config"), root.join("conf.d")),
                root.join("state"),
            ),
            Arc::new(tessera_state::ManualClock::new()),
        )
        .unwrap();
        let mut actor = Actor::new().unwrap();
        let target = Target {
            endpoint: EndpointRef::new("local", "run"),
            label: "Speaker".into(),
            interface: "audio0".into(),
            source: "192.0.2.2".parse().unwrap(),
            peer: "192.0.2.2:4440".parse().unwrap(),
            profile: tessera_control::arc::Legacy::from_advertised_version("2.7.41").unwrap(),
        };
        let expected: Vec<_> = (1..=2)
            .map(|rx| Expected {
                rx,
                source: None,
                raw_status: 0,
            })
            .collect();
        let token = actor
            .table
            .issue(
                7,
                Instant::now(),
                Observation {
                    binding: Binding {
                        target: target.clone(),
                        context: ("run".into(), 0),
                        geometry: 2,
                        remote: false,
                        expected: Arc::new(expected.clone()),
                    },
                    receiver: target.endpoint.clone(),
                    rx: 2,
                    source: None,
                    raw_status: 0,
                },
            )
            .unwrap();
        let Admission::New(ticket) = actor
            .table
            .accept(
                7,
                Instant::now(),
                &token,
                ReceiverChange::Connect {
                    source: RoutingSourceRef::new(EndpointRef::new("tx", "run"), 1),
                },
            )
            .unwrap()
        else {
            panic!("missing ticket")
        };
        let job = Job {
            ticket,
            plan: Plan {
                profile: target.profile,
                rx_count: 2,
                rx: 2,
                expected,
                desired: Some(ReceiverSelection::new("source", "B")),
            },
            target,
            deadline: Instant::now(),
            exited: Arc::new(AtomicBool::new(true)),
        };
        actor.table.dispatch(&token).unwrap();
        engine
            .begin_receiver_mutation(ReceiverOperationContext {
                operation: token.clone(),
                revision: 1,
                phase: ReceiverOperationPhase::DispatchPermitted,
                receiver_identity: engine.model().devices[0].wire_device_id.to_hex(),
                device: DeviceKey("main".into()),
                interface: "audio0".into(),
                address: "192.0.2.2".into(),
                rx_channels: 2,
                sample_rate: 48000,
                encoding: 24,
                affected_rx: 2,
                expected: (1..=2)
                    .map(|rx| ReceiverRecoverySelection {
                        rx,
                        source: None,
                        raw_status: 0,
                    })
                    .collect(),
                desired: job.plan.desired.clone(),
                runtime_incarnation: "run".into(),
                host_protection_revision: 0,
            })
            .unwrap();
        let mut inventory = Inventory::new();
        let runtime = runtime_host::Authority::default();
        let reply = actor
            .finish(
                &job,
                RunResult::Stopped,
                &mut engine,
                &mut inventory,
                &runtime,
            )
            .unwrap();
        assert!(
            matches!(reply, Reply::ReceiverOperation(result) if matches!(result.state, ReceiverOperationState::Unknown { .. }))
        );
        // A newly unavailable capability must not turn a previously dispatched
        // duplicate into an unsent refusal, or reopen it for execution.
        let Start::Existing(duplicate) = actor
            .start(
                7.into(),
                &token,
                job.ticket.change.clone(),
                &engine,
                &mut inventory,
                &runtime,
            )
            .unwrap()
        else {
            panic!("dispatched duplicate launched another worker");
        };
        assert!(matches!(
            duplicate.state,
            ReceiverOperationState::Unknown { .. }
        ));
        assert!(actor.table.busy());
        assert!(engine.receiver_operation_uncertain());
        let before = engine.intent().clone();
        assert!(
            actor
                .finish(
                    &job,
                    RunResult::Observed(tessera_control::receiver::Outcome::NamesMatch),
                    &mut engine,
                    &mut inventory,
                    &runtime
                )
                .is_err()
        );
        assert_eq!(engine.intent(), &before);
        drop(engine);
        std::fs::remove_dir_all(root).unwrap();
    }
}
