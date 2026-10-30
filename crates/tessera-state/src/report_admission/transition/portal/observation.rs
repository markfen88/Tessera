//! One private observation transaction. The RX legacy exception is an explicit
//! preparation policy, disabled by default; it supplies no deliberate native
//! supersession or callback order. Production mutation remains unavailable.
use super::*;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum LegacyPolicy {
    #[default]
    Disabled,
    OutsidePortalFootprint,
    // Separate private preparation policy. The observer and test-actor facade
    // cannot select it. Omission is unverified and may remove admin-written TX.
    OutsidePortalWithTxRemoval,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LegacyRefusal {
    PolicyDisabled,
    PortalDependency,
    Protected,
    OutsideBinding,
    OtherReportDomain,
    UnresolvedReportDomain,
    CrossReportDomain,
    // This first combined slice integrates RX only. The separate bounded TX
    // role must get a reviewed observation boundary before activation.
    TxObservationUnsupported,
    // A protected TX omission is policy-ineligible, rather than a failed
    // member of the eligible RX/whole-TX-removal batch.
    ProtectedTxOmission,
    RemovalOnly,
    Invalid(Vec<String>),
    BatchRefused(Vec<String>),
    RolledBack(Vec<String>),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LegacyUnit {
    component: Component,
    stored: Vec<FlowSpec>,
    observed: Vec<FlowSpec>,
    pub(super) refusal: Option<LegacyRefusal>,
    // A whole-carrier omission proposal, never proof of native unsubscribe.
    tx_omission_unverified: bool,
}
pub(super) struct ObservationTransition {
    pub(super) next: Authority,
    pub(super) status: ReportStatus,
    pub(super) context: Option<ContextRefusal>,
    pub(super) outcomes: Vec<Terminal>,
    pub(super) waiting: Vec<(Revision, AssociationError)>,
    pub(super) outside: Option<OutsideDifference>,
    pub(super) legacy: Vec<LegacyUnit>,
    // Seeds for the common resource graph, independent of continuing generic RX
    // restrictions. Narrowing these vertices grants no runtime rights.
    pub(super) legacy_footprint: BTreeMap<String, BTreeSet<u16>>,
}

impl PortalState {
    fn legacy_footprint(&self) -> BTreeMap<String, BTreeSet<u16>> {
        // These are resource vertices, not a second carrier-closure algorithm.
        // The common graph closes over old/current/baseline/configured/observed
        // carriers and conflicts. Saved keys survive geometry loss unchanged.
        let mut footprint: BTreeMap<String, BTreeSet<u16>> = BTreeMap::new();
        for (device, channel) in self.saved.keys() {
            footprint
                .entry(device.clone())
                .or_default()
                .insert(*channel);
        }
        for attempt in &self.pending {
            footprint
                .entry(attempt.scope.device.clone())
                .or_default()
                .extend(&attempt.scope.channels);
        }
        footprint
    }
}

impl Authority {
    pub(super) fn observation_proposal(
        &self,
        proposal: ProposalReport<'_>,
        now: u64,
        policy: LegacyPolicy,
        env: &Environment,
    ) -> ObservationTransition {
        self.observation_with_backstop(proposal, now, policy, env, validation_errors)
    }

    pub(super) fn observation_with_backstop(
        &self,
        proposal: ProposalReport<'_>,
        now: u64,
        policy: LegacyPolicy,
        env: &Environment,
        backstop: impl FnOnce(&[FlowSpec], &Config, &Environment) -> Vec<Issue>,
    ) -> ObservationTransition {
        let terminal_start = self.portal.terminal.len();
        // Portal unit validation runs against retained outside intent. Defer
        // the aggregate backstop until both paths have built one candidate.
        let portal = self.portal_with_backstop(
            ProposalReport {
                identity: proposal.identity,
                binding: proposal.binding,
                snapshot: proposal.snapshot,
            },
            now,
            env,
            |_, _, _| Vec::new(),
        );
        let mut result = ObservationTransition {
            next: portal.next,
            status: ReportStatus::Refused,
            context: None,
            outcomes: portal.outcomes,
            waiting: portal.waiting,
            outside: portal.outside,
            legacy: Vec::new(),
            legacy_footprint: BTreeMap::new(),
        };
        let context = (|| {
            self.guards
                .writer(proposal.identity, proposal.binding)
                .map_err(ContextRefusal::Guard)?;
            if self.config.control.locked.contains(&ControlLock::Flows)
                || (self
                    .config
                    .control
                    .locked
                    .contains(&ControlLock::Subscriptions)
                    && !same_set(&self.stored, proposal.snapshot))
            {
                return Err(ContextRefusal::Guard(Refusal::Locked));
            }
            let invalid = validation_errors(&[], &self.config, env);
            if !invalid.is_empty() {
                return Err(ContextRefusal::ConfigInvalid(errors(invalid)));
            }
            if self
                .stored
                .iter()
                .map(|f| &f.id)
                .collect::<BTreeSet<_>>()
                .len()
                != self.stored.len()
            {
                return Err(ContextRefusal::StoredAmbiguous(Vec::new()));
            }
            Ok(())
        })();
        if let Err(error) = context {
            return result.rollback(self, terminal_start, error);
        }
        // Include old complete carriers as well as admitted baselines and the
        // working candidate. A portal replacement cannot erase graph evidence
        // that would have joined a legacy rename/removal to a fenced resource.
        let baseline: Vec<_> = self
            .baseline
            .iter()
            .map(|b| b.flow.clone())
            .chain(self.stored.iter().cloned())
            .collect();
        let stored = result.next.stored.clone();
        let vertices: Vec<_> = result.next.portal.legacy_footprint().into_iter().collect();
        let groups = match resource_components(
            Inputs {
                baseline: &baseline,
                observed: proposal.snapshot,
                stored: &stored,
            },
            &self.config,
            &vertices,
        ) {
            Ok(groups) => groups,
            Err(error) => {
                let refusal = match error {
                    ModelError::Capacity => ContextRefusal::Capacity,
                    other => ContextRefusal::ConfigInvalid(vec![format!("{other:?}")]),
                };
                return result.rollback(self, terminal_start, refusal);
            }
        };
        let removal_only = !validation_errors(&self.stored, &self.config, env).is_empty();
        let configured = match configured_specs(&self.config) {
            Ok(flows) => flows,
            Err(error) => {
                return result.rollback(
                    self,
                    terminal_start,
                    ContextRefusal::ConfigInvalid(vec![format!("{error:?}")]),
                );
            }
        };
        // A replacement can change every graph key (ID, peer and port). Before
        // admitting any TX omission, the complete global TX observation
        // must contain only exact current carriers, preserving multiplicity.
        // Otherwise retain all TX components as divergence; independent RX
        // components can still participate in the legacy batch.
        let stored_tx: Vec<_> = self
            .stored
            .iter()
            .filter(|f| f.direction == Direction::Tx)
            .cloned()
            .collect();
        let observed_tx: Vec<_> = proposal
            .snapshot
            .iter()
            .filter(|f| f.direction == Direction::Tx)
            .cloned()
            .collect();
        let report_tx_removal_only = subset(&observed_tx, &stored_tx);
        for component in groups {
            if component.stored.is_empty() && component.observed.is_empty() {
                continue;
            }
            let prior: Vec<_> = component
                .stored
                .iter()
                .map(|i| stored[*i].clone())
                .collect();
            let observed: Vec<_> = component
                .observed
                .iter()
                .map(|i| proposal.snapshot[*i].clone())
                .collect();
            let unchanged = same_set(&prior, &observed);
            // Keep the full graph, including original/baseline/configured
            // carriers. Projecting first could hide a cross-domain conflict.
            let membership: Vec<_> = component
                .baseline
                .iter()
                .map(|i| &baseline[*i])
                .chain(component.stored.iter().map(|i| &stored[*i]))
                .chain(component.observed.iter().map(|i| &proposal.snapshot[*i]))
                .chain(component.configured.iter().map(|i| &configured[*i]))
                .map(|flow| report_domain::partition(proposal.binding, flow))
                .collect();
            let domain_refusal = if membership.contains(&report_domain::Partition::Unresolved) {
                Some(LegacyRefusal::UnresolvedReportDomain)
            } else if membership.contains(&report_domain::Partition::InDomain)
                && membership.contains(&report_domain::Partition::OtherDomain)
            {
                Some(LegacyRefusal::CrossReportDomain)
            } else if membership.contains(&report_domain::Partition::OtherDomain) {
                Some(LegacyRefusal::OtherReportDomain)
            } else {
                None
            };
            let changed: Vec<_> = prior
                .iter()
                .filter(|f| !observed.contains(f))
                .chain(observed.iter().filter(|f| !prior.contains(f)))
                .collect();
            let has_tx = prior
                .iter()
                .chain(&observed)
                .any(|f| f.direction == Direction::Tx);
            let protected_tx_omission = prior.iter().any(|f| {
                f.direction == Direction::Tx && self.ledger.protected(f) && !observed.contains(f)
            });
            let tx_omission = has_tx
                && policy == LegacyPolicy::OutsidePortalWithTxRemoval
                && report_tx_removal_only
                && !prior
                    .iter()
                    .any(|f| self.ledger.protected(f) && !observed.contains(f))
                && prior
                    .iter()
                    .chain(&observed)
                    .all(|f| f.direction == Direction::Tx && proposal.binding.contains(f))
                && subset(&observed, &prior);
            let refusal = if unchanged {
                None
            } else if domain_refusal.is_some() {
                domain_refusal
            } else if !component.scopes.is_empty() {
                Some(LegacyRefusal::PortalDependency)
            } else if policy == LegacyPolicy::Disabled {
                Some(LegacyRefusal::PolicyDisabled)
            } else if protected_tx_omission {
                Some(LegacyRefusal::ProtectedTxOmission)
            } else if has_tx && !tx_omission {
                Some(LegacyRefusal::TxObservationUnsupported)
            } else if changed.iter().any(|f| !proposal.binding.contains(f)) {
                Some(LegacyRefusal::OutsideBinding)
            } else if prior
                .iter()
                .any(|f| self.ledger.protected(f) && !observed.contains(f))
            {
                Some(LegacyRefusal::Protected)
            } else {
                None
            };
            result.legacy.push(LegacyUnit {
                component,
                stored: prior,
                observed,
                refusal,
                tx_omission_unverified: !unchanged && tx_omission,
            });
        }
        // Graph eligibility excludes fenced components. Every remaining changed
        // unit belongs to ONE legacy batch, including content that later fails
        // binding/protection/validation. Never adopt a valid sibling beside it.
        let batch: Vec<_> = result
            .legacy
            .iter()
            .enumerate()
            .filter(|(_, unit)| {
                !same_set(&unit.stored, &unit.observed)
                    && unit.component.scopes.is_empty()
                    // Policy-ineligible TX components remain divergence; they
                    // cannot starve the eligible outside-footprint RX batch.
                    && !matches!(unit.refusal, Some(LegacyRefusal::TxObservationUnsupported
                        | LegacyRefusal::ProtectedTxOmission))
                    && !matches!(unit.refusal, Some(LegacyRefusal::OtherReportDomain
                        | LegacyRefusal::UnresolvedReportDomain | LegacyRefusal::CrossReportDomain))
                    && policy != LegacyPolicy::Disabled
            })
            .map(|(index, _)| index)
            .collect();
        let selected_stored: BTreeSet<_> = batch
            .iter()
            .flat_map(|i| result.legacy[*i].component.stored.iter().copied())
            .collect();
        let selected_observed: BTreeSet<_> = batch
            .iter()
            .flat_map(|i| result.legacy[*i].component.observed.iter().copied())
            .collect();
        // Validation replaces complete units, preserving observation multiplicity.
        let mut candidate: Vec<_> = stored
            .iter()
            .enumerate()
            .filter(|(i, _)| !selected_stored.contains(i))
            .map(|(_, f)| f.clone())
            .collect();
        candidate.extend(
            proposal
                .snapshot
                .iter()
                .enumerate()
                .filter(|(i, _)| selected_observed.contains(i))
                .map(|(_, f)| f.clone()),
        );
        let initial_refusal = batch.iter().find_map(|i| result.legacy[*i].refusal.clone());
        let refusal = initial_refusal.or_else(|| {
            if removal_only && !subset(&candidate, &self.stored) {
                Some(LegacyRefusal::RemovalOnly)
            } else {
                let invalid = validation_errors(&candidate, &self.config, env);
                (!removal_only && !invalid.is_empty())
                    .then(|| LegacyRefusal::Invalid(errors(invalid)))
            }
        });
        if let Some(refusal) = refusal {
            for index in &batch {
                result.legacy[*index].refusal.get_or_insert_with(|| {
                    LegacyRefusal::BatchRefused(vec![format!("{refusal:?}")])
                });
            }
        } else if !batch.is_empty() {
            // Preserve exact unchanged slots. All observed duplicates were
            // checked above; new entries append in the report's original order.
            result.next.stored = stored
                .iter()
                .enumerate()
                .filter(|(i, f)| {
                    !selected_stored.contains(i)
                        || selected_observed
                            .iter()
                            .any(|o| proposal.snapshot[*o] == **f)
                })
                .map(|(_, f)| f.clone())
                .collect();
            for (i, flow) in proposal.snapshot.iter().enumerate() {
                if selected_observed.contains(&i) && !result.next.stored.contains(flow) {
                    result.next.stored.push(flow.clone());
                }
            }
            result
                .next
                .ledger
                .entries
                .retain(|_, s| result.next.stored.contains(&s.flow));
            for index in &batch {
                let unit = &result.legacy[*index];
                for flow in unit.observed.iter().filter(|f| !unit.stored.contains(f)) {
                    result.next.ledger.entries.insert(
                        flow.id.clone(),
                        EntryStamp {
                            flow: flow.clone(),
                            writer: Writer::Foreign,
                            generation: result.next.ledger.generation,
                            epoch: epoch(&result.next.guards, &flow.device.0),
                        },
                    );
                }
                result.next.baseline.retain(|b| {
                    !unit
                        .component
                        .baseline
                        .iter()
                        .any(|i| baseline[*i] == b.flow)
                });
            }
        }
        let invalid = backstop(&result.next.stored, &self.config, env);
        if (!removal_only && !invalid.is_empty())
            || (removal_only && !subsequence(&result.next.stored, &self.stored))
        {
            return result.rollback(
                self,
                terminal_start,
                ContextRefusal::AggregateConflict(errors(invalid)),
            );
        }
        let complete = result.outcomes.iter().all(|t| {
            matches!(
                t.outcome,
                AttemptOutcome::Admitted | AttemptOutcome::Unchanged
            )
        }) && result.waiting.is_empty()
            && result.legacy.iter().all(|u| u.refusal.is_none())
            && invalid.is_empty();
        result.status = if complete {
            ReportStatus::Complete
        } else if result.next.stored != self.stored {
            ReportStatus::Partial
        } else {
            ReportStatus::Refused
        };
        result.legacy_footprint = result.next.portal.legacy_footprint();
        result
    }
}

impl ObservationTransition {
    fn rollback(
        mut self,
        before: &Authority,
        terminal_start: usize,
        context: ContextRefusal,
    ) -> Self {
        self.next.stored = before.stored.clone();
        self.next.ledger = before.ledger.clone();
        self.next.baseline = before.baseline.clone();
        let reason = vec![format!("{context:?}")];
        for terminal in &mut self.next.portal.terminal[terminal_start..] {
            if matches!(
                terminal.outcome,
                AttemptOutcome::Admitted | AttemptOutcome::Unchanged
            ) {
                terminal.outcome = AttemptOutcome::RolledBack(reason.clone());
                terminal.carriers.clear();
            }
        }
        for unit in &mut self.legacy {
            if unit.refusal.is_none() && !same_set(&unit.stored, &unit.observed) {
                unit.refusal = Some(LegacyRefusal::RolledBack(reason.clone()));
            }
        }
        self.outcomes = self.next.portal.terminal[terminal_start..].to_vec();
        self.status = ReportStatus::Refused;
        self.context = Some(context);
        self.legacy_footprint = self.next.portal.legacy_footprint();
        self
    }
}
