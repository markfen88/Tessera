//! Default-off observation of actual StateEngine acceptance sites. This is not
//! a report admission path. The existing engine alone decides/persists commands.
//! Runtime reports remain unverified; only tests supply holder/launch premises.
use super::*;
use std::collections::VecDeque;
use tessera_control_types::{Command, Origin};

const TRACE_CAPACITY: usize = 64;
mod budget;
#[cfg(feature = "routing-shadow-test-hooks")]
pub mod test_hooks;

/// No client-supplied labels or command payloads are retained in the trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OriginClass {
    Protocol,
    Cli,
    Config,
    Api,
    Internal,
    Other,
}
impl From<&Origin> for OriginClass {
    fn from(origin: &Origin) -> Self {
        match origin {
            Origin::Protocol { .. } => Self::Protocol,
            Origin::Cli { .. } => Self::Cli,
            Origin::Config => Self::Config,
            Origin::Api { .. } => Self::Api,
            Origin::Internal => Self::Internal,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Accepted {
    origin: OriginClass,
    kind: &'static str,
    changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnverifiedReport {
    flows: usize,
    within_budget: bool,
    // Exact order and content only. Neither fact implies authentication,
    // production order, deliberately changed intent or native effect.
    matches_intent: Option<bool>,
    shadow_matches_intent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeFailure {
    Capacity,
    Transition,
    ProjectionMismatch,
    PrimarySelection,
    ManagedUnattributed,
    LegacySourceUnmodeled,
    SubscriptionUnmodeled,
}

#[derive(Debug)]
pub(crate) struct AcceptanceProbe {
    authority: Authority,
    // Exact Option projection: None and Some([]) have different legacy effects.
    projection: Option<Vec<FlowSpec>>,
    accepted: VecDeque<Accepted>,
    dropped: usize,
    reports: VecDeque<UnverifiedReport>,
    dropped_reports: usize,
    failure: Option<ProbeFailure>,
    #[cfg(feature = "routing-shadow-test-hooks")]
    premise: Option<test_hooks::PremiseWorld>,
}

impl AcceptanceProbe {
    pub(crate) fn new(
        config: &Config,
        devices: &[crate::model::DeviceModel],
        intent: &crate::persist::StateFile,
        instance: u128,
    ) -> Option<Self> {
        if instance == 0
            || legacy_source_unmodeled(config, intent)
            || !budget::context(config, devices, intent)
        {
            return None;
        }
        Some(Self {
            authority: Authority::new(
                config,
                devices,
                intent.protocol_flows.clone().unwrap_or_default(),
                launch::DaemonInstance(instance),
            ),
            projection: intent.protocol_flows.clone(),
            accepted: VecDeque::new(),
            dropped: 0,
            reports: VecDeque::new(),
            dropped_reports: 0,
            failure: None,
            #[cfg(feature = "routing-shadow-test-hooks")]
            premise: None,
        })
    }

    pub(crate) fn observe_unverified(
        &mut self,
        snapshot: &[FlowSpec],
        intent: &crate::persist::StateFile,
    ) {
        if self.reports.len() == TRACE_CAPACITY {
            self.reports.pop_front();
            self.dropped_reports = self.dropped_reports.saturating_add(1);
        }
        let within_budget = budget::flows(snapshot);
        let stored = intent.protocol_flows.as_deref().unwrap_or_default();
        self.reports.push_back(UnverifiedReport {
            flows: snapshot.len(),
            within_budget,
            matches_intent: (within_budget
                && !legacy_source_unmodeled(&self.authority.config, intent))
            .then(|| snapshot == stored),
            shadow_matches_intent: self.projection == intent.protocol_flows
                && self.authority.stored == stored,
        });
        // Do not call note_portal_observation, retire, foreign_write or a
        // reducer here. Unverified receipt cannot change any authority state.
    }

    fn stop(&mut self, cause: ProbeFailure) {
        self.authority.end_lifetime();
        #[cfg(feature = "routing-shadow-test-hooks")]
        if let Some(premise) = &mut self.premise {
            premise.authority.end_lifetime();
        }
        self.failure = Some(cause);
    }

    fn has_premise_management(&self) -> bool {
        #[cfg(feature = "routing-shadow-test-hooks")]
        if self.premise.as_ref().is_some_and(|p| p.is_managed()) {
            return true;
        }
        self.authority.guards.session.is_some()
            || !self.authority.portal.saved.is_empty()
            || !self.authority.portal.fences.is_empty()
    }

    fn portal_touches_device(&self, device: &str) -> bool {
        let touches = |a: &Authority| {
            a.portal.saved.keys().any(|(d, _)| d == device)
                || a.portal.fences.keys().any(|(d, _)| d == device)
                || a.portal.pending.iter().any(|p| p.binding.device == device)
        };
        #[cfg(feature = "routing-shadow-test-hooks")]
        if self.premise.as_ref().is_some_and(|p| touches(&p.authority)) {
            return true;
        }
        touches(&self.authority)
    }

    pub(crate) fn accepted(
        &mut self,
        origin: &Origin,
        command: &Command,
        changed: bool,
        config: &Config,
        devices: &[crate::model::DeviceModel],
        intent: &crate::persist::StateFile,
    ) {
        if self.accepted.len() == TRACE_CAPACITY {
            self.accepted.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.accepted.push_back(Accepted {
            origin: origin.into(),
            kind: command.kind(),
            changed,
        });
        if self.failure.is_some() {
            return;
        }
        // Runtime primary selection lives in intent, outside Authority::reload's
        // current context. Its accepted change invalidates the old binding.
        // A clear that does not keep the network settings drops it too.
        if changed
            && matches!(
                command,
                Command::SetPrimaryInterface { .. }
                    | Command::ClearConfiguration {
                        keep_network: false,
                        ..
                    }
            )
        {
            self.stop(ProbeFailure::PrimarySelection);
            return;
        }
        if legacy_source_unmodeled(config, intent) {
            self.stop(ProbeFailure::LegacySourceUnmodeled);
            return;
        }
        // Clearing a device's configuration removes its subscriptions too.
        if !matches!(origin, Origin::Protocol { .. })
            && matches!(command, Command::Subscribe { device, .. }
                | Command::Unsubscribe { device, .. }
                | Command::ClearConfiguration { device, .. }
                if self.portal_touches_device(&device.0))
        {
            self.stop(ProbeFailure::SubscriptionUnmodeled);
            return;
        }
        // Until attributed ingress exists, a Protocol accepted write may never
        // silently become an external write in a premise-managed world.
        if matches!(origin, Origin::Protocol { .. }) && self.has_premise_management() {
            self.stop(ProbeFailure::ManagedUnattributed);
            return;
        }
        if !budget::context(config, devices, intent) {
            self.stop(ProbeFailure::Capacity);
            return;
        }
        let result = match command {
            Command::SetProtocolFlows { flows } => self
                .authority
                .foreign_write(flows, WriteDisposition::Accepted)
                .map_err(|_| ProbeFailure::Transition),
            _ => self
                .authority
                .reload(config, devices)
                .map(|next| self.authority = next)
                .map_err(|_| ProbeFailure::Transition),
        };
        if let Err(error) = result {
            // Observation cannot alter a real accepted command. Stop this
            // shadow lifetime rather than keep exercising unfenced old rights.
            self.stop(error);
        }
        if self.failure.is_none()
            && self.authority.stored != intent.protocol_flows.as_deref().unwrap_or_default()
        {
            self.stop(ProbeFailure::ProjectionMismatch);
        }
        if self.failure.is_none() {
            self.projection = intent.protocol_flows.clone();
            #[cfg(feature = "routing-shadow-test-hooks")]
            if let Some(premise) = &mut self.premise {
                if premise.accepted(command, config, devices).is_err() {
                    self.stop(ProbeFailure::Transition);
                }
            }
        }
        // Subscribe/Unsubscribe receipts describe acceptance only. Saved-target
        // supersession/classification is not integrated by this preparation.
    }
}

fn legacy_source_unmodeled(config: &Config, intent: &crate::persist::StateFile) -> bool {
    intent.protocol_flows.is_none()
        && !config.legacy_protocol_flows.is_empty()
        && !config.control.locked.contains(&ControlLock::Flows)
}
