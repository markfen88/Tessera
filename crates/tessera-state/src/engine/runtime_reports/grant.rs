//! Explicit next-start authorization. No current-run adoption or vacancy reset.
use super::*;
use crate::persist::{RuntimeCarrierGrant, SubscriptionIntent};

impl StateEngine {
    pub(super) fn stored_carrier_flows(&self) -> Result<Vec<FlowSpec>, CommandError> {
        match &self.intent.protocol_flows {
            Some(flows) => Ok(flows.clone()),
            None => self
                .config
                .config
                .legacy_protocol_flows
                .iter()
                .map(crate::flows::to_spec)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| conflict(format!("protected legacy content: {e}"))),
        }
    }

    pub(super) fn carrier_saved_selections(&self) -> Vec<SubscriptionIntent> {
        self.model.devices.first().map_or_else(Vec::new, |first| {
            first
                .subscriptions
                .iter()
                .map(|(rx, source)| SubscriptionIntent {
                    rx_channel: *rx,
                    tx_channel: source.tx_channel.clone(),
                    tx_device: source.tx_device.clone(),
                })
                .collect()
        })
    }

    pub(super) fn carrier_grant_scope(
        &self,
        flows: &[FlowSpec],
    ) -> Result<(Vec<u32>, Option<String>), CommandError> {
        let first = &self
            .model
            .devices
            .first()
            .ok_or_else(|| unavailable("no protocol device"))?
            .key;
        let occurrences = flows
            .iter()
            .enumerate()
            .filter(|(_, f)| &f.device == first)
            .map(|(i, _)| u32::try_from(i).map_err(|_| unavailable("carrier occurrence overflow")))
            .collect::<Result<Vec<_>, _>>()?;
        let reason = if self.receiver_operation_uncertain() {
            Some("Reconcile the uncertain receiver operation first".into())
        } else if self.intent.runtime_reports_fenced {
            Some("An intentional host replacement protects carriers and empty slots".into())
        } else if let Err(error) = self.runtime_report_locks() {
            Some(error.to_string())
        } else if self.config.config.flows.iter().any(|f| f.device == first.0) {
            Some("Configured carriers in the first-device domain must remain protected".into())
        } else if occurrences.is_empty() {
            Some("No first-device stored carriers require next-start authorization".into())
        } else {
            None
        };
        Ok((occurrences, reason))
    }

    /// Record new administrative authority, preserving all content and origins.
    /// The daemon authenticates the administrator, run, review and worker fence.
    #[doc(hidden)]
    pub fn grant_runtime_carriers_to_next_run(
        &mut self,
        run: &RuntimeRun,
        reviewed: &RuntimeCarrierReview,
        daemon_epoch: &str,
        admin_uid: u32,
        id: &str,
    ) -> Result<(), CommandError> {
        // `device-lock.md` §9.2 L5: before anything else.
        self.check_local_receiver_access()?;
        let current = self.runtime_carrier_review(run)?;
        if current != *reviewed
            || current.grant_unavailable.is_some()
            || !carriers::valid_invocation(daemon_epoch)
            || id.is_empty()
            || id.len() > 64
        {
            return Err(unavailable(
                "next-start carrier review changed or is unavailable",
            ));
        }
        let before = self.intent.runtime_carrier_grant.clone();
        self.intent.runtime_carrier_grant = Some(RuntimeCarrierGrant {
            id: id.into(),
            daemon_epoch: daemon_epoch.into(),
            admin_uid,
            reviewed_runtime: current.invocation,
            protection_revision: current.protection_revision,
            binding: current.binding,
            flows: current.flows,
            saved: current.saved,
            occurrences: current.grant_occurrences,
        });
        self.debounce.mark_dirty(self.clock.now());
        if self.flush().is_err() {
            self.intent.runtime_carrier_grant = before;
            self.debounce.mark_dirty(self.clock.now());
            return Err(unavailable(
                "cannot persist next-start carrier authorization",
            ));
        }
        Ok(())
    }

    /// Consume before launcher exec. A failed baseline check discards the
    /// grant; that run starts with protected content. A save failure refuses
    /// registration. The caller must positively retire old lifetimes/workers.
    #[doc(hidden)]
    pub fn claim_runtime_carrier_grant(
        &mut self,
        run: &RuntimeRun,
        daemon_epoch: &str,
    ) -> Result<bool, CommandError> {
        let active = self.runtime_reports.active(run)?;
        let invocation = active.invocation.clone();
        let pre_exec = !active.reported && !active.restore_observed && !active.grant_claimed;
        let Some(grant) = self.intent.runtime_carrier_grant.take() else {
            return Ok(false);
        };
        let valid = pre_exec
            && invocation
                .as_ref()
                .is_some_and(|new| new != &grant.reviewed_runtime)
            && grant.daemon_epoch == daemon_epoch
            && carriers::valid_invocation(daemon_epoch)
            && grant.protection_revision == self.intent.runtime_protection_revision
            && self.carrier_binding().as_deref() == Ok(grant.binding.as_str())
            && self.stored_carrier_flows().as_deref() == Ok(grant.flows.as_slice())
            && self.carrier_saved_selections() == grant.saved
            && self
                .carrier_grant_scope(&grant.flows)
                .is_ok_and(|(scope, reason)| reason.is_none() && scope == grant.occurrences);
        self.debounce.mark_dirty(self.clock.now());
        // No ephemeral ownership exists before this durable consumption.
        if self.flush().is_err() {
            return Err(unavailable("cannot persist next-start grant consumption"));
        }
        if !valid {
            return Ok(false);
        }
        self.runtime_reports.owned = vec![false; grant.flows.len()];
        for occurrence in grant.occurrences {
            self.runtime_reports.owned[occurrence as usize] = true;
        }
        self.runtime_reports
            .active
            .as_mut()
            .expect("checked run")
            .grant_claimed = true;
        Ok(true)
    }
}
