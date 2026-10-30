//! Durable provenance and explicit transfer of retired-runtime occurrences.
//! Nothing here adopts legacy bytes, mutates a receiver, or clears a vacancy
//! fence. The daemon authenticates the live run and retires operation workers.
use super::*;
use crate::persist::{RuntimeCarrierOrigin, RuntimeCarrierOwnership};

/// An exact reviewed carrier baseline. It is not a wire token or authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCarrierReview {
    pub invocation: String,
    pub protection_revision: u64,
    pub binding: String,
    pub flows: Vec<FlowSpec>,
    pub origins: Vec<RuntimeCarrierOrigin>,
    /// Zero-based stored occurrences, never an ID/content match.
    pub releasable: Vec<u32>,
    pub unavailable: Option<String>,
    pub saved: Vec<crate::persist::SubscriptionIntent>,
    pub grant_occurrences: Vec<u32>,
    pub grant_unavailable: Option<String>,
    pub configured: Vec<FlowSpec>,
}

pub(super) fn valid_invocation(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && value.bytes().any(|b| b != b'0')
}

impl StateEngine {
    pub(super) fn carrier_binding(&self) -> Result<String, CommandError> {
        #[derive(serde::Serialize)]
        struct Projection<'a> {
            config: &'a Config,
            first: Option<tessera_control_types::DeviceStatus>,
            wire_id: Option<String>,
            selected: &'a Option<String>,
            network: &'a Option<NetworkStatus>,
        }
        let mut first = self.status().devices.into_iter().next();
        if let Some(device) = &mut first {
            device.subscriptions.clear();
            for channel in device.rx_channels.iter_mut().chain(&mut device.tx_channels) {
                channel.routing = None;
            }
        }
        let text = toml::to_string(&Projection {
            config: &self.config.config,
            first,
            wire_id: self
                .model
                .devices
                .first()
                .map(|d| d.wire_device_id.to_hex()),
            selected: &self.model.network.selected,
            network: &self.host_network,
        })
        .map_err(|_| unavailable("cannot describe carrier binding"))?;
        if text.len() > 64 * 1024 {
            return Err(unavailable("carrier binding exceeds the recovery budget"));
        }
        Ok(text)
    }

    pub(super) fn protect_carrier_provenance(&mut self) {
        self.intent.runtime_carriers = None;
        self.intent.runtime_carrier_grant = None;
        match self.intent.runtime_protection_revision.checked_add(1) {
            Some(next) if next <= i64::MAX as u64 => {
                self.intent.runtime_protection_revision = next;
            }
            _ => {
                self.intent.runtime_reports_fenced = true;
                self.runtime_reports.exhausted = true;
            }
        }
        self.debounce.mark_dirty(self.clock.now());
    }

    pub(super) fn revalidate_carrier_provenance(&mut self) {
        if self
            .intent
            .runtime_carriers
            .as_ref()
            .is_some_and(|record| self.carrier_binding().as_deref() != Ok(record.binding.as_str()))
        {
            self.protect_carrier_provenance();
        }
        if self
            .intent
            .runtime_carrier_grant
            .as_ref()
            .is_some_and(|grant| {
                self.carrier_binding().as_deref() != Ok(grant.binding.as_str())
                    || self.carrier_saved_selections() != grant.saved
                    || self.stored_carrier_flows().as_deref() != Ok(grant.flows.as_slice())
            })
        {
            self.intent.runtime_carrier_grant = None;
            self.debounce.mark_dirty(self.clock.now());
        }
    }

    pub(super) fn carrier_origins(&self, count: usize) -> Vec<RuntimeCarrierOrigin> {
        self.intent
            .runtime_carriers
            .as_ref()
            .filter(|record| record.origins.len() == count)
            .map_or_else(
                || vec![RuntimeCarrierOrigin::default(); count],
                |record| {
                    record
                        .origins
                        .iter()
                        .map(|origin| {
                            if origin.protection_revision == self.intent.runtime_protection_revision
                                && origin.invocation.as_deref().is_some_and(valid_invocation)
                            {
                                origin.clone()
                            } else {
                                RuntimeCarrierOrigin::default()
                            }
                        })
                        .collect()
                },
            )
    }

    pub(super) fn current_carrier_origin(
        &self,
        run: &RuntimeRun,
    ) -> Result<RuntimeCarrierOrigin, CommandError> {
        Ok(RuntimeCarrierOrigin {
            invocation: self.runtime_reports.active(run)?.invocation.clone(),
            protection_revision: self.intent.runtime_protection_revision,
        })
    }

    pub(super) fn record_carrier_provenance(
        &mut self,
        binding: String,
        origins: Vec<RuntimeCarrierOrigin>,
    ) {
        let record = Some(RuntimeCarrierOwnership { binding, origins });
        if self.intent.runtime_carriers != record {
            self.intent.runtime_carriers = record;
            self.debounce.mark_dirty(self.clock.now());
        }
    }

    /// Inspect known retired occurrences without transferring any ownership.
    /// The caller must establish the current run's kernel/systemd identity.
    #[doc(hidden)]
    pub fn runtime_carrier_review(
        &self,
        run: &RuntimeRun,
    ) -> Result<RuntimeCarrierReview, CommandError> {
        let active = self.runtime_reports.active(run)?;
        if active.binding != Binding::of(self) {
            return Err(unavailable("carrier recovery binding changed"));
        }
        let invocation = active
            .invocation
            .clone()
            .ok_or_else(|| unavailable("runtime incarnation has not been authenticated"))?;
        let binding = self.carrier_binding()?;
        let flows = self.stored_carrier_flows()?;
        let origins = if self
            .intent
            .runtime_carriers
            .as_ref()
            .is_some_and(|record| record.binding == binding)
        {
            self.carrier_origins(flows.len())
        } else {
            vec![RuntimeCarrierOrigin::default(); flows.len()]
        };
        let releasable = origins
            .iter()
            .enumerate()
            .filter(|(_, origin)| {
                origin
                    .invocation
                    .as_ref()
                    .is_some_and(|old| old != &invocation)
            })
            .map(|(index, _)| {
                u32::try_from(index).map_err(|_| unavailable("carrier occurrence overflow"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let unavailable = if self.receiver_operation_uncertain() {
            Some("Reconcile the uncertain receiver operation first".into())
        } else if self.intent.runtime_reports_fenced {
            Some("An intentional host flow replacement protects carriers and vacancies".into())
        } else if let Err(error) = self.runtime_report_locks() {
            Some(error.to_string())
        } else if !active.restore_observed {
            Some("Runtime startup restore has not been witnessed".into())
        } else if !active.reported {
            Some("This runtime has no accepted report; authorize a subsequent start instead".into())
        } else if releasable.is_empty() {
            Some("No recorded carriers from a retired runtime are eligible; unknown provenance remains protected".into())
        } else {
            None
        };
        let (grant_occurrences, grant_unavailable) = self.carrier_grant_scope(&flows)?;
        Ok(RuntimeCarrierReview {
            invocation,
            protection_revision: self.intent.runtime_protection_revision,
            binding,
            flows,
            origins,
            releasable,
            unavailable,
            saved: self.carrier_saved_selections(),
            grant_occurrences,
            grant_unavailable,
            configured: self
                .config
                .config
                .flows
                .iter()
                .map(crate::flows::to_spec)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| super::unavailable("cannot review configured carriers"))?,
        })
    }

    /// Explicitly return only the reviewed retired occurrences to this run.
    /// No flow bytes, saved selections, receiver mutation or vacancy fence change.
    /// Persist provenance before granting ephemeral ownership. The caller must
    /// authenticate the run, consume its review token and fence old workers.
    #[doc(hidden)]
    pub fn release_runtime_carriers(
        &mut self,
        run: &RuntimeRun,
        reviewed: &RuntimeCarrierReview,
    ) -> Result<(), CommandError> {
        // `device-lock.md` §9.2 L5: before anything else.
        self.check_local_receiver_access()?;
        let current = self.runtime_carrier_review(run)?;
        if current != *reviewed || current.unavailable.is_some() {
            return Err(unavailable(
                "carrier review changed or release is unavailable",
            ));
        }
        let generation = self
            .runtime_reports
            .generation
            .checked_add(1)
            .ok_or_else(|| unavailable("runtime accounting exhausted"))?;
        let before = self.intent.clone();
        let record = self
            .intent
            .runtime_carriers
            .as_mut()
            .ok_or_else(|| unavailable("carrier provenance unavailable"))?;
        for index in &reviewed.releasable {
            record.origins[*index as usize].invocation = Some(reviewed.invocation.clone());
        }
        self.debounce.mark_dirty(self.clock.now());
        if self.flush().is_err() {
            self.intent = before;
            self.debounce.mark_dirty(self.clock.now());
            return Err(unavailable("cannot persist carrier ownership release"));
        }
        self.runtime_reports
            .owned
            .resize(current.flows.len(), false);
        for index in &reviewed.releasable {
            self.runtime_reports.owned[*index as usize] = true;
        }
        self.runtime_reports.generation = generation;
        if let Some(active) = &mut self.runtime_reports.active {
            active.reading = None;
        }
        Ok(())
    }
}
