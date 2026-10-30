//! One pure decision over the authority snapshot. All state is private and
//! dormant outside tests; returned admission is not transport or audio evidence.
use super::guards::{
    Action, AuthorityView, Binding, Causality, ConsumedScope, Freshness, GrantId, Guards, Identity,
    Observation, Refusal, RxScope,
};
use super::*;

mod portal;
pub(crate) use portal::actor_probe::AcceptanceProbe;
#[cfg(feature = "routing-shadow-test-hooks")]
pub use portal::actor_probe::test_hooks;

#[derive(Debug, Clone, PartialEq, Eq)]
struct BaselineEntry {
    flow: FlowSpec,
    lifetime: Lifetime,
    generation: u64,
    epoch: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct Authority {
    config: Config,
    stored: Vec<FlowSpec>,
    ledger: Ledger,
    guards: Guards,
    launches: launch::LaunchState,
    baseline: Vec<BaselineEntry>,
    portal: portal::PortalState,
    tx_network: Option<tx_network::NetworkFacts>,
    tx_policy: tx_network::TxPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ContextRefusal {
    Guard(Refusal),
    ConfigInvalid(Vec<String>),
    StoredAmbiguous(Vec<String>),
    Capacity,
    AggregateConflict(Vec<String>),
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reason {
    NotOwned,
    Fenced,
    Protected,
    UnsupportedSplit,
    TxFenced,
    OutsideBinding,
    RemovalOnly,
    PortalRestricted,
    Scope(Refusal),
    Invalid(Vec<String>),
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Divergence {
    entry: FlowSpec,
    writer: Writer,
    channels: Vec<u16>,
    reason: Reason,
    kind: DivergenceKind,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DivergenceKind {
    StoredOmitted,
    ObservedNotAdmitted,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnitStatus {
    Admitted,
    Unchanged,
    Refused,
    Divergence,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct UnitResult {
    component: Component,
    status: UnitStatus,
    reasons: Vec<Reason>,
    divergences: Vec<Divergence>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReportStatus {
    Complete,
    Partial,
    Refused,
}
#[derive(Debug, Clone, PartialEq)]
struct Transition {
    next: Authority,
    status: ReportStatus,
    context: Option<ContextRefusal>,
    units: Vec<UnitResult>,
    actions: Vec<ActionResult>,
    recovery: Option<Recovery>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Recovery {
    initial_errors: Vec<String>,
    remaining_errors: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum ActionAdmission {
    Unit(UnitStatus),
    ContextRefused(ContextRefusal),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionEffect {
    Unverified,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct ActionResult {
    grant: GrantId,
    consumption: Option<Result<(), Refusal>>,
    scope_index: Option<usize>,
    admission: Option<ActionAdmission>,
    report_status: Option<ReportStatus>,
    // Admission alone never establishes the selected source or disconnection.
    effect: ActionEffect,
}
struct Report<'a> {
    identity: Identity,
    binding: Binding,
    freshness: Freshness,
    snapshot: &'a [FlowSpec],
    grants: &'a [GrantId],
}
fn errors(issues: Vec<Issue>) -> Vec<String> {
    issues.into_iter().map(|i| i.to_string()).collect()
}
fn epoch(guards: &Guards, device: &str) -> u64 {
    guards.epochs.get(device).copied().unwrap_or(0)
}
fn same_set(a: &[FlowSpec], b: &[FlowSpec]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut unmatched: Vec<_> = b.iter().collect();
    for flow in a {
        let Some(i) = unmatched.iter().position(|f| **f == *flow) else {
            return false;
        };
        unmatched.swap_remove(i);
    }
    true
}
/// Exact multiplicity, not membership. This prevents a duplicated observation
/// from entering a baseline through the removal-only path.
fn subset(a: &[FlowSpec], b: &[FlowSpec]) -> bool {
    let mut remaining: Vec<_> = b.iter().collect();
    for flow in a {
        let Some(index) = remaining.iter().position(|candidate| **candidate == *flow) else {
            return false;
        };
        remaining.swap_remove(index);
    }
    true
}
fn subsequence(a: &[FlowSpec], b: &[FlowSpec]) -> bool {
    let mut remaining = b.iter();
    a.iter()
        .all(|flow| remaining.by_ref().any(|candidate| candidate == flow))
}
fn channels(flow: &FlowSpec, config: &Config) -> BTreeSet<u16> {
    if flow.channels.is_empty() {
        (1..=config
            .device(&flow.device.0)
            .map_or(0, |d| match flow.direction {
                Direction::Rx => d.rx_channels,
                Direction::Tx => d.tx_channels,
            }))
            .collect()
    } else {
        flow.channels.iter().copied().collect()
    }
}
impl Authority {
    fn new(
        config: &Config,
        devices: &[crate::model::DeviceModel],
        stored: Vec<FlowSpec>,
        instance: launch::DaemonInstance,
    ) -> Self {
        let config = crate::flows::effective_validation_config(config, devices);
        let ledger = Ledger::restored(&stored, &config);
        Self {
            config,
            stored,
            ledger,
            guards: Guards::new(instance),
            launches: launch::LaunchState::default(),
            baseline: Vec::new(),
            portal: portal::PortalState::default(),
            tx_network: None,
            tx_policy: tx_network::TxPolicy::default(),
        }
    }
    fn reserve_launch(
        &mut self,
        issuer: &mut launch::LaunchIssuer,
        binding: &Binding,
        attempt: launch::LaunchAttempt,
        clock: (u64, u64),
        absent: launch::PriorRuntimeAbsent,
    ) -> Result<Lifetime, Refusal> {
        if self.guards.session.is_some() {
            return Err(Refusal::IncumbentAlive);
        }
        if issuer.instance() != self.guards.instance() {
            return Err(Refusal::LifetimeUnknown);
        }
        if self.config.device(&binding.device).is_none() {
            return Err(Refusal::BindingChanged);
        }
        let reservation = issuer.reserve(
            binding.clone(),
            attempt,
            (self.ledger.generation, epoch(&self.guards, &binding.device)),
            clock,
            absent,
        )?;
        let lifetime = reservation.lifetime;
        self.launches.replace(reservation);
        Ok(lifetime)
    }
    fn register_launch(
        &mut self,
        identity: Identity,
        binding: &Binding,
        witness: launch::PreExecWitness,
        now: u64,
    ) -> Result<(), Refusal> {
        if matches!(identity, Identity::Unverified) {
            return Err(Refusal::WriterUnverified);
        }
        if self.guards.session.is_some() {
            return Err(Refusal::IncumbentAlive);
        }
        let reservation = self.launches.association(
            identity,
            binding,
            witness,
            (self.ledger.generation, epoch(&self.guards, &binding.device)),
            now,
        )?;
        if let Err(error) = self.activate_registered(identity, binding) {
            self.launches.revoke(launch::LaunchEnd::Invalidated);
            return Err(error);
        }
        self.launches.revoke(launch::LaunchEnd::Consumed);
        self.launches.active_process = Some(witness);
        debug_assert_eq!(reservation.lifetime, witness.lifetime);
        Ok(())
    }
    fn abandon_launch(&mut self) {
        self.launches.revoke(launch::LaunchEnd::Abandoned);
    }
    fn activate_registered(
        &mut self,
        identity: Identity,
        binding: &Binding,
    ) -> Result<(), Refusal> {
        if self.config.device(&binding.device).is_none() {
            return Err(Refusal::BindingChanged);
        }
        self.guards.register(identity, binding)?;
        self.baseline.clear();
        let lifetime = self.guards.session.as_ref().unwrap().lifetime;
        let channels = self.config.device(&binding.device).unwrap().tx_channels;
        let network = self
            .tx_network
            .as_ref()
            .filter(|facts| facts.supports(binding));
        self.ledger.tx_role = (channels > 0
            && network.is_some()
            && !self.config.control.locked.contains(&ControlLock::Flows)
            && !self
                .config
                .control
                .locked
                .contains(&ControlLock::Subscriptions))
        .then(|| TxRole {
            lifetime,
            binding: binding.clone(),
            generation: self.ledger.generation,
            epoch: epoch(&self.guards, &binding.device),
            channels,
            network: network.unwrap().clone(),
            policy: self.tx_policy.clone(),
            device_multicast: self
                .config
                .device(&binding.device)
                .unwrap()
                .multicast
                .allow_tx_multicast,
        });
        Ok(())
    }
    fn set_tx_network(
        &mut self,
        network: Option<tx_network::NetworkFacts>,
        policy: tx_network::TxPolicy,
    ) -> Result<(), Refusal> {
        if self.tx_network == network && self.tx_policy == policy {
            return Ok(());
        }
        let mut next = self.clone();
        let mut affected: BTreeSet<_> = self
            .tx_network
            .iter()
            .chain(network.iter())
            .map(|n| n.binding.device.clone())
            .collect();
        if let Some(session) = &self.guards.session {
            affected.insert(session.binding.device.clone());
        }
        next.guards.fence_devices(&affected)?;
        next.baseline
            .retain(|b| !affected.contains(&b.flow.device.0));
        next.ledger.acquirable.retain(|d, _| !affected.contains(d));
        next.ledger.tx_role = None;
        next.launches.revoke(launch::LaunchEnd::Invalidated);
        next.portal
            .revoke_devices(&affected, portal::Revoke::NetworkChanged);
        next.tx_network = network;
        next.tx_policy = policy;
        *self = next;
        Ok(())
    }
    fn end_lifetime(&mut self) {
        self.portal.revoke_all(portal::Revoke::LifetimeEnded);
        self.guards.end_lifetime();
        self.launches.revoke(launch::LaunchEnd::LifetimeEnded);
        self.launches.active_process = None;
        self.ledger.tx_role = None;
        self.baseline.clear();
    }
    fn issue(
        &mut self,
        identity: Identity,
        binding: &Binding,
        cause: Causality,
        action: Action,
        scope: RxScope,
    ) -> Result<GrantId, Refusal> {
        if self.portal.restricts_scope(&scope) {
            return Err(Refusal::Protected);
        }
        self.guards.issue_future(
            identity,
            binding,
            cause,
            action,
            scope,
            AuthorityView {
                config: &self.config,
                stored: &self.stored,
                ledger: &self.ledger,
            },
        )
    }
    /// Required authority transition, including byte-identical accepted writes.
    /// The authority has already accepted/refused the write; no second validator.
    fn foreign_write(
        &mut self,
        next: &[FlowSpec],
        disposition: WriteDisposition,
    ) -> Result<(), ModelError> {
        let ledger = self.ledger.foreign_replacement(next, disposition)?;
        if disposition == WriteDisposition::Accepted {
            self.portal.revoke_all(portal::Revoke::ForeignWrite);
            self.launches.revoke(launch::LaunchEnd::ForeignWrite);
            self.stored = next.to_vec();
            self.ledger = ledger;
            self.baseline.clear();
            self.guards.retire(self.ledger.generation);
        }
        Ok(())
    }
    /// Locks can only change through this transition in the joined model. Both
    /// directions fence epochs/baselines. Activating Flows also clears intent,
    /// matching the existing engine; unlocking never restores it.
    fn set_locks(&self, locks: BTreeSet<ControlLock>) -> Result<Self, Refusal> {
        if self.config.control.locked == locks {
            return Ok(self.clone());
        }
        let mut next = self.clone();
        next.portal.revoke_all(portal::Revoke::LockChanged);
        next.launches.revoke(launch::LaunchEnd::LockChanged);
        let devices = self.config.devices.iter().map(|d| d.id.clone()).collect();
        next.guards.fence_devices(&devices)?;
        next.baseline.clear();
        next.ledger.acquirable.clear();
        next.ledger.tx_role = None;
        if locks.contains(&ControlLock::Flows)
            && !self.config.control.locked.contains(&ControlLock::Flows)
        {
            next.foreign_write(&[], WriteDisposition::Accepted)
                .map_err(|_| Refusal::Exhausted)?;
        }
        next.config.control.locked = locks;
        Ok(next)
    }
    fn reload(
        &self,
        config: &Config,
        devices: &[crate::model::DeviceModel],
    ) -> Result<Self, Refusal> {
        let effective = crate::flows::effective_validation_config(config, devices);
        let mut next = self.set_locks(effective.control.locked.clone())?;
        let global =
            self.config.flows != effective.flows || self.config.network != effective.network;
        let affected: BTreeSet<_> = self
            .config
            .devices
            .iter()
            .chain(&effective.devices)
            .filter(|d| global || self.config.device(&d.id) != effective.device(&d.id))
            .map(|d| d.id.clone())
            .collect();
        if next
            .launches
            .pending
            .as_ref()
            .is_some_and(|r| affected.contains(&r.binding.device))
        {
            next.launches.revoke(launch::LaunchEnd::Reload);
        }
        next.guards.fence_devices(&affected)?;
        next.portal
            .revoke_devices(&affected, portal::Revoke::Reload);
        next.baseline
            .retain(|b| !affected.contains(&b.flow.device.0));
        next.ledger.acquirable.retain(|d, _| !affected.contains(d));
        if next
            .ledger
            .tx_role
            .as_ref()
            .is_some_and(|role| affected.contains(&role.binding.device))
        {
            next.ledger.tx_role = None;
        }
        next.config = effective;
        Ok(next)
    }
    fn report(&self, report: Report<'_>, env: &Environment) -> Transition {
        self.report_with_backstop(report, env, validation_errors)
    }
    fn report_with_backstop(
        &self,
        report: Report<'_>,
        env: &Environment,
        backstop: impl FnOnce(&[FlowSpec], &Config, &Environment) -> Vec<Issue>,
    ) -> Transition {
        let mut actions: Vec<_> = report
            .grants
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(ActionResult::pending)
            .collect();
        let mut result = self.decide(report, env, backstop, &mut actions);
        for action in &mut actions {
            action.report_status = Some(result.status);
            action.admission = if let Some(error) = &result.context {
                Some(ActionAdmission::ContextRefused(error.clone()))
            } else {
                action.scope_index.and_then(|scope| {
                    result
                        .units
                        .iter()
                        .find(|unit| unit.component.scopes.contains(&scope))
                        .map(|unit| ActionAdmission::Unit(unit.status))
                })
            };
        }
        result.actions = actions;
        result
    }
    fn decide(
        &self,
        report: Report<'_>,
        env: &Environment,
        backstop: impl FnOnce(&[FlowSpec], &Config, &Environment) -> Vec<Issue>,
        actions: &mut Vec<ActionResult>,
    ) -> Transition {
        let mut working = self.clone();
        // Every authenticated report path records receipt, even when flow
        // admission or freshness later refuses it. Receipt supplies no native
        // production order, accepted foreign write or supersession permission.
        working.note_portal_observation(report.identity, &report.binding, report.snapshot);
        let observation = match working.guards.observe(
            report.identity,
            &report.binding,
            report.freshness,
            report.snapshot,
            working.ledger.generation,
        ) {
            Ok(obs) => obs,
            Err(error) => return Transition::refused(working, ContextRefusal::Guard(error)),
        };
        // Freshness and scope consumption are deliberately outside the flow,
        // provenance and admitted-baseline transaction.
        let mut scope_shapes = Vec::new();
        let mut scopes = Vec::new();
        if report.grants.len() > guards::MAX_GRANTS {
            return Transition::refused(working, ContextRefusal::Capacity);
        }
        let mut selected: BTreeSet<_> = report.grants.iter().copied().collect();
        selected.extend(working.guards.associated(&observation));
        let mut scope_error = None;
        for id in selected {
            let action_index = match actions.iter().position(|action| action.grant == id) {
                Some(index) => index,
                None => {
                    actions.push(ActionResult::pending(id));
                    actions.len() - 1
                }
            };
            let shape = match working.guards.scope_for(id, report.identity) {
                Ok(scope) => scope,
                Err(error) => {
                    scope_error.get_or_insert(error);
                    continue;
                }
            };
            actions[action_index].scope_index = Some(scope_shapes.len());
            scope_shapes.push(shape);
            let authority = AuthorityView {
                config: &working.config,
                stored: &working.stored,
                ledger: &working.ledger,
            };
            let consumed = working
                .guards
                .consume(id, report.identity, &observation, authority);
            actions[action_index].consumption =
                Some(consumed.as_ref().map(|_| ()).map_err(Clone::clone));
            scopes.push(consumed);
        }
        if let Some(error) = scope_error {
            return Transition::refused(working, ContextRefusal::Guard(error));
        }
        let baseline_records: Vec<_> = working
            .baseline
            .iter()
            .filter(|b| {
                b.lifetime == observation.lifetime
                    && b.generation == working.ledger.generation
                    && b.epoch == epoch(&working.guards, &b.flow.device.0)
            })
            .cloned()
            .collect();
        let baseline: Vec<_> = baseline_records.iter().map(|b| b.flow.clone()).collect();
        let configured = match configured_specs(&working.config) {
            Ok(flows) => flows,
            Err(error) => {
                return Transition::refused(
                    working,
                    ContextRefusal::ConfigInvalid(vec![format!("{error:?}")]),
                );
            }
        };
        if check_capacity([
            baseline.as_slice(),
            observation.snapshot.as_slice(),
            working.stored.as_slice(),
            configured.as_slice(),
        ])
        .is_err()
        {
            return Transition::refused(working, ContextRefusal::Capacity);
        }
        let base_errors = validation_errors(&[], &working.config, env);
        if !base_errors.is_empty() {
            return Transition::refused(
                working,
                ContextRefusal::ConfigInvalid(errors(base_errors)),
            );
        }
        let stored_errors = validation_errors(&working.stored, &working.config, env);
        let removal_only = !stored_errors.is_empty();
        if removal_only
            && working
                .stored
                .iter()
                .map(|flow| &flow.id)
                .collect::<BTreeSet<_>>()
                .len()
                != working.stored.len()
        {
            // The current provenance map has one exact record per ID. Recovery
            // cannot infer multiplicity ownership for an ambiguous stored ID.
            return Transition::refused(
                working,
                ContextRefusal::StoredAmbiguous(errors(stored_errors)),
            );
        }
        if working.config.control.locked.contains(&ControlLock::Flows)
            || (working
                .config
                .control
                .locked
                .contains(&ControlLock::Subscriptions)
                && !same_set(&working.stored, &observation.snapshot))
        {
            return Transition::refused(working, ContextRefusal::Guard(Refusal::Locked));
        }
        let input = Inputs {
            baseline: &baseline,
            stored: &working.stored,
            observed: &observation.snapshot,
        };
        let groups = match scoped_components(input, &working.config, &scope_shapes) {
            Ok(groups) => groups,
            Err(ModelError::Capacity) => {
                return Transition::refused(working, ContextRefusal::Capacity);
            }
            Err(error) => {
                return Transition::refused(
                    working,
                    ContextRefusal::ConfigInvalid(vec![format!("{error:?}")]),
                );
            }
        };
        let mut units = Vec::new();
        for component in groups {
            if component.stored.is_empty()
                && component.observed.is_empty()
                && component.baseline.is_empty()
                && component.scopes.is_empty()
            {
                continue;
            }
            units.push(evaluate(
                &working,
                &observation,
                &baseline,
                component,
                &scopes,
                env,
                removal_only,
            ));
        }
        let successful =
            |unit: &UnitResult| matches!(unit.status, UnitStatus::Admitted | UnitStatus::Unchanged);
        let mut next_stored = working.stored.clone();
        let mut remove_stored = BTreeSet::new();
        let mut remove_baseline = BTreeSet::new();
        let mut add_observed = BTreeSet::new();
        for unit in units.iter().filter(|u| successful(u)) {
            remove_stored.extend(
                unit.component
                    .stored
                    .iter()
                    .filter(|i| {
                        !unit
                            .component
                            .observed
                            .iter()
                            .any(|o| observation.snapshot[*o] == working.stored[**i])
                    })
                    .copied(),
            );
            remove_baseline.extend(unit.component.baseline.iter().copied());
            add_observed.extend(unit.component.observed.iter().copied());
        }
        next_stored = next_stored
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !remove_stored.contains(i))
            .map(|(_, flow)| flow)
            .collect();
        for i in &add_observed {
            let flow = &observation.snapshot[*i];
            if !next_stored.contains(flow) {
                next_stored.push(flow.clone());
            }
        }
        let aggregate_errors = backstop(&next_stored, &working.config, env);
        // Invalid starting sets use structural subset recovery: no additions,
        // rebuilds, multiplicity growth or reorder. No diagnostic-string/error-
        // count comparison supplies permission. The current validator contains
        // only unary and pairwise flow obligations under unchanged context.
        if (!removal_only && !aggregate_errors.is_empty())
            || (removal_only && !subsequence(&next_stored, &working.stored))
        {
            return Transition::refused(
                working,
                ContextRefusal::AggregateConflict(errors(aggregate_errors)),
            );
        }
        let mut next_ledger = working.ledger.clone();
        next_ledger
            .entries
            .retain(|_, entry| next_stored.contains(&entry.flow));
        for flow in next_stored
            .iter()
            .filter(|flow| !working.stored.contains(flow))
        {
            next_ledger.entries.insert(
                flow.id.clone(),
                EntryStamp {
                    flow: flow.clone(),
                    writer: Writer::Runtime(observation.lifetime),
                    generation: next_ledger.generation,
                    epoch: epoch(&working.guards, &flow.device.0),
                },
            );
        }
        let mut next_baseline: Vec<_> = baseline_records
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !remove_baseline.contains(i))
            .map(|(_, b)| b)
            .collect();
        next_baseline.extend(
            add_observed
                .into_iter()
                .filter(|i| !working.portal.restricted(&observation.snapshot[*i]))
                .map(|i| BaselineEntry {
                    flow: observation.snapshot[i].clone(),
                    lifetime: observation.lifetime,
                    generation: working.ledger.generation,
                    epoch: epoch(&working.guards, &observation.snapshot[i].device.0),
                }),
        );
        // One pure returned commit: intent, rights and admitted baselines move
        // together. No part of a refused unit advances its admitted baseline.
        working.stored = next_stored;
        working.ledger = next_ledger;
        working.baseline = next_baseline;
        let recovered_valid = aggregate_errors.is_empty();
        let status = if !recovered_valid {
            if working.stored != self.stored {
                ReportStatus::Partial
            } else {
                ReportStatus::Refused
            }
        } else if units.iter().all(successful) {
            ReportStatus::Complete
        } else if units.iter().any(successful) {
            ReportStatus::Partial
        } else {
            ReportStatus::Refused
        };
        if status == ReportStatus::Refused {
            // Whole-report refusal cannot advance admitted authority, even
            // when independent unchanged units would refresh their baseline.
            // Observation freshness and burned scopes remain in the guards.
            working.stored = self.stored.clone();
            working.ledger = self.ledger.clone();
            working.baseline = self.baseline.clone();
        }
        Transition {
            next: working,
            status,
            context: None,
            units,
            actions: Vec::new(),
            recovery: removal_only.then(|| Recovery {
                initial_errors: errors(stored_errors),
                remaining_errors: errors(aggregate_errors),
            }),
        }
    }
}
impl Transition {
    fn refused(next: Authority, error: ContextRefusal) -> Self {
        Self {
            next,
            status: ReportStatus::Refused,
            context: Some(error),
            units: Vec::new(),
            actions: Vec::new(),
            recovery: None,
        }
    }
}
impl ActionResult {
    fn pending(grant: GrantId) -> Self {
        Self {
            grant,
            consumption: None,
            scope_index: None,
            admission: None,
            report_status: None,
            effect: ActionEffect::Unverified,
        }
    }
}
fn evaluate(
    authority: &Authority,
    observation: &Observation,
    baseline: &[FlowSpec],
    component: Component,
    scopes: &[Result<ConsumedScope, Refusal>],
    env: &Environment,
    removal_only: bool,
) -> UnitResult {
    let mut reasons = Vec::new();
    let mut divergences = Vec::new();
    let observed: Vec<_> = component
        .observed
        .iter()
        .map(|i| observation.snapshot[*i].clone())
        .collect();
    let stored: Vec<_> = component
        .stored
        .iter()
        .map(|i| authority.stored[*i].clone())
        .collect();
    let removed: Vec<_> = stored.iter().filter(|f| !observed.contains(f)).collect();
    let added: Vec<_> = observed.iter().filter(|f| !stored.contains(f)).collect();
    if removal_only && !subset(&observed, &stored) {
        reasons.push(Reason::RemovalOnly);
    }
    let consumed: Vec<_> = component
        .scopes
        .iter()
        .filter_map(|i| match &scopes[*i] {
            Ok(scope) => Some(scope),
            Err(error) => {
                reasons.push(Reason::Scope(error.clone()));
                None
            }
        })
        .collect();
    for scope in &consumed {
        if scope.lifetime != observation.lifetime
            || scope.binding != observation.binding
            || scope.generation != authority.ledger.generation
            || scope.epoch != epoch(&authority.guards, &scope.scope.device)
            || scope.report_order != observation.order
        {
            reasons.push(Reason::Scope(Refusal::StaleScope));
        }
        if let Err(error) = scope
            .scope
            .check_action(scope.action, &observation.snapshot)
        {
            reasons.push(Reason::Scope(error));
        }
    }
    let owned = |flow: &FlowSpec| {
        !authority.portal.restricted(flow)
            && authority.ledger.owns_exact(
                flow,
                observation.lifetime,
                epoch(&authority.guards, &flow.device.0),
            )
            && baseline.contains(flow)
    };
    for flow in &removed {
        let protected = authority.ledger.protected(flow);
        let scoped = consumed.iter().any(|s| s.scope.contains(flow));
        let restricted = authority.portal.restricted(flow);
        if protected || restricted || (!owned(flow) && !scoped) {
            let reason = if restricted {
                Reason::PortalRestricted
            } else if protected {
                Reason::Protected
            } else if consumed.iter().any(|s| s.scope.touches(flow)) {
                Reason::UnsupportedSplit
            } else {
                Reason::NotOwned
            };
            divergences.push(Divergence {
                entry: (*flow).clone(),
                writer: authority
                    .ledger
                    .entries
                    .get(&flow.id)
                    .map_or(Writer::Unknown, |s| s.writer),
                channels: channels(flow, &authority.config).into_iter().collect(),
                reason,
                kind: DivergenceKind::StoredOmitted,
            });
        }
    }
    for flow in &added {
        if authority.portal.restricted(flow) {
            reasons.push(Reason::PortalRestricted);
        }
        if !observation.binding.contains(flow) {
            reasons.push(Reason::OutsideBinding);
        }
        if flow.direction == Direction::Tx {
            if !authority.ledger.tx_allows(
                flow,
                observation.lifetime,
                epoch(&authority.guards, &flow.device.0),
            ) {
                reasons.push(Reason::TxFenced);
            }
            continue;
        }
        let scoped = consumed
            .iter()
            .any(|s| s.action == Action::Retarget && s.scope.contains(flow));
        // Owned maintenance may rebuild only the exact whole receiver carrier;
        // graph connectivity, ID and port never expand its resource permission.
        let maintenance = removed.iter().any(|old| {
            owned(old)
                && old.direction == Direction::Rx
                && old.device == flow.device
                && channels(old, &authority.config) == channels(flow, &authority.config)
        });
        if !scoped
            && !maintenance
            && authority
                .ledger
                .acquisition_is_fenced(flow, epoch(&authority.guards, &flow.device.0))
        {
            reasons.push(Reason::Fenced);
        }
    }
    if !removal_only
        && reasons.is_empty()
        && divergences.is_empty()
        && !same_set(&stored, &observed)
    {
        let mut candidate: Vec<_> = authority
            .stored
            .iter()
            .enumerate()
            .filter(|(i, _)| !component.stored.contains(i))
            .map(|(_, f)| f.clone())
            .collect();
        // Validate the complete replacement, including observation multiplicity.
        candidate.extend(observed.iter().cloned());
        let invalid = validation_errors(&candidate, &authority.config, env);
        if !invalid.is_empty() {
            reasons.push(Reason::Invalid(errors(invalid)));
        }
    }
    let status = if !reasons.is_empty() {
        UnitStatus::Refused
    } else if !divergences.is_empty() {
        UnitStatus::Divergence
    } else if removed.is_empty() && added.is_empty() {
        UnitStatus::Unchanged
    } else {
        UnitStatus::Admitted
    };
    if matches!(status, UnitStatus::Refused | UnitStatus::Divergence) {
        // A refused unit admits none of its additions, including additions
        // otherwise permitted but atomically joined to a failing receiver.
        for flow in added {
            divergences.push(Divergence {
                entry: flow.clone(),
                writer: Writer::Runtime(observation.lifetime),
                channels: channels(flow, &authority.config).into_iter().collect(),
                reason: reasons.first().cloned().unwrap_or(Reason::NotOwned),
                kind: DivergenceKind::ObservedNotAdmitted,
            });
        }
    }
    UnitResult {
        component,
        status,
        reasons,
        divergences,
    }
}
