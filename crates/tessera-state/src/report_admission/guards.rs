//! Logical proof premises for the private model; no production attestation.
use super::*;

pub(super) const MAX_GRANTS: usize = 256;

/// Authority-supplied effective binding; not a runtime-selected routing label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Binding {
    pub(super) device: String,
    pub(super) interface: String,
    pub(super) address: Ipv4Addr,
}
impl Binding {
    pub(super) fn valid(&self) -> bool {
        !self.device.is_empty()
            && !self.interface.is_empty()
            && !self.address.is_unspecified()
            && !self.address.is_multicast()
            && !self.address.is_broadcast()
    }
    pub(super) fn contains(&self, flow: &FlowSpec) -> bool {
        flow.device.0 == self.device
            && flow.interface.as_deref() == Some(self.interface.as_str())
            && address(flow.local_address.as_deref()) == Some(self.address)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Identity {
    #[default]
    Unverified,
    Verified(Lifetime),
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Freshness {
    #[default]
    Unordered,
    Ordered(u64),
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) enum Causality {
    #[default]
    Unproven,
    Proven {
        lifetime: Lifetime,
        binding: Binding,
        action: Action,
        scope: RxScope,
        operation: u64,
        report_order: u64,
        generation: u64,
        epoch: u64,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    Retarget,
    Disconnect,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Refusal {
    WriterUnverified,
    LifetimeUnknown,
    IncumbentAlive,
    BindingChanged,
    FreshnessUnproven,
    StaleOrTied,
    CausalityUnproven,
    Locked,
    InvalidScope,
    UnsupportedSplit,
    Protected,
    GrantMissing,
    StaleScope,
    ObservationMismatch,
    ActionMismatch,
    OperationReused,
    Capacity,
    Exhausted,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RxScope {
    pub(super) device: String,
    pub(super) channels: BTreeSet<u16>,
    rx_channels: u16,
}
impl RxScope {
    pub(super) fn review(device: &str, channels: &[u16], config: &Config) -> Result<Self, Refusal> {
        let geometry = config.device(device).ok_or(Refusal::InvalidScope)?;
        let selected: BTreeSet<_> = channels.iter().copied().collect();
        if selected.is_empty()
            || selected.len() != channels.len()
            || selected
                .iter()
                .any(|c| *c == 0 || *c > geometry.rx_channels)
        {
            return Err(Refusal::InvalidScope);
        }
        Ok(Self {
            device: device.into(),
            channels: selected,
            rx_channels: geometry.rx_channels,
        })
    }
    pub(super) fn flow_channels(&self, flow: &FlowSpec) -> BTreeSet<u16> {
        if flow.channels.is_empty() {
            (1..=self.rx_channels).collect()
        } else {
            flow.channels.iter().copied().collect()
        }
    }
    pub(super) fn touches(&self, flow: &FlowSpec) -> bool {
        flow.direction == Direction::Rx
            && flow.device.0 == self.device
            && (flow.channels.is_empty() || flow.channels.iter().any(|c| self.channels.contains(c)))
    }
    pub(super) fn contains(&self, flow: &FlowSpec) -> bool {
        self.touches(flow)
            && if flow.channels.is_empty() {
                self.channels.len() == usize::from(self.rx_channels)
            } else {
                flow.channels.iter().all(|c| self.channels.contains(c))
            }
    }
    fn check_carriers(&self, stored: &[FlowSpec]) -> Result<(), Refusal> {
        if stored.iter().any(|f| self.touches(f) && !self.contains(f)) {
            return Err(Refusal::UnsupportedSplit);
        }
        Ok(())
    }
    pub(super) fn check_action(&self, action: Action, report: &[FlowSpec]) -> Result<(), Refusal> {
        self.check_carriers(report)?;
        let reported: BTreeSet<_> = report
            .iter()
            .filter(|f| self.touches(f))
            .flat_map(|f| self.flow_channels(f))
            .collect();
        match action {
            Action::Disconnect if reported.is_empty() => Ok(()),
            Action::Retarget if reported == self.channels => Ok(()),
            _ => Err(Refusal::ActionMismatch),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Observation {
    pub(super) lifetime: Lifetime,
    pub(super) binding: Binding,
    pub(super) order: u64,
    pub(super) generation: u64,
    pub(super) epochs: BTreeMap<String, u64>,
    pub(super) snapshot: Vec<FlowSpec>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Session {
    pub(super) lifetime: Lifetime,
    pub(super) binding: Binding,
    pub(super) watermark: Option<u64>,
    operation_watermark: u64,
    pub(super) latest: Option<Observation>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct GrantId(u64);
#[derive(Debug, Clone, PartialEq, Eq)]
struct Grant {
    lifetime: Lifetime,
    binding: Binding,
    generation: u64,
    epoch: u64,
    report_order: u64,
    action: Action,
    scope: RxScope,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ConsumedScope {
    pub(super) scope: RxScope,
    pub(super) action: Action,
    pub(super) lifetime: Lifetime,
    pub(super) binding: Binding,
    pub(super) generation: u64,
    pub(super) epoch: u64,
    pub(super) report_order: u64,
}
#[derive(Clone, Copy)]
pub(super) struct AuthorityView<'a> {
    pub(super) config: &'a Config,
    pub(super) stored: &'a [FlowSpec],
    pub(super) ledger: &'a Ledger,
}
impl AuthorityView<'_> {
    fn protected(&self, scope: &RxScope) -> bool {
        self.stored
            .iter()
            .any(|f| self.ledger.protected(f) && scope.touches(f))
            || self
                .config
                .flows
                .iter()
                .any(|f| crate::flows::to_spec(f).map_or(true, |f| scope.touches(&f)))
    }
    fn locked(&self) -> bool {
        self.config.control.locked.contains(&ControlLock::Flows)
            || self
                .config
                .control
                .locked
                .contains(&ControlLock::Subscriptions)
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Guards {
    pub(super) session: Option<Session>,
    highest_lifetime: u64,
    instance: launch::DaemonInstance,
    pub(super) epochs: BTreeMap<String, u64>,
    next_grant: u64,
    grants: BTreeMap<GrantId, Grant>,
}
impl Default for Guards {
    fn default() -> Self {
        Self::new(TEST_INSTANCE)
    }
}
impl Guards {
    pub(super) fn restrict_scope(&mut self, scope: &RxScope) {
        self.grants.retain(|_, grant| {
            grant.scope.device != scope.device || grant.scope.channels.is_disjoint(&scope.channels)
        });
    }
    pub(super) fn new(instance: launch::DaemonInstance) -> Self {
        Self {
            session: None,
            highest_lifetime: 0,
            instance,
            epochs: BTreeMap::new(),
            next_grant: 0,
            grants: BTreeMap::new(),
        }
    }
    pub(super) fn instance(&self) -> launch::DaemonInstance {
        self.instance
    }
    pub(super) fn register(
        &mut self,
        identity: Identity,
        binding: &Binding,
    ) -> Result<(), Refusal> {
        let Identity::Verified(lifetime) = identity else {
            return Err(Refusal::WriterUnverified);
        };
        if self.session.is_some() {
            return Err(Refusal::IncumbentAlive);
        }
        if !binding.valid() {
            return Err(Refusal::BindingChanged);
        }
        if lifetime.instance != self.instance || lifetime.serial <= self.highest_lifetime {
            return Err(Refusal::LifetimeUnknown);
        }
        self.session = Some(Session {
            lifetime,
            binding: binding.clone(),
            watermark: None,
            operation_watermark: 0,
            latest: None,
        });
        self.highest_lifetime = lifetime.serial;
        Ok(())
    }
    pub(super) fn writer(
        &self,
        identity: Identity,
        binding: &Binding,
    ) -> Result<&Session, Refusal> {
        let Identity::Verified(lifetime) = identity else {
            return Err(Refusal::WriterUnverified);
        };
        let session = self.session.as_ref().ok_or(Refusal::LifetimeUnknown)?;
        if lifetime != session.lifetime {
            return Err(Refusal::LifetimeUnknown);
        }
        if binding != &session.binding {
            return Err(Refusal::BindingChanged);
        }
        Ok(session)
    }
    pub(super) fn end_lifetime(&mut self) {
        self.session = None;
        self.grants.clear();
    }
    pub(super) fn retire(&mut self, generation: u64) {
        self.grants.retain(|_, grant| {
            self.session.as_ref().is_some_and(|s| {
                s.lifetime == grant.lifetime
                    && s.binding == grant.binding
                    && s.watermark.is_none_or(|order| {
                        grant.report_order > order
                            || (grant.report_order == order && s.latest.is_some())
                    })
            }) && grant.generation == generation
                && grant.epoch == self.epochs.get(&grant.scope.device).copied().unwrap_or(0)
        });
    }
    /// Freshness advances before payload admission. An oversized newer report
    /// retains no snapshot and still invalidates all older reports and grants.
    pub(super) fn observe(
        &mut self,
        identity: Identity,
        binding: &Binding,
        freshness: Freshness,
        snapshot: &[FlowSpec],
        generation: u64,
    ) -> Result<Observation, Refusal> {
        let session = self.writer(identity, binding)?;
        let Freshness::Ordered(order) = freshness else {
            return Err(Refusal::FreshnessUnproven);
        };
        if session.watermark.is_some_and(|previous| order <= previous) {
            return Err(Refusal::StaleOrTied);
        }
        let session = self.session.as_mut().expect("verified session");
        session.watermark = Some(order);
        session.latest = None;
        let capacity = check_capacity([snapshot]);
        if capacity.is_err() {
            self.retire(generation);
            return Err(Refusal::Capacity);
        }
        let session = self.session.as_mut().expect("verified session");
        let observation = Observation {
            lifetime: session.lifetime,
            binding: session.binding.clone(),
            order,
            generation,
            epochs: self.epochs.clone(),
            snapshot: snapshot.to_vec(),
        };
        session.latest = Some(observation.clone());
        self.retire(generation);
        Ok(observation)
    }
    /// Called by the joined authority's mandatory lock/reload transitions.
    pub(super) fn fence_devices(&mut self, devices: &BTreeSet<String>) -> Result<(), Refusal> {
        let increments: Result<Vec<_>, _> = devices
            .iter()
            .map(|device| {
                let epoch = self.epochs.get(device).copied().unwrap_or(0);
                Ok((
                    device.clone(),
                    epoch.checked_add(1).ok_or(Refusal::Exhausted)?,
                ))
            })
            .collect();
        self.epochs.extend(increments?);
        self.grants
            .retain(|_, g| !devices.contains(&g.scope.device));
        Ok(())
    }
    pub(super) fn issue(
        &mut self,
        identity: Identity,
        binding: &Binding,
        causality: Causality,
        action: Action,
        scope: RxScope,
        authority: AuthorityView<'_>,
    ) -> Result<GrantId, Refusal> {
        self.writer(identity, binding)?;
        self.retire(authority.ledger.generation);
        if self.grants.len() >= MAX_GRANTS
            || self
                .grants
                .values()
                .map(|g| g.scope.channels.len())
                .sum::<usize>()
                + scope.channels.len()
                > MAX_CHANNEL_VALUES
        {
            return Err(Refusal::Capacity);
        }
        let next = self.next_grant.checked_add(1).ok_or(Refusal::Exhausted)?;
        let Causality::Proven {
            lifetime,
            binding: action_binding,
            action: proven_action,
            scope: proven_scope,
            operation,
            report_order,
            generation,
            epoch,
        } = causality
        else {
            return Err(Refusal::CausalityUnproven);
        };
        let session = self.writer(Identity::Verified(lifetime), &action_binding)?;
        if operation <= session.operation_watermark {
            return Err(Refusal::OperationReused);
        }
        // Seeing a valid, new operation burns it even when a later check fails.
        self.session
            .as_mut()
            .expect("verified session")
            .operation_watermark = operation;
        if proven_action != action || proven_scope != scope {
            return Err(Refusal::ActionMismatch);
        }
        if scope.device != binding.device {
            return Err(Refusal::BindingChanged);
        }
        if authority.locked() {
            return Err(Refusal::Locked);
        }
        let session = self.writer(identity, binding)?;
        if generation != authority.ledger.generation
            || epoch != self.epochs.get(&scope.device).copied().unwrap_or(0)
            || authority
                .config
                .device(&scope.device)
                .map(|d| d.rx_channels)
                != Some(scope.rx_channels)
        {
            return Err(Refusal::StaleScope);
        }
        if session.watermark.is_some_and(|order| {
            report_order < order || (report_order == order && session.latest.is_none())
        }) {
            return Err(Refusal::ObservationMismatch);
        }
        scope.check_carriers(authority.stored)?;
        if authority.protected(&scope) {
            return Err(Refusal::Protected);
        }
        let grant = Grant {
            lifetime: session.lifetime,
            binding: binding.clone(),
            generation,
            epoch,
            report_order,
            action,
            scope,
        };
        let id = GrantId(next);
        self.next_grant = next;
        self.grants.insert(id, grant);
        Ok(id)
    }
    pub(super) fn scope_for(&self, id: GrantId, identity: Identity) -> Result<RxScope, Refusal> {
        let grant = self.grants.get(&id).ok_or(Refusal::GrantMissing)?;
        self.writer(identity, &grant.binding)?;
        Ok(grant.scope.clone())
    }
    /// The joined report step observes first, so an already-observed order
    /// cannot be associated with its next report. Burn the operation and retire
    /// any issued handle rather than retaining an unusable grant.
    pub(super) fn issue_future(
        &mut self,
        identity: Identity,
        binding: &Binding,
        causality: Causality,
        action: Action,
        scope: RxScope,
        authority: AuthorityView<'_>,
    ) -> Result<GrantId, Refusal> {
        let current = self.writer(identity, binding)?.watermark;
        let not_future = matches!(&causality, Causality::Proven { report_order, .. }
            if current.is_some_and(|order| *report_order <= order));
        let id = self.issue(identity, binding, causality, action, scope, authority)?;
        if not_future {
            self.grants.remove(&id);
            return Err(Refusal::ObservationMismatch);
        }
        Ok(id)
    }
    /// The caller cannot hide an action associated with this observation.
    pub(super) fn associated(&self, observation: &Observation) -> Vec<GrantId> {
        self.grants
            .iter()
            .filter(|(_, grant)| {
                grant.lifetime == observation.lifetime
                    && grant.binding == observation.binding
                    && grant.report_order == observation.order
            })
            .map(|(id, _)| *id)
            .collect()
    }
    /// Wrong-identity requests cannot burn another holder's grant. Once the
    /// holder presents it, success and failure both consume the permission.
    pub(super) fn consume(
        &mut self,
        id: GrantId,
        identity: Identity,
        observation: &Observation,
        authority: AuthorityView<'_>,
    ) -> Result<ConsumedScope, Refusal> {
        let grant = self.grants.get(&id).ok_or(Refusal::GrantMissing)?;
        self.writer(identity, &grant.binding)?;
        let grant = self.grants.remove(&id).expect("checked grant");
        if authority.locked() {
            return Err(Refusal::Locked);
        }
        let session = self.writer(identity, &grant.binding)?;
        let epoch = self.epochs.get(&grant.scope.device).copied().unwrap_or(0);
        if session.lifetime != grant.lifetime
            || authority.ledger.generation != grant.generation
            || authority.ledger.generation != observation.generation
            || epoch != grant.epoch
            || observation
                .epochs
                .get(&grant.scope.device)
                .copied()
                .unwrap_or(0)
                != epoch
            || authority
                .config
                .device(&grant.scope.device)
                .map(|d| d.rx_channels)
                != Some(grant.scope.rx_channels)
        {
            return Err(Refusal::StaleScope);
        }
        if session.latest.as_ref() != Some(observation)
            || observation.order != grant.report_order
            || observation.lifetime != grant.lifetime
            || observation.binding != grant.binding
        {
            return Err(Refusal::ObservationMismatch);
        }
        grant.scope.check_carriers(authority.stored)?;
        if authority.protected(&grant.scope) {
            return Err(Refusal::Protected);
        }
        grant
            .scope
            .check_action(grant.action, &observation.snapshot)?;
        Ok(ConsumedScope {
            scope: grant.scope,
            action: grant.action,
            lifetime: grant.lifetime,
            binding: grant.binding,
            generation: grant.generation,
            epoch: grant.epoch,
            report_order: grant.report_order,
        })
    }
}
