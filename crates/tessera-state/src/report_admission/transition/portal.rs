//! Confined daemon-owned portal proposals. No callback ordering or native
//! causality is asserted. Process association and current inventory are trusted
//! model premises. This path cannot be reached through report grants.
use super::*;

pub(super) mod actor_probe;
mod observation;

const MAX_ATTEMPTS: usize = 256;
const MAX_TERMINALS: usize = 512;
const MAX_DEADLINE: u64 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Revision {
    instance: launch::DaemonInstance,
    serial: u64,
}
/// Retained outside a reconstructed Authority, just like the launch issuer.
#[derive(Debug)]
struct PortalIssuer {
    instance: launch::DaemonInstance,
    serial: u64,
}
impl PortalIssuer {
    fn new(instance: launch::DaemonInstance) -> Self {
        Self {
            instance,
            serial: 0,
        }
    }
    fn issue(&mut self) -> Result<Revision, PortalError> {
        self.serial = self.serial.checked_add(1).ok_or(PortalError::Capacity)?;
        Ok(Revision {
            instance: self.instance,
            serial: self.serial,
        })
    }
}
/// Exact inventory endpoint and incarnation, not proof of the native source.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint {
    key: u128,
    incarnation: u128,
    source_channel: u16,
    peer: Ipv4Addr,
}
impl Endpoint {
    fn usable(&self) -> bool {
        self.key != 0
            && self.incarnation != 0
            && self.source_channel != 0
            && !self.peer.is_unspecified()
            && !self.peer.is_multicast()
            && !self.peer.is_broadcast()
            && !self.peer.is_loopback()
            && !self.peer.is_link_local()
            && self.peer.octets()[0] != 0
            && self.peer.octets()[0] < 240
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum Required {
    Vacant,
    Kept(Ipv4Addr),
    Selected(Endpoint),
}
impl Required {
    fn peer(&self) -> Option<Ipv4Addr> {
        match self {
            Self::Vacant => None,
            Self::Kept(peer) => Some(*peer),
            Self::Selected(endpoint) => Some(endpoint.peer),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Requested {
    channel: u16,
    endpoint: Option<u128>,
}
/// What the actor showed the operator: admitted and last proposed carrier
/// content, saved targets and concurrency fences. This is not native readback.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpectedView {
    instance: launch::DaemonInstance,
    lifetime: Lifetime,
    binding: Binding,
    generation: u64,
    epoch: u64,
    revision: u64,
    stored: Vec<FlowSpec>,
    observed: Option<(Binding, Vec<FlowSpec>)>,
    saved: BTreeMap<(String, u16), Required>,
    fences: BTreeMap<(String, u16), Revision>,
    inventory: Vec<Endpoint>,
}
impl ExpectedView {
    fn same_state(&self, other: &Self) -> bool {
        let mut a = self.clone();
        let mut b = other.clone();
        // Only the requested inventory endpoints gate the command. Discovery
        // churn on an unrelated endpoint must not invalidate the operator view.
        a.inventory.clear();
        b.inventory.clear();
        a == b
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Attempt {
    revision: Revision,
    lifetime: Lifetime,
    binding: Binding,
    scope: RxScope,
    required: BTreeMap<u16, Required>,
    // Dispatch and saved intent use only the requested channels, never kept C\R.
    dispatch: BTreeMap<u16, Required>,
    generation: u64,
    epoch: u64,
    expires: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchOutcome {
    Done,
    Failed,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct DispatchLink {
    origin: Revision,
    successor: Revision,
    targets: BTreeMap<u16, Required>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Revoke {
    Superseded(Revision),
    Coalesced(Revision),
    ForeignWrite,
    LockChanged,
    Reload,
    InventoryChanged,
    NetworkChanged,
    LifetimeEnded,
    Cancelled,
    DispatchFailed(Revision),
    Context(PortalError),
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum AttemptOutcome {
    Admitted,
    Unchanged,
    Refused(Vec<String>),
    RolledBack(Vec<String>),
    Expired,
    Revoked(Revoke),
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum StoredRecognition {
    StoredAlreadyPresent,
    StoredVacant,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct CarrierRecognition {
    flow: FlowSpec,
    writer: Writer,
    revision: Revision,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Terminal {
    revision: Revision,
    outcome: AttemptOutcome,
    recognition: Option<StoredRecognition>,
    carriers: Vec<CarrierRecognition>,
    effect: ActionEffect,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PortalError {
    Guard(Refusal),
    Capacity,
    Inventory,
    InvalidRequest,
    Protected,
    Binding,
    AnySource,
    StaleView,
    ConfigInvalid(Vec<String>),
    StoredAmbiguous,
    StoredOutsideInvalid(Vec<String>),
    Invalidated,
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum AssociationError {
    Holder,
    Binding,
    Carrier,
    Peer,
    Coverage,
    Capacity,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct PortalState {
    inventory: Vec<Endpoint>,
    pending: Vec<Attempt>,
    terminal: Vec<Terminal>,
    saved: BTreeMap<(String, u16), Required>,
    // Continuing revision fences survive failure, expiry, cancellation, foreign
    // writes and lifetime end. Observation cannot reopen them.
    fences: BTreeMap<(String, u16), Revision>,
    view_revision: u64,
    view_exhausted: bool,
    observed: Option<(Binding, Vec<FlowSpec>)>,
    // Outstanding dispatches follow only targets inherited by pending attempts.
    // Terminal draining never erases a dependency. Done or terminal successors
    // retire links; unknown results retain them until the finite deadline.
    dispatches: Vec<DispatchLink>,
}
impl PortalState {
    pub(super) fn restricted(&self, flow: &FlowSpec) -> bool {
        flow.direction == Direction::Rx
            && self.fences.keys().any(|(device, channel)| {
                device == &flow.device.0
                    && (flow.channels.is_empty() || flow.channels.contains(channel))
            })
    }
    pub(super) fn restricts_scope(&self, scope: &RxScope) -> bool {
        scope
            .channels
            .iter()
            .any(|channel| self.fences.contains_key(&(scope.device.clone(), *channel)))
    }
    pub(super) fn revoke_all(&mut self, cause: Revoke) {
        let pending = std::mem::take(&mut self.pending);
        for attempt in pending {
            self.finish(
                &attempt,
                AttemptOutcome::Revoked(cause.clone()),
                None,
                Vec::new(),
            );
        }
    }
    pub(super) fn revoke_devices(&mut self, devices: &BTreeSet<String>, cause: Revoke) {
        let pending = std::mem::take(&mut self.pending);
        for attempt in pending {
            if devices.contains(&attempt.binding.device) {
                self.finish(
                    &attempt,
                    AttemptOutcome::Revoked(cause.clone()),
                    None,
                    Vec::new(),
                );
            } else {
                self.pending.push(attempt);
            }
        }
    }
    fn finish(
        &mut self,
        attempt: &Attempt,
        outcome: AttemptOutcome,
        recognition: Option<StoredRecognition>,
        carriers: Vec<CarrierRecognition>,
    ) {
        self.dispatches
            .retain(|link| link.successor != attempt.revision);
        // Each pending attempt reserves one terminal slot at commit. The actor
        // drains delivered records independently of the continuing fences.
        debug_assert!(self.terminal.len() < MAX_TERMINALS);
        self.terminal.push(Terminal {
            revision: attempt.revision,
            outcome,
            recognition,
            carriers,
            effect: ActionEffect::Unverified,
        });
    }
    fn drain_terminals(&mut self) -> Vec<Terminal> {
        std::mem::take(&mut self.terminal)
    }
}

struct ProposalReport<'a> {
    identity: Identity,
    binding: &'a Binding,
    snapshot: &'a [FlowSpec],
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct OutsideDifference {
    // Complete exact entries, including duplicates and tuning, remain visible.
    stored: Vec<FlowSpec>,
    observed: Vec<FlowSpec>,
}
struct PortalTransition {
    next: Authority,
    status: ReportStatus,
    outcomes: Vec<Terminal>,
    waiting: Vec<(Revision, AssociationError)>,
    outside: Option<OutsideDifference>,
}

fn association(
    attempt: &Attempt,
    snapshot: &[FlowSpec],
) -> Result<Vec<FlowSpec>, AssociationError> {
    let mut covered = BTreeSet::new();
    let mut inside = Vec::new();
    for flow in snapshot.iter().filter(|flow| attempt.scope.touches(flow)) {
        if !attempt.scope.contains(flow) || !attempt.binding.contains(flow) || flow.any_source {
            return Err(AssociationError::Carrier);
        }
        let selected = attempt.scope.flow_channels(flow);
        if selected.is_empty() || selected.iter().any(|c| !attempt.required.contains_key(c)) {
            return Err(AssociationError::Carrier);
        }
        let peer = address(flow.peer.as_deref()).ok_or(AssociationError::Peer)?;
        if selected
            .iter()
            .any(|c| attempt.required[c].peer() != Some(peer))
        {
            return Err(AssociationError::Peer);
        }
        covered.extend(selected);
        inside.push(flow.clone());
    }
    let wanted = attempt
        .required
        .iter()
        .filter_map(|(c, r)| r.peer().map(|_| *c))
        .collect();
    if covered != wanted {
        return Err(AssociationError::Coverage);
    }
    // Deliberately preserve duplicate IDs, channel order/duplicates, port zero
    // and tuning for the consuming validation step.
    Ok(inside)
}
fn carrier_recognitions(
    authority: &Authority,
    attempt: &Attempt,
    flows: &[FlowSpec],
) -> Vec<CarrierRecognition> {
    flows
        .iter()
        .map(|flow| CarrierRecognition {
            flow: flow.clone(),
            writer: authority
                .ledger
                .entries
                .get(&flow.id)
                .map_or(Writer::Unknown, |s| s.writer),
            revision: attempt.revision,
        })
        .collect()
}
impl Authority {
    fn portal_view(&self, binding: &Binding) -> Result<ExpectedView, PortalError> {
        let session = self
            .guards
            .session
            .as_ref()
            .ok_or(PortalError::Guard(Refusal::LifetimeUnknown))?;
        if session.binding != *binding {
            return Err(PortalError::Binding);
        }
        if self.portal.view_exhausted {
            return Err(PortalError::Capacity);
        }
        Ok(ExpectedView {
            instance: self.guards.instance(),
            lifetime: session.lifetime,
            binding: binding.clone(),
            generation: self.ledger.generation,
            epoch: epoch(&self.guards, &binding.device),
            revision: self.portal.view_revision,
            stored: self.stored.clone(),
            observed: self.portal.observed.clone(),
            saved: self.portal.saved.clone(),
            fences: self.portal.fences.clone(),
            inventory: self.portal.inventory.clone(),
        })
    }
    pub(super) fn note_portal_observation(
        &mut self,
        identity: Identity,
        binding: &Binding,
        snapshot: &[FlowSpec],
    ) {
        if self.guards.writer(identity, binding).is_err() {
            return;
        }
        let oversized = check_capacity([snapshot]).is_err();
        if !oversized
            && self
                .portal
                .observed
                .as_ref()
                .is_some_and(|(b, flows)| b == binding && same_set(flows, snapshot))
        {
            return;
        }
        match self.portal.view_revision.checked_add(1) {
            Some(revision) => {
                self.portal.view_revision = revision;
                // An oversized authenticated receipt still invalidates the view.
                // Do not retain its unbounded content or claim equality with it.
                self.portal.observed = (!oversized).then(|| (binding.clone(), snapshot.to_vec()));
            }
            None => self.portal.view_exhausted = true,
        }
    }
    fn set_portal_inventory(&mut self, inventory: Vec<Endpoint>) -> Result<(), PortalError> {
        if inventory.len() > MAX_NODES {
            return Err(PortalError::Capacity);
        }
        if inventory
            .iter()
            .map(|e| e.key)
            .collect::<BTreeSet<_>>()
            .len()
            != inventory.len()
        {
            return Err(PortalError::Inventory);
        }
        if self.portal.inventory != inventory {
            let changed_peers: BTreeSet<_> = self
                .portal
                .inventory
                .iter()
                .filter(|e| !inventory.contains(e))
                .chain(
                    inventory
                        .iter()
                        .filter(|e| !self.portal.inventory.contains(e)),
                )
                .map(|e| e.peer)
                .collect();
            let pending = std::mem::take(&mut self.portal.pending);
            for attempt in pending {
                if attempt
                    .required
                    .values()
                    .filter_map(Required::peer)
                    .any(|peer| changed_peers.contains(&peer))
                {
                    self.portal.finish(
                        &attempt,
                        AttemptOutcome::Revoked(Revoke::InventoryChanged),
                        None,
                        Vec::new(),
                    );
                } else {
                    self.portal.pending.push(attempt);
                }
            }
            self.portal.inventory = inventory;
        }
        Ok(())
    }
    fn portal_context(&self, attempt: &Attempt, env: &Environment) -> Result<(), PortalError> {
        if self.portal.view_exhausted {
            return Err(PortalError::Capacity);
        }
        if self.config.control.locked.contains(&ControlLock::Flows)
            || self
                .config
                .control
                .locked
                .contains(&ControlLock::Subscriptions)
        {
            return Err(PortalError::Guard(Refusal::Locked));
        }
        if self.ledger.generation != attempt.generation
            || epoch(&self.guards, &attempt.binding.device) != attempt.epoch
            || self
                .guards
                .writer(Identity::Verified(attempt.lifetime), &attempt.binding)
                .is_err()
        {
            return Err(PortalError::Invalidated);
        }
        let reviewed = RxScope::review(
            &attempt.scope.device,
            &attempt.scope.channels.iter().copied().collect::<Vec<_>>(),
            &self.config,
        )
        .map_err(PortalError::Guard)?;
        if reviewed != attempt.scope {
            return Err(PortalError::Invalidated);
        }
        for required in attempt.required.values() {
            if let Required::Selected(endpoint) = required {
                if self
                    .portal
                    .inventory
                    .iter()
                    .filter(|e| e.key == endpoint.key)
                    .collect::<Vec<_>>()
                    != vec![endpoint]
                {
                    return Err(PortalError::Inventory);
                }
            }
        }
        let configured = configured_specs(&self.config)
            .map_err(|e| PortalError::ConfigInvalid(vec![format!("{e:?}")]))?;
        check_capacity([self.stored.as_slice(), configured.as_slice()])
            .map_err(|_| PortalError::Capacity)?;
        if self.portal.pending.len() > MAX_ATTEMPTS
            || self.portal.pending.len() + self.portal.terminal.len() > MAX_TERMINALS
        {
            return Err(PortalError::Capacity);
        }
        let invalid = validation_errors(&[], &self.config, env);
        if !invalid.is_empty() {
            return Err(PortalError::ConfigInvalid(errors(invalid)));
        }
        if self
            .stored
            .iter()
            .map(|f| &f.id)
            .collect::<BTreeSet<_>>()
            .len()
            != self.stored.len()
        {
            return Err(PortalError::StoredAmbiguous);
        }
        let outside: Vec<_> = self
            .stored
            .iter()
            .filter(|f| !attempt.scope.touches(f))
            .cloned()
            .collect();
        let invalid = validation_errors(&outside, &self.config, env);
        if !invalid.is_empty() {
            return Err(PortalError::StoredOutsideInvalid(errors(invalid)));
        }
        Ok(())
    }

    fn retire_portal_pending(&mut self, now: u64, env: &Environment) {
        let pending = std::mem::take(&mut self.portal.pending);
        for attempt in pending {
            // Authority failure wins over expiry on both command and observation
            // paths. Retired intent cannot participate in a new scope closure;
            // its saved target and continuing revision fences remain intact.
            if let Err(error) = self.portal_context(&attempt, env) {
                self.portal.finish(
                    &attempt,
                    AttemptOutcome::Revoked(Revoke::Context(error)),
                    None,
                    Vec::new(),
                );
            } else if now >= attempt.expires {
                let matching = association(&attempt, &self.stored).ok();
                let recognition = matching.as_ref().map(|_| {
                    if attempt.required.values().all(|r| r.peer().is_none()) {
                        StoredRecognition::StoredVacant
                    } else {
                        StoredRecognition::StoredAlreadyPresent
                    }
                });
                let carriers = matching
                    .map(|flows| carrier_recognitions(self, &attempt, &flows))
                    .unwrap_or_default();
                self.portal
                    .finish(&attempt, AttemptOutcome::Expired, recognition, carriers);
            } else {
                self.portal.pending.push(attempt);
            }
        }
    }

    /// This is the existing daemon actor's operator command in a private model.
    /// No runtime command, callback, saved target or grant can call this path.
    fn commit_portal(
        &mut self,
        issuer: &mut PortalIssuer,
        expected: &ExpectedView,
        requested: &[Requested],
        clock: (u64, u64),
        env: &Environment,
    ) -> Result<Attempt, PortalError> {
        let binding = &expected.binding;
        let session = self
            .guards
            .session
            .as_ref()
            .ok_or(PortalError::Guard(Refusal::LifetimeUnknown))?;
        if binding != &session.binding || issuer.instance != self.guards.instance() {
            return Err(PortalError::Binding);
        }
        let lifetime = session.lifetime;
        if requested.is_empty()
            || requested.len() > MAX_CHANNEL_VALUES
            || requested
                .iter()
                .map(|r| r.channel)
                .collect::<BTreeSet<_>>()
                .len()
                != requested.len()
            || clock.1 == 0
            || clock.1 > MAX_DEADLINE
        {
            return Err(PortalError::InvalidRequest);
        }
        self.retire_portal_pending(clock.0, env);
        if !expected.same_state(&self.portal_view(binding)?) {
            return Err(PortalError::StaleView);
        }
        if self.portal.pending.len() >= MAX_ATTEMPTS
            || self.portal.pending.len() + self.portal.terminal.len() >= MAX_TERMINALS
        {
            return Err(PortalError::Capacity);
        }
        let expires = clock.0.checked_add(clock.1).ok_or(PortalError::Capacity)?;
        let mut dispatch = BTreeMap::new();
        for request in requested {
            let target = match request.endpoint {
                None => Required::Vacant,
                Some(key) => {
                    let matches: Vec<_> = self
                        .portal
                        .inventory
                        .iter()
                        .filter(|e| e.key == key && e.usable())
                        .collect();
                    if matches.len() != 1 {
                        return Err(PortalError::Inventory);
                    }
                    if expected
                        .inventory
                        .iter()
                        .filter(|e| e.key == key)
                        .collect::<Vec<_>>()
                        != matches
                    {
                        return Err(PortalError::StaleView);
                    }
                    Required::Selected(matches[0].clone())
                }
            };
            dispatch.insert(request.channel, target);
        }
        let peers: BTreeSet<_> = dispatch.values().filter_map(Required::peer).collect();
        let mut selected: BTreeSet<_> = dispatch.keys().copied().collect();
        let mut absorbed = BTreeSet::new();
        loop {
            let before = selected.len();
            let scope = RxScope::review(
                &binding.device,
                &selected.iter().copied().collect::<Vec<_>>(),
                &self.config,
            )
            .map_err(PortalError::Guard)?;
            for (i, pending) in self.portal.pending.iter().enumerate() {
                // Pending committed intent can coalesce into the same native
                // carrier before either report arrives. Treat a shared requested
                // peer as part of closure too; carry the whole old map forward.
                // This bounded refinement is explicit in the next review brief.
                if pending.binding == *binding
                    && (!pending.scope.channels.is_disjoint(&selected)
                        || pending
                            .required
                            .values()
                            .filter_map(Required::peer)
                            .any(|p| peers.contains(&p)))
                {
                    selected.extend(&pending.scope.channels);
                    absorbed.insert(i);
                }
            }
            let configured = configured_specs(&self.config)
                .map_err(|e| PortalError::ConfigInvalid(vec![format!("{e:?}")]))?;
            if configured.iter().any(|f| scope.touches(f)) {
                return Err(PortalError::Protected);
            }
            for flow in &self.stored {
                let same_peer = binding.contains(flow)
                    && flow.direction == Direction::Rx
                    && address(flow.peer.as_deref()).is_some_and(|p| peers.contains(&p))
                    && !self.ledger.protected(flow)
                    && !flow.any_source;
                if !scope.touches(flow) && !same_peer {
                    continue;
                }
                if self.ledger.protected(flow) {
                    return Err(PortalError::Protected);
                }
                if flow.any_source {
                    return Err(PortalError::AnySource);
                }
                if !binding.contains(flow) || address(flow.peer.as_deref()).is_none() {
                    return Err(PortalError::Binding);
                }
                selected.extend(channels(flow, &self.config));
            }
            if selected.len() == before {
                break;
            }
        }
        let scope = RxScope::review(
            &binding.device,
            &selected.iter().copied().collect::<Vec<_>>(),
            &self.config,
        )
        .map_err(PortalError::Guard)?;
        let mut required: BTreeMap<_, _> =
            selected.iter().map(|c| (*c, Required::Vacant)).collect();
        for flow in self.stored.iter().filter(|f| scope.touches(f)) {
            let peer = address(flow.peer.as_deref()).ok_or(PortalError::Binding)?;
            for channel in channels(flow, &self.config) {
                required.insert(channel, Required::Kept(peer));
            }
        }
        for i in &absorbed {
            required.extend(self.portal.pending[*i].required.clone());
        }
        required.extend(dispatch.clone());
        let mut attempt = Attempt {
            revision: Revision {
                instance: issuer.instance,
                serial: 0,
            },
            lifetime,
            binding: binding.clone(),
            scope,
            required,
            dispatch,
            generation: self.ledger.generation,
            epoch: epoch(&self.guards, &binding.device),
            expires,
        };
        self.portal_context(&attempt, env)?;
        let mut dispatches = self.portal.dispatches.clone();
        for link in &mut dispatches {
            if absorbed
                .iter()
                .any(|i| self.portal.pending[*i].revision == link.successor)
            {
                link.targets.retain(|channel, target| {
                    !attempt.dispatch.contains_key(channel)
                        && attempt.required.get(channel) == Some(target)
                });
                link.successor = attempt.revision;
            }
        }
        dispatches.retain(|link| !link.targets.is_empty());
        dispatches.push(DispatchLink {
            origin: attempt.revision,
            successor: attempt.revision,
            targets: attempt.dispatch.clone(),
        });
        if dispatches.len() > MAX_TERMINALS {
            return Err(PortalError::Capacity);
        }
        attempt.revision = issuer.issue()?;
        // The placeholder serial is never issued. Replace it only after every
        // fallible check, preserving the issuer outside resettable Authority.
        for link in &mut dispatches {
            if link.origin.serial == 0 {
                link.origin = attempt.revision;
            }
            if link.successor.serial == 0 {
                link.successor = attempt.revision;
            }
        }
        self.portal.dispatches = dispatches;
        // All mutations commit together before dispatch. Supersession consumes
        // the whole old attempt; no old rights are restored on its remainder.
        let pending = std::mem::take(&mut self.portal.pending);
        for (i, old) in pending.into_iter().enumerate() {
            if absorbed.contains(&i) {
                self.portal.finish(
                    &old,
                    AttemptOutcome::Revoked(
                        if old
                            .dispatch
                            .keys()
                            .any(|c| attempt.dispatch.contains_key(c))
                        {
                            Revoke::Superseded(attempt.revision)
                        } else {
                            Revoke::Coalesced(attempt.revision)
                        },
                    ),
                    None,
                    Vec::new(),
                );
            } else {
                self.portal.pending.push(old);
            }
        }
        self.guards.restrict_scope(&attempt.scope);
        self.baseline.retain(|b| !attempt.scope.touches(&b.flow));
        for channel in &attempt.scope.channels {
            self.portal
                .fences
                .insert((binding.device.clone(), *channel), attempt.revision);
        }
        for (channel, target) in &attempt.dispatch {
            self.portal
                .saved
                .insert((binding.device.clone(), *channel), target.clone());
        }
        self.portal.pending.push(attempt.clone());
        Ok(attempt)
    }
    fn cancel_portal(&mut self, revision: Revision, dispatch_failed: bool) {
        if dispatch_failed {
            self.portal_dispatch_result(revision, DispatchOutcome::Failed);
            return;
        }
        if let Some(i) = self
            .portal
            .pending
            .iter()
            .position(|a| a.revision == revision)
        {
            let attempt = self.portal.pending.remove(i);
            self.portal.finish(
                &attempt,
                AttemptOutcome::Revoked(Revoke::Cancelled),
                None,
                Vec::new(),
            );
        }
    }
    fn portal_dispatch_result(
        &mut self,
        revision: Revision,
        result: DispatchOutcome,
    ) -> Option<Revision> {
        // The actor records every receipt, including Unknown/late/unmatched
        // results. This reducer never retries or treats timeout as rejection.
        if result == DispatchOutcome::Unknown {
            return None;
        }
        let index = self
            .portal
            .dispatches
            .iter()
            .position(|link| link.origin == revision)?;
        let link = self.portal.dispatches.remove(index);
        if result == DispatchOutcome::Done {
            return None;
        }
        let index = self.portal.pending.iter().position(|attempt| {
            attempt.revision == link.successor
                && link
                    .targets
                    .iter()
                    .any(|(channel, target)| attempt.required.get(channel) == Some(target))
        })?;
        let attempt = self.portal.pending.remove(index);
        self.portal.finish(
            &attempt,
            AttemptOutcome::Revoked(Revoke::DispatchFailed(revision)),
            None,
            Vec::new(),
        );
        Some(attempt.revision)
    }

    fn portal_proposal(
        &self,
        proposal: ProposalReport<'_>,
        now: u64,
        env: &Environment,
    ) -> PortalTransition {
        self.portal_with_backstop(proposal, now, env, validation_errors)
    }
    fn portal_with_backstop(
        &self,
        proposal: ProposalReport<'_>,
        now: u64,
        env: &Environment,
        backstop: impl FnOnce(&[FlowSpec], &Config, &Environment) -> Vec<Issue>,
    ) -> PortalTransition {
        let mut next = self.clone();
        let terminal_start = next.portal.terminal.len();
        next.note_portal_observation(proposal.identity, proposal.binding, proposal.snapshot);
        next.retire_portal_pending(now, env);
        let mut candidates = std::mem::take(&mut next.portal.pending);
        candidates.sort_by_key(|a| a.revision.serial);
        let selected_scopes: Vec<_> = candidates.iter().map(|a| a.scope.clone()).collect();
        let mut waiting = Vec::new();
        for attempt in candidates {
            let association = if proposal.identity != Identity::Verified(attempt.lifetime) {
                Err(AssociationError::Holder)
            } else if proposal.binding != &attempt.binding {
                Err(AssociationError::Binding)
            } else if check_capacity([proposal.snapshot]).is_err() {
                Err(AssociationError::Capacity)
            } else {
                association(&attempt, proposal.snapshot)
            };
            let observed = match association {
                Ok(observed) => observed,
                Err(error) => {
                    waiting.push((attempt.revision, error));
                    next.portal.pending.push(attempt);
                    continue;
                }
            };
            // Consumed here, before port/duplicates/overlap/tuning/protection
            // validation. Never reinserted after a content refusal or rollback.
            let mut candidate: Vec<_> = next
                .stored
                .iter()
                .filter(|f| !attempt.scope.touches(f))
                .cloned()
                .collect();
            candidate.extend(observed.iter().cloned());
            let removal_only = !validation_errors(&self.stored, &self.config, env).is_empty();
            let invalid = validation_errors(&candidate, &next.config, env);
            let protected = next
                .stored
                .iter()
                .any(|f| attempt.scope.touches(f) && next.ledger.protected(f));
            if protected
                || (removal_only && !subset(&candidate, &self.stored))
                || (!removal_only && !invalid.is_empty())
            {
                let reasons = if protected {
                    vec!["Protected".into()]
                } else if removal_only {
                    vec!["RemovalOnly".into()]
                } else {
                    errors(invalid)
                };
                next.portal
                    .finish(&attempt, AttemptOutcome::Refused(reasons), None, Vec::new());
                continue;
            }
            let unchanged = same_set(
                &observed,
                &next
                    .stored
                    .iter()
                    .filter(|f| attempt.scope.touches(f))
                    .cloned()
                    .collect::<Vec<_>>(),
            );
            // Preserve unchanged carriers in their exact slots. Append only new
            // carriers in observed order; validation used full multiplicity.
            let mut committed: Vec<_> = next
                .stored
                .iter()
                .filter(|f| !attempt.scope.touches(f) || observed.contains(f))
                .cloned()
                .collect();
            for flow in &observed {
                if !committed.contains(flow) {
                    committed.push(flow.clone());
                }
            }
            next.ledger
                .entries
                .retain(|_, stamp| committed.contains(&stamp.flow));
            for flow in observed.iter().filter(|f| !next.stored.contains(f)) {
                next.ledger.entries.insert(
                    flow.id.clone(),
                    EntryStamp {
                        flow: flow.clone(),
                        writer: Writer::Portal(attempt.lifetime, attempt.revision.serial),
                        generation: next.ledger.generation,
                        epoch: attempt.epoch,
                    },
                );
            }
            next.stored = committed;
            let carriers = carrier_recognitions(&next, &attempt, &observed);
            next.portal.finish(
                &attempt,
                if unchanged {
                    AttemptOutcome::Unchanged
                } else {
                    AttemptOutcome::Admitted
                },
                None,
                carriers,
            );
        }
        let invalid = backstop(&next.stored, &next.config, env);
        let removal_only = !validation_errors(&self.stored, &self.config, env).is_empty();
        if (!removal_only && !invalid.is_empty())
            || (removal_only && !subsequence(&next.stored, &self.stored))
        {
            next.stored = self.stored.clone();
            next.ledger = self.ledger.clone();
            next.baseline = self.baseline.clone();
            for terminal in &mut next.portal.terminal[terminal_start..] {
                if matches!(
                    terminal.outcome,
                    AttemptOutcome::Admitted | AttemptOutcome::Unchanged
                ) {
                    terminal.outcome = AttemptOutcome::RolledBack(errors(invalid.clone()));
                    terminal.carriers.clear();
                }
            }
        }
        let outcomes = next.portal.terminal[terminal_start..].to_vec();
        let successful = |t: &Terminal| {
            matches!(
                t.outcome,
                AttemptOutcome::Admitted | AttemptOutcome::Unchanged
            )
        };
        let status = if !outcomes.is_empty()
            && outcomes.iter().all(successful)
            && waiting.is_empty()
            && invalid.is_empty()
        {
            ReportStatus::Complete
        } else if outcomes.iter().any(successful) {
            ReportStatus::Partial
        } else {
            ReportStatus::Refused
        };
        let bounded_authenticated = self
            .guards
            .writer(proposal.identity, proposal.binding)
            .is_ok()
            && check_capacity([proposal.snapshot]).is_ok();
        let outside_stored: Vec<_> = if bounded_authenticated {
            self.stored
                .iter()
                .filter(|f| !selected_scopes.iter().any(|s| s.touches(f)))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let outside_observed: Vec<_> = if bounded_authenticated {
            proposal
                .snapshot
                .iter()
                .filter(|f| !selected_scopes.iter().any(|s| s.touches(f)))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        // An unauthenticated report never supplies attributed outside evidence.
        let outside = (bounded_authenticated && !same_set(&outside_stored, &outside_observed))
            .then_some(OutsideDifference {
                stored: outside_stored,
                observed: outside_observed,
            });
        PortalTransition {
            next,
            status,
            outcomes,
            waiting,
            outside,
        }
    }
}
