//! Explicit private fixture premises. These Rust-only types have no serde or
//! control-request representation. They establish no production process proof.
//! Candidates are diagnostic, effect is always unverified, and no executor exists.
use super::super::observation::LegacyPolicy;
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestBinding {
    pub device: String,
    pub interface: String,
    pub address: Ipv4Addr,
}
impl TestBinding {
    fn into_binding(self) -> Result<Binding, TestError> {
        if self
            .device
            .len()
            .checked_add(self.interface.len())
            .is_none_or(|n| n > MAX_TEXT_BYTES)
        {
            return Err(TestError::Capacity);
        }
        let binding = Binding {
            device: self.device,
            interface: self.interface,
            address: self.address,
        };
        binding.valid().then_some(binding).ok_or(TestError::Refused)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestHolder {
    lifetime: Lifetime,
    witness: launch::PreExecWitness,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestRevision(Revision);
#[derive(Debug, Clone)]
pub struct TestView(ExpectedView);
#[derive(Debug, Clone)]
pub struct TestEndpoint {
    pub key: u128,
    pub incarnation: u128,
    pub source_channel: u16,
    pub peer: Ipv4Addr,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestDispatch {
    Done,
    Failed,
    Unknown,
}
impl From<TestDispatch> for DispatchOutcome {
    fn from(value: TestDispatch) -> Self {
        match value {
            TestDispatch::Done => Self::Done,
            TestDispatch::Failed => Self::Failed,
            TestDispatch::Unknown => Self::Unknown,
        }
    }
}
#[derive(Debug)]
pub enum TestEvent {
    /// Supplies prior absence, a trusted launcher and pre-exec witness as TEST
    /// premises. None is inferred from the process number, environment or socket.
    RegisterTestPremise {
        binding: TestBinding,
        process_incarnation: u128,
        now: u64,
    },
    Inventory(Vec<TestEndpoint>),
    View(TestBinding),
    Operator {
        view: TestView,
        requested: Vec<(u16, Option<u128>)>,
        now: u64,
        valid_for: u64,
    },
    /// Liveness and holder association are fixture inputs, never connector claims.
    Report {
        holder: Option<TestHolder>,
        live: bool,
        binding: TestBinding,
        snapshot: Vec<FlowSpec>,
        now: u64,
        legacy: bool,
        aggregate_fault: bool,
    },
    Dispatch {
        revision: TestRevision,
        result: TestDispatch,
    },
    EndLifetime,
    Inspect,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestError {
    Unavailable,
    Stopped,
    Capacity,
    Refused,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestOutcome {
    Admitted,
    Unchanged,
    Refused,
    RolledBack,
    Expired,
    Revoked,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestRecognition {
    AlreadyPresent,
    Vacant,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestTerminal {
    pub revision: TestRevision,
    pub outcome: TestOutcome,
    pub recognition: Option<TestRecognition>,
    pub dispatch_failed_by: Option<TestRevision>,
}
impl From<&Terminal> for TestTerminal {
    fn from(value: &Terminal) -> Self {
        let outcome = match value.outcome {
            AttemptOutcome::Admitted => TestOutcome::Admitted,
            AttemptOutcome::Unchanged => TestOutcome::Unchanged,
            AttemptOutcome::Refused(_) => TestOutcome::Refused,
            AttemptOutcome::RolledBack(_) => TestOutcome::RolledBack,
            AttemptOutcome::Expired => TestOutcome::Expired,
            AttemptOutcome::Revoked(_) => TestOutcome::Revoked,
        };
        Self {
            revision: TestRevision(value.revision),
            outcome,
            recognition: value.recognition.as_ref().map(|r| match r {
                StoredRecognition::StoredAlreadyPresent => TestRecognition::AlreadyPresent,
                StoredRecognition::StoredVacant => TestRecognition::Vacant,
            }),
            dispatch_failed_by: match value.outcome {
                AttemptOutcome::Revoked(Revoke::DispatchFailed(origin)) => {
                    Some(TestRevision(origin))
                }
                _ => None,
            },
        }
    }
}
#[derive(Debug)]
pub struct TestState {
    pub managed_bindings: usize,
    pub unverified_reports: usize,
    pub actual_projection: Vec<FlowSpec>,
    pub generation: u64,
    pub active: bool,
    pub tx_open: bool,
    pub pending: usize,
    pub saved: usize,
    pub fences: usize,
    pub terminals: Vec<TestTerminal>,
    pub would_dispatch_count: u64,
    pub observed_retained: bool,
    pub view_revision: u64,
}
#[derive(Debug)]
pub struct TestObservation {
    pub candidate: Vec<FlowSpec>,
    pub terminals: Vec<TestTerminal>,
    pub context_refused: bool,
    pub legacy_refusals: usize,
    pub waiting: usize,
}
#[derive(Debug)]
pub enum TestReply {
    Ack,
    Registered(TestHolder),
    View(TestView),
    WouldDispatch {
        revision: TestRevision,
        result: TestDispatch,
    },
    Observation(TestObservation),
    Dispatch {
        revoked: Option<TestRevision>,
    },
    State(TestState),
}

#[derive(Debug)]
pub(super) struct Bookkeeping {
    launch: launch::LaunchIssuer,
    portal: PortalIssuer,
    would_dispatch: u64,
    // Sticky for this observer/daemon instance, outside resettable Authority.
    // No observation, foreign write, reload or lifetime end can release it.
    managed: Vec<Binding>,
}
impl Bookkeeping {
    pub(super) fn new(instance: launch::DaemonInstance) -> Self {
        Self {
            launch: launch::LaunchIssuer::new(instance),
            portal: PortalIssuer::new(instance),
            would_dispatch: 0,
            managed: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub(super) struct PremiseWorld {
    pub(super) authority: Authority,
    bookkeeping: Bookkeeping,
}
impl PremiseWorld {
    pub(super) fn is_managed(&self) -> bool {
        !self.bookkeeping.managed.is_empty()
            || self.authority.guards.session.is_some()
            || !self.authority.portal.saved.is_empty()
            || !self.authority.portal.fences.is_empty()
    }
    fn new(authority: Authority) -> Self {
        let bookkeeping = Bookkeeping::new(authority.guards.instance());
        Self {
            authority,
            bookkeeping,
        }
    }
    pub(super) fn accepted(
        &mut self,
        command: &Command,
        config: &Config,
        devices: &[crate::model::DeviceModel],
    ) -> Result<(), ()> {
        match command {
            Command::SetProtocolFlows { flows } => self
                .authority
                .foreign_write(flows, WriteDisposition::Accepted)
                .map_err(|_| ()),
            _ => self
                .authority
                .reload(config, devices)
                .map(|next| self.authority = next)
                .map_err(|_| ()),
        }
    }
    fn event(
        &mut self,
        event: TestEvent,
        config: &Config,
        env: &Environment,
    ) -> Result<TestReply, TestError> {
        match event {
            TestEvent::RegisterTestPremise {
                binding,
                process_incarnation,
                now,
            } => {
                let binding = binding.into_binding()?;
                // This bounded exercise is confined to the existing first
                // logical device. The binding itself remains a trusted premise.
                if config
                    .devices
                    .first()
                    .is_none_or(|d| d.id != binding.device)
                {
                    return Err(TestError::Refused);
                }
                if self.authority.guards.session.is_some() {
                    return Err(TestError::Refused);
                }
                // The trusted test-launch selection marks management BEFORE
                // registration. Missing process facts cannot choose fallback.
                if !self.bookkeeping.managed.contains(&binding) {
                    let text = self
                        .bookkeeping
                        .managed
                        .iter()
                        .chain(std::iter::once(&binding))
                        .try_fold(0_usize, |bytes, b| {
                            bytes
                                .checked_add(b.device.len())
                                .and_then(|n| n.checked_add(b.interface.len()))
                        });
                    if self.bookkeeping.managed.len() >= 64
                        || text.is_none_or(|n| n > MAX_TEXT_BYTES / 16)
                    {
                        return Err(TestError::Capacity);
                    }
                    self.bookkeeping.managed.push(binding.clone());
                }
                if process_incarnation == 0 {
                    return Err(TestError::Refused);
                }
                let attempt = launch::LaunchAttempt(process_incarnation);
                let lifetime = self
                    .authority
                    .reserve_launch(
                        &mut self.bookkeeping.launch,
                        &binding,
                        attempt,
                        (now, 100),
                        launch::PriorRuntimeAbsent,
                    )
                    .map_err(|_| TestError::Refused)?;
                self.authority
                    .register_launch(
                        Identity::Verified(lifetime),
                        &binding,
                        launch::PreExecWitness {
                            lifetime,
                            attempt,
                            process: process_incarnation,
                        },
                        now,
                    )
                    .map_err(|_| TestError::Refused)?;
                // No network facts are supplied by this facade. TX stays closed.
                Ok(TestReply::Registered(TestHolder {
                    lifetime,
                    witness: launch::PreExecWitness {
                        lifetime,
                        attempt,
                        process: process_incarnation,
                    },
                }))
            }
            TestEvent::Inventory(entries) => {
                if entries.len() > MAX_NODES {
                    return Err(TestError::Capacity);
                }
                self.authority
                    .set_portal_inventory(
                        entries
                            .into_iter()
                            .map(|e| Endpoint {
                                key: e.key,
                                incarnation: e.incarnation,
                                source_channel: e.source_channel,
                                peer: e.peer,
                            })
                            .collect(),
                    )
                    .map_err(|_| TestError::Refused)?;
                Ok(TestReply::Ack)
            }
            TestEvent::View(binding) => self
                .authority
                .portal_view(&binding.into_binding()?)
                .map(|v| TestReply::View(TestView(v)))
                .map_err(|_| TestError::Refused),
            TestEvent::Operator {
                view,
                requested,
                now,
                valid_for,
            } => {
                if requested.len() > MAX_CHANNEL_VALUES {
                    return Err(TestError::Capacity);
                }
                let count = self
                    .bookkeeping
                    .would_dispatch
                    .checked_add(1)
                    .ok_or(TestError::Capacity)?;
                let attempt = self
                    .authority
                    .commit_portal(
                        &mut self.bookkeeping.portal,
                        &view.0,
                        &requested
                            .into_iter()
                            .map(|(channel, endpoint)| Requested { channel, endpoint })
                            .collect::<Vec<_>>(),
                        (now, valid_for),
                        env,
                    )
                    .map_err(|_| TestError::Refused)?;
                self.bookkeeping.would_dispatch = count;
                self.authority
                    .portal_dispatch_result(attempt.revision, DispatchOutcome::Unknown);
                Ok(TestReply::WouldDispatch {
                    revision: TestRevision(attempt.revision),
                    result: TestDispatch::Unknown,
                })
            }
            TestEvent::Report {
                holder,
                live,
                binding,
                snapshot,
                now,
                legacy,
                aggregate_fault,
            } => {
                let binding = binding.into_binding()?;
                let identity = holder.map_or(Identity::Unverified, |h| {
                    if self.authority.launches.active_process == Some(h.witness) {
                        Identity::Verified(h.lifetime)
                    } else {
                        Identity::Unverified
                    }
                });
                if !live && self.authority.guards.writer(identity, &binding).is_ok() {
                    self.authority.end_lifetime();
                }
                let identity = if live { identity } else { Identity::Unverified };
                let mut transition = self.authority.observation_with_backstop(
                    ProposalReport {
                        identity,
                        binding: &binding,
                        snapshot: &snapshot,
                    },
                    now,
                    if legacy {
                        LegacyPolicy::OutsidePortalFootprint
                    } else {
                        LegacyPolicy::Disabled
                    },
                    env,
                    |flows, config, env| {
                        if aggregate_fault {
                            vec![Issue::error("test-premise", "injected aggregate failure")]
                        } else {
                            validation_errors(flows, config, env)
                        }
                    },
                );
                let diagnostic = TestObservation {
                    candidate: transition.next.stored.clone(),
                    terminals: transition.outcomes.iter().map(TestTerminal::from).collect(),
                    context_refused: transition.context.is_some(),
                    legacy_refusals: transition
                        .legacy
                        .iter()
                        .filter(|u| u.refusal.is_some())
                        .count(),
                    waiting: transition.waiting.len(),
                };
                // Never reuse a hypothetical candidate as actual engine intent.
                // Keep terminal consumption, observation revisions and fences.
                // Restore actual provenance; retain only existing baselines that
                // survived invalidation, never newly hypothetical ownership.
                transition.next.stored = self.authority.stored.clone();
                transition.next.ledger.entries = self.authority.ledger.entries.clone();
                transition
                    .next
                    .baseline
                    .retain(|b| self.authority.baseline.contains(b));
                self.authority = transition.next;
                Ok(TestReply::Observation(diagnostic))
            }
            TestEvent::Dispatch { revision, result } => Ok(TestReply::Dispatch {
                revoked: self
                    .authority
                    .portal_dispatch_result(revision.0, result.into())
                    .map(TestRevision),
            }),
            TestEvent::EndLifetime => {
                self.authority.end_lifetime();
                Ok(TestReply::Ack)
            }
            TestEvent::Inspect => Ok(TestReply::State(TestState {
                managed_bindings: self.bookkeeping.managed.len(),
                unverified_reports: 0,
                actual_projection: self.authority.stored.clone(),
                generation: self.authority.ledger.generation,
                active: self.authority.guards.session.is_some(),
                tx_open: self.authority.ledger.tx_role.is_some(),
                pending: self.authority.portal.pending.len(),
                saved: self.authority.portal.saved.len(),
                fences: self.authority.portal.fences.len(),
                terminals: self
                    .authority
                    .portal
                    .terminal
                    .iter()
                    .map(TestTerminal::from)
                    .collect(),
                would_dispatch_count: self.bookkeeping.would_dispatch,
                observed_retained: self.authority.portal.observed.is_some(),
                view_revision: self.authority.portal.view_revision,
            })),
        }
    }
}

impl AcceptanceProbe {
    pub(crate) fn test_event(
        &mut self,
        event: TestEvent,
        config: &Config,
        devices: &[crate::model::DeviceModel],
        intent: &crate::persist::StateFile,
        env: &Environment,
    ) -> Result<TestReply, TestError> {
        if self.failure.is_some() {
            return Err(TestError::Stopped);
        }
        if legacy_source_unmodeled(config, intent) {
            self.stop(ProbeFailure::LegacySourceUnmodeled);
            return Err(TestError::Stopped);
        }
        if !budget::context(config, devices, intent) {
            self.stop(ProbeFailure::Capacity);
            return Err(TestError::Capacity);
        }
        if self.projection != intent.protocol_flows
            || self.authority.stored != intent.protocol_flows.as_deref().unwrap_or_default()
        {
            self.stop(ProbeFailure::ProjectionMismatch);
            return Err(TestError::Stopped);
        }
        let premise = self
            .premise
            .get_or_insert_with(|| PremiseWorld::new(self.authority.clone()));
        if premise.authority.stored != self.authority.stored {
            self.stop(ProbeFailure::ProjectionMismatch);
            return Err(TestError::Stopped);
        }
        premise.authority = match premise.authority.reload(config, devices) {
            Ok(next) => next,
            Err(_) => {
                self.stop(ProbeFailure::Transition);
                return Err(TestError::Stopped);
            }
        };
        let mut reply = match premise.event(event, config, env) {
            Err(TestError::Capacity) => {
                // Trusted selection must not fall back to an unmanaged shadow
                // when even its first binding cannot be retained. Stop rather
                // than truncate identities or evict previous management keys.
                self.stop(ProbeFailure::Capacity);
                return Err(TestError::Capacity);
            }
            result => result?,
        };
        if let TestReply::State(state) = &mut reply {
            state.unverified_reports = self.reports.len();
        }
        Ok(reply)
    }
}
