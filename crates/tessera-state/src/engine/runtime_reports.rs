//! Authority-side complete-report transition. These Rust-only entry points do
//! not authenticate a process. The daemon must establish the unit invocation,
//! binding, complete-report path and live sender before calling them on its
//! actor. Ordinary socket commands and diagnostic observations grant no report
//! authority.
//!
//! Live ownership is ephemeral. Durable provenance permits an explicit review
//! of retired-run carriers, never automatic adoption after restart.
use super::*;
use tessera_control_types::{FlowSpec, ReceiverSelection};

mod carriers;
mod grant;
pub use carriers::RuntimeCarrierReview;

#[derive(Debug)]
struct RunKey;

/// An authority-created run handle, bound to one engine and one binding.
/// It has no wire encoding; a peer-supplied invocation string is insufficient.
#[derive(Debug, Clone)]
pub struct RuntimeRun(Arc<RunKey>);

/// A single, coalesced readback in flight, never a subscription mutation grant.
/// The callback payload is not retained. Only a fresh receiver read supplies
/// values for the existing saved slot.
#[derive(Debug)]
pub struct RuntimeRead {
    run: RuntimeRun,
    generation: u64,
    echo: u64,
    read: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct Binding {
    config: Config,
    first: Option<DeviceModel>,
    selected: Option<String>,
    network: Option<NetworkStatus>,
}

impl Binding {
    fn of(engine: &StateEngine) -> Self {
        let mut first = engine.model.devices.first().cloned();
        if let Some(first) = &mut first {
            // Readback changes the existing saved slot; it must not change its
            // own binding or create a second route store.
            first.subscriptions.clear();
        }
        Self {
            config: engine.config.config.clone(),
            first,
            selected: engine.model.network.selected.clone(),
            network: engine.host_network.clone(),
        }
    }
}

#[derive(Debug)]
struct Active {
    run: RuntimeRun,
    invocation: Option<String>,
    binding: Binding,
    reported: bool,
    restore_observed: bool,
    echo: u64,
    dirty: bool,
    reading: Option<u64>,
    grant_claimed: bool,
}

#[derive(Debug, Default)]
pub(super) struct RuntimeReports {
    active: Option<Active>,
    /// One bit for each stored occurrence, in order. Missing bits are protected.
    owned: Vec<bool>,
    /// Includes accepted byte-identical host writes. Exhaustion closes admission.
    generation: u64,
    next_read: u64,
    exhausted: bool,
}

fn unavailable(reason: &str) -> CommandError {
    CommandError::Unsupported {
        what: format!("runtime report integration: {reason}"),
    }
}

fn conflict(reason: impl Into<String>) -> CommandError {
    invalid("runtime_report", reason)
}

impl RuntimeReports {
    fn active(&self, run: &RuntimeRun) -> Result<&Active, CommandError> {
        self.active
            .as_ref()
            .filter(|a| !self.exhausted && Arc::ptr_eq(&a.run.0, &run.0))
            .ok_or_else(|| unavailable("run is unavailable or its binding changed"))
    }

    fn protect(&mut self) {
        self.owned.clear();
        if let Some(active) = &mut self.active {
            active.reported = false;
            // A pending mirror cannot overwrite an intervening accepted host
            // write, even if its resulting content was identical.
            active.reading = None;
        }
        match self.generation.checked_add(1) {
            Some(next) => self.generation = next,
            None => {
                self.exhausted = true;
                self.active = None;
            }
        }
    }
}

impl StateEngine {
    /// Begin a run after daemon-side invocation/binding authentication. This
    /// method supplies no such proof. Refuses an already active run; replacing
    /// one requires explicit retirement. Production registration uses
    /// `begin_runtime_reports_for` with its authenticated invocation.
    ///
    /// # Errors
    /// No first device, a held local lock, an active run or exhausted accounting.
    #[doc(hidden)]
    pub fn begin_runtime_reports(&mut self) -> Result<RuntimeRun, CommandError> {
        self.begin_runtime_reports_inner(None)
    }

    /// Begin after the daemon authenticates this systemd invocation and its
    /// binding. The string itself grants no identity or process authority.
    #[doc(hidden)]
    pub fn begin_runtime_reports_for(
        &mut self,
        invocation: &str,
    ) -> Result<RuntimeRun, CommandError> {
        if !carriers::valid_invocation(invocation) {
            return Err(unavailable("invalid runtime incarnation"));
        }
        self.begin_runtime_reports_inner(Some(invocation.to_owned()))
    }

    fn begin_runtime_reports_inner(
        &mut self,
        invocation: Option<String>,
    ) -> Result<RuntimeRun, CommandError> {
        if self.runtime_reports.active.is_some() || self.runtime_reports.exhausted {
            return Err(unavailable(
                "a run is already active or accounting is exhausted",
            ));
        }
        self.runtime_report_locks()?;
        self.model
            .devices
            .first()
            .ok_or_else(|| unavailable("no local protocol device"))?;
        let run = RuntimeRun(Arc::new(RunKey));
        self.runtime_reports.protect();
        if self.runtime_reports.exhausted {
            return Err(unavailable("accounting is exhausted"));
        }
        self.runtime_reports.active = Some(Active {
            run: run.clone(),
            invocation,
            binding: Binding::of(self),
            reported: false,
            restore_observed: false,
            echo: 0,
            dirty: false,
            reading: None,
            grant_claimed: false,
        });
        Ok(run)
    }

    /// Retire only this run, leaving all stored carriers protected. This does
    /// not assert that stale ports remain usable or restore transmitter state.
    #[doc(hidden)]
    pub fn end_runtime_reports(&mut self, run: &RuntimeRun) {
        if self.runtime_reports.active(run).is_ok() {
            self.runtime_reports.protect();
            self.runtime_reports.active = None;
        }
    }

    /// True only after this binding's run has delivered an accepted complete
    /// report, including an empty startup report. Does not prove selected audio,
    /// planner freshness, sender liveness or readiness on another host.
    #[doc(hidden)]
    #[must_use]
    pub fn runtime_reports_ready(&self, run: &RuntimeRun) -> bool {
        self.runtime_reports.active(run).is_ok_and(|a| {
            a.reported
                && !self.intent.runtime_reports_fenced
                && a.binding == Binding::of(self)
                && self.runtime_report_locks().is_ok()
        })
    }

    /// Conservative initial execution gate for the complete local callback
    /// domain. Report admission remains useful alongside protected content,
    /// but the requester must not provoke negotiation for such a domain yet.
    #[doc(hidden)]
    pub fn runtime_operation_ready(&self, run: &RuntimeRun) -> Result<(), CommandError> {
        if !self.runtime_reports_ready(run) {
            return Err(unavailable(
                "registered runtime is not ready for receiver execution",
            ));
        }
        let first = &self
            .model
            .devices
            .first()
            .ok_or_else(|| unavailable("no protocol device"))?
            .key;
        if self.config.config.flows.iter().any(|f| f.device == first.0)
            || self.intent.protocol_flows.as_ref().is_some_and(|flows| {
                flows.iter().enumerate().any(|(index, f)| {
                    &f.device == first && self.runtime_reports.owned.get(index) != Some(&true)
                })
            })
        {
            return Err(unavailable(
                "protected carriers in the local callback domain require explicit qualification",
            ));
        }
        Ok(())
    }

    fn runtime_report_locks(&self) -> Result<(), CommandError> {
        for control in [ControlLock::Flows, ControlLock::Subscriptions] {
            if self.config.config.control.locked.contains(&control) {
                return Err(locked(control.as_str()));
            }
        }
        Ok(())
    }

    /// Host-write generation for a live, fully admitted runtime callback domain.
    #[doc(hidden)]
    pub fn runtime_operation_generation(&self, run: &RuntimeRun) -> Result<u64, CommandError> {
        self.runtime_operation_ready(run)?;
        Ok(self.runtime_reports.generation)
    }

    /// Whether dispatch or legacy uncertainty still fences receiver execution.
    #[doc(hidden)]
    pub fn receiver_operation_uncertain(&self) -> bool {
        self.intent.receiver_operation_uncertain || self.intent.receiver_operation.is_some()
    }

    /// Persist operation-specific context before permission to emit bytes.
    /// Failure leaves the in-memory fence closed too.
    #[doc(hidden)]
    pub fn begin_receiver_mutation(
        &mut self,
        context: tessera_control_types::ReceiverOperationContext,
    ) -> Result<(), CommandError> {
        // `device-lock.md` §9.2 L5: before anything else.
        self.check_local_receiver_access()?;
        if self.receiver_operation_uncertain() {
            return Err(unavailable(
                "an uncertain receiver operation requires reconciliation",
            ));
        }
        if context.operation.0.is_empty()
            || context.operation.0.len() > 64
            || context.revision == 0
            || context.rx_channels == 0
            || context.rx_channels > 255
            || context.affected_rx == 0
            || context.affected_rx > context.rx_channels
            || context.expected.len() != usize::from(context.rx_channels)
            || context
                .expected
                .iter()
                .map(|r| r.rx)
                .ne(1..=context.rx_channels)
            || context
                .expected
                .iter()
                .filter_map(|r| r.source.as_ref())
                .chain(context.desired.iter())
                .any(|s| s.tx_device.len() > 255 || s.tx_channel.len() > 255)
        {
            return Err(unavailable("invalid receiver operation context"));
        }
        self.intent.receiver_operation_uncertain = true;
        self.intent.receiver_operation = Some(context);
        self.debounce.mark_dirty(self.clock.now());
        self.flush()
            .map(|_| ())
            .map_err(|_| unavailable("cannot persist receiver operation fence"))
    }

    /// Clear only the matching operation after correlated complete readback.
    #[doc(hidden)]
    pub fn confirm_receiver_mutation(
        &mut self,
        operation: &tessera_control_types::ReceiverExpectation,
    ) -> Result<(), CommandError> {
        if self
            .intent
            .receiver_operation
            .as_ref()
            .is_none_or(|c| &c.operation != operation)
        {
            return Err(unavailable(
                "receiver confirmation belongs to another operation",
            ));
        }
        let before = self.intent.clone();
        self.intent.receiver_operation_uncertain = false;
        self.intent.receiver_operation = None;
        self.debounce.mark_dirty(self.clock.now());
        if self.flush().is_err() {
            self.intent = before;
            self.debounce.mark_dirty(self.clock.now());
            return Err(unavailable("cannot persist receiver confirmation"));
        }
        Ok(())
    }

    /// Recovery requires a live binding and clear locks, but never adopts
    /// restored carriers or requires runtime acquisition to have succeeded.
    #[doc(hidden)]
    pub fn runtime_recovery_context(&self, run: &RuntimeRun) -> Result<(u64, u64), CommandError> {
        let active = self.runtime_reports.active(run)?;
        if active.binding != Binding::of(self) {
            return Err(unavailable("runtime binding changed"));
        }
        Ok((self.runtime_reports.generation, active.echo))
    }

    /// Recovery may inspect a protected run, but cannot accept a baseline until
    /// a valid complete report proves that run has passed its startup restore.
    /// This does not reopen report ownership or require an accepted acquisition.
    #[doc(hidden)]
    pub fn runtime_restore_observed(&self, run: &RuntimeRun) -> Result<bool, CommandError> {
        Ok(self.runtime_reports.active(run)?.restore_observed)
    }

    /// Public carrier projection for recovery; contains no daemon credentials.
    #[doc(hidden)]
    pub fn receiver_configured_carriers(&self) -> Result<Vec<FlowSpec>, CommandError> {
        self.config
            .config
            .flows
            .iter()
            .map(|f| {
                crate::flows::to_spec(f)
                    .map_err(|_| unavailable("cannot project configured carriers"))
            })
            .collect()
    }

    /// Atomically persist the accepted saved-slot baseline and resolve exactly
    /// one uncertainty episode. Caller proves worker retirement and fresh review
    /// on the actor. No carrier content, ownership or vacancy fence is changed.
    #[doc(hidden)]
    pub fn reconcile_receiver_mutation(
        &mut self,
        operation: Option<&tessera_control_types::ReceiverExpectation>,
        selections: Vec<(Channel, Option<ReceiverSelection>)>,
    ) -> Result<(), CommandError> {
        // `device-lock.md` §9.2 L5: before anything else.
        self.check_local_receiver_access()?;
        if !self.receiver_operation_uncertain()
            || self
                .intent
                .receiver_operation
                .as_ref()
                .map(|c| &c.operation)
                != operation
        {
            return Err(unavailable(
                "receiver reconciliation belongs to another operation",
            ));
        }
        self.runtime_report_locks()?;
        let first = self
            .model
            .devices
            .first()
            .ok_or_else(|| unavailable("no protocol device"))?;
        if selections.len() != usize::from(first.rx_channels)
            || selections
                .iter()
                .map(|(c, _)| c.get())
                .ne(1..=first.rx_channels)
        {
            return Err(conflict(
                "reconciliation requires the complete ordered RX scope",
            ));
        }
        let key = first.key.clone();
        let before = self.intent.clone();
        let mut intent = before.clone();
        for (channel, source) in selections {
            let command = match source {
                Some(s) => Command::Subscribe {
                    device: key.clone(),
                    rx_channel: channel,
                    tx_device: s.tx_device,
                    tx_channel: s.tx_channel,
                },
                None => Command::Unsubscribe {
                    device: key.clone(),
                    rx_channel: channel,
                },
            };
            self.mutate(&mut intent, &command)?;
        }
        intent.receiver_operation = None;
        intent.receiver_operation_uncertain = false;
        // Persist before exposing either the changed slot or a cleared fence.
        self.intent = intent;
        self.debounce.mark_dirty(self.clock.now());
        if self.flush().is_err() {
            self.intent = before;
            self.debounce.mark_dirty(self.clock.now());
            return Err(unavailable("cannot persist receiver reconciliation"));
        }
        // A mirror read started before this accepted baseline must not later
        // overwrite it, even when its detached echo has not advanced yet.
        // Its completion is rejected by the existing per-read ticket check.
        if let Some(active) = &mut self.runtime_reports.active {
            active.reading = None;
        }
        let (model, warnings, _) = reconcile(&self.config.config, &mut self.intent, &self.env);
        let mut events = self.diff_models(&self.model, &model, &Origin::Internal);
        events.extend(runtime_changes(&before, &self.intent, &Origin::Internal));
        self.prune_runtime(&model);
        self.model = model;
        self.state_warnings = warnings;
        for event in events {
            self.bus.publish(&event);
        }
        Ok(())
    }

    pub(super) fn revalidate_runtime_binding(&mut self) {
        self.revalidate_carrier_provenance();
        if self
            .runtime_reports
            .active
            .as_ref()
            .is_some_and(|a| a.binding != Binding::of(self))
        {
            self.runtime_reports.protect();
            self.runtime_reports.active = None;
        }
    }

    pub(super) fn record_runtime_host_write(&mut self, command: &Command) {
        // Clearing a device's configuration replaces its controller flows
        // too: the same intentional whole-set write.
        if matches!(
            command,
            Command::SetProtocolFlows { .. } | Command::ClearConfiguration { .. }
        ) {
            if self.runtime_reports.active.is_some()
                || self.intent.runtime_carriers.is_some()
                || self.intent.runtime_carrier_grant.is_some()
            {
                self.intent.runtime_reports_fenced = true;
                // Even an identical setter changes durable protection. It
                // follows the same debounce contract as its stored intent.
                self.debounce.mark_dirty(self.clock.now());
            }
            self.protect_carrier_provenance();
            self.runtime_reports.protect();
        }
    }

    pub(super) fn guard_managed_intent(&self, command: &Command) -> Result<(), CommandError> {
        // Clearing a device's configuration removes its subscriptions.
        let (Command::Subscribe { device, .. }
        | Command::Unsubscribe { device, .. }
        | Command::ClearConfiguration { device, .. }) = command
        else {
            return Ok(());
        };
        if self.runtime_reports.active.as_ref().is_some_and(|a| {
            a.binding
                .first
                .as_ref()
                .is_some_and(|first| &first.key == device)
        }) {
            return Err(unavailable(
                "saved-intent writes to the active local receiver require a receiver operation",
            ));
        }
        Ok(())
    }

    /// Admit the COMPLETE raw report from an authenticated, currently live
    /// sender in `run`. The caller must perform its liveness check on the actor
    /// immediately before this call. There is no content-equality adoption.
    ///
    /// # Errors
    /// Inactive run, locks, invalid raw/aggregate content or protected collision.
    /// A refusal changes neither stored intent nor ownership. Missing protected
    /// carriers are retained, never removed to make validation succeed.
    #[doc(hidden)]
    pub fn apply_runtime_report(
        &mut self,
        run: &RuntimeRun,
        raw: Vec<FlowSpec>,
    ) -> Result<Applied, CommandError> {
        self.revalidate_runtime_binding();
        let active = self.runtime_reports.active(run)?;
        self.runtime_report_locks()?;
        let first = active
            .binding
            .first
            .as_ref()
            .ok_or_else(|| unavailable("no protocol device"))?;
        if raw.iter().any(|f| f.device != first.key) {
            return Err(conflict(
                "complete report contains a different local device",
            ));
        }
        // Validate raw input independently. Never filter it before validation,
        // even when a protected occurrence has the same id or channel.
        self.validate_runtime_flows(&raw)?;
        // Even a protected-content conflict happens after this authenticated
        // run has passed startup restore. This grants no carrier ownership.
        self.runtime_reports
            .active
            .as_mut()
            .expect("checked active run")
            .restore_observed = true;
        if self.intent.runtime_reports_fenced && !raw.is_empty() {
            return Err(unavailable(
                "host replacement protects vacancies; report acquisition is closed",
            ));
        }
        let stored = match &self.intent.protocol_flows {
            Some(flows) => flows.clone(),
            None => self
                .config
                .config
                .legacy_protocol_flows
                .iter()
                .map(crate::flows::to_spec)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| conflict(format!("protected legacy content: {e}")))?,
        };
        let mut merged = Vec::with_capacity(stored.len().saturating_add(raw.len()));
        let mut owned = Vec::with_capacity(merged.capacity());
        let stored_origins = self.carrier_origins(stored.len());
        let incoming_origin = self.current_carrier_origin(run)?;
        let mut origins = Vec::with_capacity(merged.capacity());
        let mut inserted = false;
        for (index, flow) in stored.iter().enumerate() {
            if self.runtime_reports.owned.get(index) == Some(&true) {
                if !inserted {
                    merged.extend(raw.iter().cloned());
                    owned.extend(std::iter::repeat_n(true, raw.len()));
                    origins.extend(std::iter::repeat_n(incoming_origin.clone(), raw.len()));
                    inserted = true;
                }
            } else {
                // Matching content does not transfer ownership. In particular
                // a byte-identical host write cannot be undone by an echo.
                if raw.iter().any(|incoming| incoming.id == flow.id) {
                    return Err(conflict(format!("carrier `{}` is protected", flow.id)));
                }
                merged.push(flow.clone());
                owned.push(false);
                origins.push(stored_origins[index].clone());
            }
        }
        if !inserted {
            origins.extend(std::iter::repeat_n(incoming_origin, raw.len()));
            merged.extend(raw);
            owned.resize(merged.len(), true);
        }
        self.validate_runtime_flows(&merged)?;
        let carrier_binding = self.carrier_binding()?;
        let applied = self.apply_inner_from(
            Origin::Internal,
            Command::SetProtocolFlows { flows: merged },
            Confirm::No,
            false,
        )?;
        self.record_carrier_provenance(carrier_binding, origins);
        self.runtime_reports.owned = owned;
        if let Some(active) = &mut self.runtime_reports.active {
            active.reported = true;
        }
        // A next-start grant is not historical provenance. Its first actual
        // report must persist both content and real provenance before ACK.
        if self.runtime_reports.active(run)?.grant_claimed {
            if self.flush().is_err() {
                self.intent.runtime_reports_fenced = true;
                self.protect_carrier_provenance();
                self.runtime_reports.protect();
                return Err(unavailable("cannot persist the granted run's first report"));
            }
            self.runtime_reports
                .active
                .as_mut()
                .expect("checked run")
                .grant_claimed = false;
        }
        Ok(applied)
    }

    fn validate_runtime_flows(&self, flows: &[FlowSpec]) -> Result<(), CommandError> {
        let converted = crate::flows::from_specs(flows, &self.env).map_err(flow_invalid)?;
        let mut config =
            crate::flows::effective_validation_config(&self.config.config, &self.model.devices);
        config.flows.extend(converted);
        let issues: Vec<_> = crate::validate::validate(&config, &self.env)
            .into_iter()
            .filter(Issue::is_error)
            .collect();
        if issues.is_empty() {
            Ok(())
        } else {
            Err(flow_invalid(issues))
        }
    }

    /// An authenticated Subscribe/Unsubscribe echo is only a dirty signal for
    /// the whole first local receiver. Its names and channel payload are never
    /// written. Multiple signals coalesce into at most one active read.
    ///
    /// # Errors
    /// Inactive run or exhausted accounting.
    #[doc(hidden)]
    pub fn notice_runtime_echo(&mut self, run: &RuntimeRun) -> Result<(), CommandError> {
        self.revalidate_runtime_binding();
        self.runtime_reports.active(run)?;
        let active = self
            .runtime_reports
            .active
            .as_mut()
            .expect("checked active run");
        let Some(next) = active.echo.checked_add(1) else {
            self.runtime_reports.exhausted = true;
            self.runtime_reports.protect();
            self.runtime_reports.active = None;
            return Err(unavailable("echo accounting is exhausted"));
        };
        active.echo = next;
        active.dirty = true;
        Ok(())
    }

    /// Start one read if dirty. A second echo during this read leaves another
    /// read needed; it cannot cause the earlier result to rewind saved intent.
    ///
    /// # Errors
    /// Inactive run, local lock or exhausted accounting.
    #[doc(hidden)]
    pub fn begin_runtime_read(
        &mut self,
        run: &RuntimeRun,
    ) -> Result<Option<RuntimeRead>, CommandError> {
        self.revalidate_runtime_binding();
        let active = self.runtime_reports.active(run)?;
        self.runtime_report_locks()?;
        if !active.reported {
            return Err(unavailable(
                "run has not delivered its first complete report",
            ));
        }
        if !active.dirty || active.reading.is_some() {
            return Ok(None);
        }
        let Some(read) = self.runtime_reports.next_read.checked_add(1) else {
            self.runtime_reports.exhausted = true;
            self.runtime_reports.protect();
            self.runtime_reports.active = None;
            return Err(unavailable("read accounting is exhausted"));
        };
        self.runtime_reports.next_read = read;
        let active = self
            .runtime_reports
            .active
            .as_mut()
            .expect("checked active run");
        active.reading = Some(read);
        active.dirty = false;
        Ok(Some(RuntimeRead {
            run: run.clone(),
            generation: self.runtime_reports.generation,
            echo: active.echo,
            read,
        }))
    }

    /// Finish one fresh, correlated complete local receiver read. `None`
    /// denotes a failed read: retain the slot without retry or reassertion.
    /// All channels must be present exactly once. Raw status does not enter
    /// saved intent. The daemon separately retains uncertainty/observations.
    ///
    /// # Errors
    /// Stale ticket, lock, incomplete geometry or invalid names. The complete
    /// saved-slot change is atomic; no per-channel partial commit occurs.
    #[doc(hidden)]
    pub fn finish_runtime_read(
        &mut self,
        ticket: RuntimeRead,
        selections: Option<Vec<(Channel, Option<ReceiverSelection>)>>,
    ) -> Result<Applied, CommandError> {
        self.revalidate_runtime_binding();
        let active = self.runtime_reports.active(&ticket.run)?;
        if active.reading != Some(ticket.read)
            || ticket.generation != self.runtime_reports.generation
        {
            return Err(unavailable("read ticket was superseded"));
        }
        let superseded = active.echo != ticket.echo;
        let first = active
            .binding
            .first
            .clone()
            .ok_or_else(|| unavailable("no protocol device"))?;
        self.runtime_reports
            .active
            .as_mut()
            .expect("checked active run")
            .reading = None;
        if superseded {
            return Err(unavailable("newer echo requires a fresh read"));
        }
        self.runtime_report_locks()?;
        let Some(selections) = selections else {
            return Err(unavailable("receiver read failed; saved intent retained"));
        };
        let ids: BTreeSet<_> = selections
            .iter()
            .map(|(channel, _)| channel.get())
            .collect();
        if ids.len() != selections.len()
            || ids.len() != usize::from(first.rx_channels)
            || ids.iter().copied().ne(1..=first.rx_channels)
        {
            return Err(conflict(
                "readback does not contain exactly the complete local RX scope",
            ));
        }
        let mut intent = self.intent.clone();
        for (channel, source) in selections {
            let command = match source {
                Some(source) => Command::Subscribe {
                    device: first.key.clone(),
                    rx_channel: channel,
                    tx_device: source.tx_device,
                    tx_channel: source.tx_channel,
                },
                None => Command::Unsubscribe {
                    device: first.key.clone(),
                    rx_channel: channel,
                },
            };
            self.mutate(&mut intent, &command)?;
        }
        if intent == self.intent {
            return Ok(Applied::default());
        }
        let (model, warnings, _) = reconcile(&self.config.config, &mut intent, &self.env);
        let mut events = self.diff_models(&self.model, &model, &Origin::Internal);
        events.extend(runtime_changes(&self.intent, &intent, &Origin::Internal));
        self.intent = intent;
        self.prune_runtime(&model);
        self.model = model;
        self.state_warnings = warnings;
        self.debounce.mark_dirty(self.clock.now());
        for event in &events {
            self.bus.publish(event);
        }
        Ok(Applied {
            changed: true,
            events,
            ..Applied::default()
        })
    }
}
