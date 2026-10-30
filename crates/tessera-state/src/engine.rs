//! The state engine: owns the effective model, applies [`Command`]s, persists,
//! publishes events.
//!
//! It is a plain synchronous value. In `tesserad` it lives inside one task
//! that owns it (ADR 0008's "state actor") and calls [`StateEngine::apply`]
//! for each request, [`StateEngine::poll_persist`] when
//! [`StateEngine::next_persist_deadline`] passes, and [`StateEngine::flush`]
//! on shutdown. This crate does not depend on tokio (ADR 0003 amendment).
//!
//! **Blocking I/O.** [`StateEngine::start`], [`StateEngine::poll_persist`],
//! [`StateEngine::flush`] and `apply(.., Command::ReloadConfig)` read or write
//! files synchronously. Inside an async runtime, run them on a blocking thread
//! (for example `spawn_blocking`) or dedicate a plain thread to the engine;
//! never call them on an executor thread that serves other tasks.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tessera_control_types::{
    BridgeDirection as WireBridgeDirection, BridgeState, BridgeStatus, Channel, ChannelStatus,
    ClockRole, ClockSettings, ClockStatus, Command, CommandError, DeviceKey, DeviceStatus,
    Direction, Encoding, Event, Impact, ImpactToken, LatencyUs, Limits, NetworkStatus, Origin,
    PREFERRED_LEADER_BELOW, Query, Reply, SampleRate, Status, SubscriptionState,
    SubscriptionStatus,
};

use crate::bus::{EventBus, Subscription};
use crate::config::{
    BridgeConfig, BridgeDirection, ClockLock, Config, ControlLock, DeviceLock, NetworkLock,
};
use crate::env::{
    Clock, Environment, RateLimit, validate_channel_label, validate_device_name, validate_name,
};
use crate::error::{Issue, StartError, StateError};
use crate::formats::Refusal;
use crate::loader::{
    ConfigPaths, LoadedConfig, Overrides, load_from_sources_for_state, read_sources,
};
use crate::model::{
    DeviceModel, Model, SubscriptionEntry, WireDeviceId, latency_allowed, reconcile,
};
use crate::persist::{
    ClockIntent, Debouncer, DeviceIntent, NetworkIntent, StateFile, StateLoad, SubscriptionIntent,
    load_state, remove_stale_temporaries, write_atomic,
};
use crate::validate::validate_for_start;

mod access_guard;
mod runtime_reports;
pub use runtime_reports::{RuntimeCarrierReview, RuntimeRead, RuntimeRun};

/// File locations for the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Configuration file and drop-in directory.
    pub config: ConfigPaths,
    /// Explicit state file path; `None` means `<daemon.state_dir>/state.toml`
    /// from the loaded configuration.
    pub state_file: Option<PathBuf>,
}

impl Paths {
    /// Explicit locations (tests).
    #[must_use]
    pub fn new(config: ConfigPaths, state_file: impl Into<PathBuf>) -> Self {
        Self {
            config,
            state_file: Some(state_file.into()),
        }
    }

    /// `/etc/tessera/...` and the configured state directory.
    #[must_use]
    pub fn system() -> Self {
        Self {
            config: ConfigPaths::system(),
            state_file: None,
        }
    }
}

/// What start-up found.
#[derive(Debug, Clone)]
pub struct StartupReport {
    /// Configuration and state warnings.
    pub warnings: Vec<Issue>,
    /// What happened to the state file.
    pub state: StateLoad,
    /// Configuration files read, in merge order.
    pub config_files: Vec<PathBuf>,
    /// Devices that had no identity in the state file and got a new one.
    pub new_identities: Vec<DeviceKey>,
}

/// Why a device must be restarted after a reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RestartReason {
    /// `rx_channels` or `tx_channels` changed.
    ChannelCount,
    /// `network.interfaces` or `network.secondary` changed.
    Interface,
}

/// Something the daemon must do in response to a command, beyond reacting to
/// events.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Effect {
    /// A reload added a device.
    DeviceAdded(DeviceKey),
    /// A reload removed a device (its persisted state is kept).
    DeviceRemoved(DeviceKey),
    /// Only this logical device must be restarted (ADR 0008).
    DeviceRestartRequired {
        /// The device.
        device: DeviceKey,
        /// Why.
        reason: RestartReason,
    },
    /// A reload changed this non-device section of the configuration
    /// (`daemon`, `control`, `audio`, `network`, `clock`, `engine`,
    /// `discovery`, `backends`, `aes67`, `defaults`, `bridge`, `flow`); the
    /// daemon decides what to do.
    ConfigSectionChanged(&'static str),
    /// A reload changed these keys of apply class `restart` (schema paths
    /// such as `engine.rx_batch`): they take effect at the next daemon
    /// start, and the daemon should say so (never silently ignore them).
    /// Overridden keys (command line) never change on a reload.
    RestartKeysChanged(Vec<String>),
}

/// Result of a successfully handled command.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Applied {
    /// The command changed persisted state (or, for a reload, was applied).
    /// False for no-ops such as setting a value to what it already is.
    pub changed: bool,
    /// The events that were published for this command, in order.
    pub events: Vec<Event>,
    /// Follow-up work for the daemon.
    pub effects: Vec<Effect>,
    /// What the command restarted and dropped (or, for a dry run, would
    /// have): `Some` for every command that changed something, for a dry
    /// run and for a reload; `None` for a no-op.
    pub impact: Option<Impact>,
}

#[derive(Debug)]
struct Bucket {
    milli_tokens: u64,
    last: Duration,
}

#[derive(Debug)]
struct RateLimiter {
    cfg: RateLimit,
    buckets: HashMap<String, Bucket>,
}

impl RateLimiter {
    fn new(cfg: RateLimit) -> Self {
        Self {
            cfg,
            buckets: HashMap::new(),
        }
    }

    fn capacity(&self) -> u64 {
        u64::from(self.cfg.burst.max(1)) * 1000
    }

    fn allow(&mut self, peer: &str, now: Duration) -> bool {
        let cap = self.capacity();
        if !self.buckets.contains_key(peer) {
            if self.buckets.len() >= self.cfg.max_peers.max(1) {
                let oldest = self
                    .buckets
                    .iter()
                    .min_by_key(|(_, b)| b.last)
                    .map(|(k, _)| k.clone());
                if let Some(k) = oldest {
                    self.buckets.remove(&k);
                }
            }
            self.buckets.insert(
                peer.to_owned(),
                Bucket {
                    milli_tokens: cap,
                    last: now,
                },
            );
        }
        let rate = u128::from(self.cfg.per_second);
        let Some(b) = self.buckets.get_mut(peer) else {
            return true;
        };
        let elapsed = now.saturating_sub(b.last).as_nanos();
        let refill = u64::try_from(elapsed * rate / 1_000_000).unwrap_or(u64::MAX);
        b.milli_tokens = b.milli_tokens.saturating_add(refill).min(cap);
        b.last = now;
        if b.milli_tokens >= 1000 {
            b.milli_tokens -= 1000;
            true
        } else {
            false
        }
    }
}

type RuntimeSub = (SubscriptionState, Option<LatencyUs>);

fn subscription_status(
    rx_channel: Channel,
    entry: &SubscriptionEntry,
    state: SubscriptionState,
    effective_latency: Option<LatencyUs>,
) -> SubscriptionStatus {
    SubscriptionStatus::new(
        rx_channel,
        entry.tx_channel.as_str(),
        entry.tx_device.as_str(),
        state,
    )
    .with_effective_latency(effective_latency)
}

fn channel_statuses(count: u16, label: impl Fn(u16) -> String) -> Vec<ChannelStatus> {
    (1..=count)
        .filter_map(|c| Some(ChannelStatus::new(Channel::new(c)?, label(c))))
        .collect()
}

fn invalid(field: &str, reason: impl Into<String>) -> CommandError {
    CommandError::Invalid {
        field: field.to_owned(),
        reason: reason.into(),
    }
}

fn locked(field: &str) -> CommandError {
    CommandError::locked(field)
}

fn not_found(what: impl Into<String>) -> CommandError {
    CommandError::NotFound { what: what.into() }
}

/// The state actor's core. See the module documentation.
#[derive(Debug)]
pub struct StateEngine {
    env: Environment,
    paths: Paths,
    overrides: Overrides,
    state_path: PathBuf,
    state_path_derived: bool,
    clock: Arc<dyn Clock>,
    config: LoadedConfig,
    intent: StateFile,
    model: Model,
    state_warnings: Vec<Issue>,
    persist_error: Option<String>,
    /// Short-lived notices shown with the warnings until their deadline
    /// (engine clock), e.g. a controller's identify request.
    notices: Vec<(Issue, Duration)>,
    runtime_subs: BTreeMap<(String, u16), RuntimeSub>,
    clocks: Vec<ClockStatus>,
    /// Interfaces with a PTP port now (`None`: not reported; show all).
    clock_interfaces: Option<Vec<String>>,
    host_network: Option<NetworkStatus>,
    /// The latest runtime observation of each bridge, by name.
    bridge_reports: BTreeMap<String, BridgeStatus>,
    bus: EventBus,
    debounce: Debouncer,
    limiter: Option<RateLimiter>,
    /// Ownership is acquired through an admitted report path, never by matching
    /// stored content. No wire decoder can create a runtime run or read ticket.
    runtime_reports: runtime_reports::RuntimeReports,
    /// The device access lock (`device-lock.md` §9), in its own file.
    access: crate::access::AccessLocks,
    // Private default-off diagnostics. It never decides the actual command's
    // outcome, adopts candidate flows, persists data or dispatches natively.
    #[cfg(any(test, feature = "routing-shadow"))]
    pub(crate) admission_probe: Option<crate::report_admission::AcceptanceProbe>,
    // Instance-local diagnostic context revision. Never persisted, exposed on
    // the wire or usable as a lifetime/grant. Exhaustion stays unavailable.
    #[cfg(any(test, feature = "routing-shadow"))]
    routing_query_revision: Option<u64>,
}

impl StateEngine {
    /// Load configuration and state and build the model.
    ///
    /// Sequence: load and validate the configuration (fatal on error), check
    /// it against this host (missing or wireless interfaces are fatal), load
    /// the state file (a corrupt one is moved aside and reported), apply the
    /// ADR 0008 precedence, generate missing device identities, and write the
    /// state file straight away if anything had to be created or repaired
    /// (a failed write is a warning and is retried by the debouncer).
    ///
    /// # Errors
    /// [`StartError`].
    pub fn start(
        env: Environment,
        paths: Paths,
        clock: Arc<dyn Clock>,
    ) -> Result<(Self, StartupReport), StartError> {
        Self::start_with_overrides(env, paths, Overrides::new(), clock)
    }

    /// [`start`](Self::start) with values from the command line that
    /// override every configuration file ([`Overrides`]). Reloads apply the
    /// same overrides again, so they never change those keys.
    ///
    /// # Errors
    /// [`StartError`].
    pub fn start_with_overrides(
        env: Environment,
        paths: Paths,
        overrides: Overrides,
        clock: Arc<dyn Clock>,
    ) -> Result<(Self, StartupReport), StartError> {
        let sources = read_sources(&paths.config).map_err(StartError::Config)?;
        // Resolve state_dir without consulting legacy controller intent. Read
        // sources once so a concurrent file edit cannot split the two passes.
        let mut loaded = load_from_sources_for_state(&sources, &env, &overrides, true)
            .map_err(StartError::Config)?;
        let host: Vec<Issue> = validate_for_start(&loaded.config, &env)
            .into_iter()
            .filter(Issue::is_error)
            .collect();
        if !host.is_empty() {
            return Err(StartError::Host(host));
        }
        let state_path_derived = paths.state_file.is_none();
        let state_path = paths
            .state_file
            .clone()
            .unwrap_or_else(|| loaded.config.daemon.state_dir.join("state.toml"));
        // No writer can be active yet: drop leftovers of an interrupted write.
        remove_stale_temporaries(&state_path);
        let (mut intent, load) = load_state(&state_path).map_err(StartError::State)?;
        loaded = load_from_sources_for_state(
            &sources,
            &env,
            &overrides,
            intent.protocol_flows.is_some(),
        )
        .map_err(StartError::Config)?;

        let new_identities: Vec<DeviceKey> = loaded
            .config
            .devices
            .iter()
            .filter(|d| {
                intent
                    .device
                    .get(&d.id)
                    .and_then(|s| s.wire_device_id.as_deref())
                    .and_then(WireDeviceId::from_hex)
                    .is_none()
            })
            .map(|d| DeviceKey(d.id.clone()))
            .collect();

        let (model, warnings, changed) = reconcile(&loaded.config, &mut intent, &env);
        let access_path = state_path.with_file_name(crate::access::FILE_NAME);
        remove_stale_temporaries(&access_path);
        let access = crate::access::AccessLocks::load(
            access_path,
            loaded.config.devices.iter().map(|d| d.id.as_str()),
            env.pin_cost,
        );
        let mut engine = Self {
            debounce: Debouncer::new(env.persist_delay),
            limiter: env.rate_limit.map(RateLimiter::new),
            env,
            paths,
            overrides,
            state_path,
            state_path_derived,
            clock,
            config: loaded,
            intent,
            model,
            state_warnings: warnings,
            persist_error: None,
            notices: Vec::new(),
            runtime_subs: BTreeMap::new(),
            clocks: Vec::new(),
            clock_interfaces: None,
            host_network: None,
            bridge_reports: BTreeMap::new(),
            bus: EventBus::new(),
            runtime_reports: runtime_reports::RuntimeReports::default(),
            access,
            #[cfg(any(test, feature = "routing-shadow"))]
            admission_probe: None,
            #[cfg(any(test, feature = "routing-shadow"))]
            routing_query_revision: Some(1),
        };
        let needs_write = changed
            || !matches!(
                load,
                StateLoad::Loaded {
                    migrated_from: None
                }
            );
        if needs_write {
            let now = engine.clock.now();
            if let Err(e) = engine.persist_now() {
                engine.persist_error = Some(e.to_string());
                engine.debounce.mark_dirty(now);
            }
        }
        let report = StartupReport {
            warnings: engine.warnings(),
            state: load,
            config_files: engine.config.files.clone(),
            new_identities,
        };
        Ok((engine, report))
    }

    /// Internal, feature-gated diagnostics; no routing authority is opened.
    /// A daemon instance label is an identity namespace, not a process proof.
    #[doc(hidden)]
    #[cfg(any(test, feature = "routing-shadow"))]
    pub fn enable_routing_shadow(&mut self, instance: u128) -> bool {
        if instance == 0 || self.admission_probe.is_some() {
            return false;
        }
        let Some(probe) = crate::report_admission::AcceptanceProbe::new(
            &self.config.config,
            &self.model.devices,
            &self.intent,
            instance,
        ) else {
            return false;
        };
        self.admission_probe = Some(probe);
        true
    }

    /// Observe an unattributed protocol proposal using current engine intent.
    /// No holder or source claim is inferred from connector liveness or labels.
    #[doc(hidden)]
    #[cfg(any(test, feature = "routing-shadow"))]
    pub fn observe_routing_proposal(&mut self, snapshot: &[tessera_control_types::FlowSpec]) {
        if let Some(probe) = &mut self.admission_probe {
            probe.observe_unverified(snapshot, &self.intent);
        }
    }

    /// Private fixture premises only. No listener or request decoder calls this.
    #[doc(hidden)]
    #[cfg(feature = "routing-shadow-test-hooks")]
    pub fn routing_shadow_test_event(
        &mut self,
        event: crate::routing_shadow_test_hooks::TestEvent,
    ) -> Result<
        crate::routing_shadow_test_hooks::TestReply,
        crate::routing_shadow_test_hooks::TestError,
    > {
        use crate::routing_shadow_test_hooks::TestError;
        self.admission_probe
            .as_mut()
            .ok_or(TestError::Unavailable)?
            .test_event(
                event,
                &self.config.config,
                &self.model.devices,
                &self.intent,
                &self.env,
            )
    }

    // ---- read access ----------------------------------------------------

    /// The effective model.
    #[must_use]
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Instance-local revision for private launch-query diagnostics only.
    /// Compare only within this engine instance; this is no native revision,
    /// process identity, registration, lifetime or permission.
    #[cfg(any(test, feature = "routing-shadow"))]
    #[must_use]
    pub fn routing_query_revision(&self) -> Option<u64> {
        self.routing_query_revision
    }

    /// Borrow the same host observation used by Status's primary/interface
    /// fields. Stored selection/locks are read separately from the model.
    #[cfg(any(test, feature = "routing-shadow"))]
    #[must_use]
    pub fn routing_query_network(&self) -> Option<&NetworkStatus> {
        self.host_network.as_ref()
    }

    #[cfg(any(test, feature = "routing-shadow"))]
    fn advance_routing_query_revision(&mut self) {
        self.routing_query_revision = self.routing_query_revision.and_then(|r| r.checked_add(1));
    }

    // Compare only the first device's launch context and the effective primary
    // binding. Periodic equal observations, unrelated interfaces, flow reports
    // and unchanged reloads must not consume a one-time launch observation.
    // This is broader than just interface/IP: identity, geometry, tuning and
    // applicable locks also affect the context served by the two queries.
    #[cfg(any(test, feature = "routing-shadow"))]
    fn routing_query_context_eq(
        &self,
        config: &Config,
        model: &Model,
        network: Option<&NetworkStatus>,
    ) -> bool {
        let same_device = match (self.model.devices.first(), model.devices.first()) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                (
                    &a.key,
                    a.wire_device_id,
                    &a.name,
                    a.sample_rate,
                    a.encoding,
                    a.latency_us,
                    a.rx_channels,
                    a.tx_channels,
                    &a.locked,
                ) == (
                    &b.key,
                    b.wire_device_id,
                    &b.name,
                    b.sample_rate,
                    b.encoding,
                    b.latency_us,
                    b.rx_channels,
                    b.tx_channels,
                    &b.locked,
                )
            }
            _ => false,
        };
        if !same_device
            || self.model.network != model.network
            || self.config.config.network.interfaces.first() != config.network.interfaces.first()
            || self.config.config.control.locked != config.control.locked
            || self.host_network.as_ref().map(|n| &n.primary) != network.map(|n| &n.primary)
        {
            return false;
        }
        let old = self.host_network.as_ref().into_iter().flat_map(|n| {
            n.interfaces
                .iter()
                .filter(|i| Some(i.name.as_str()) == n.primary.as_deref())
                .map(|i| (&i.name, &i.mac, &i.ipv4, i.wireless, i.carrier))
        });
        let new = network.into_iter().flat_map(|n| {
            n.interfaces
                .iter()
                .filter(|i| Some(i.name.as_str()) == n.primary.as_deref())
                .map(|i| (&i.name, &i.mac, &i.ipv4, i.wireless, i.carrier))
        });
        old.eq(new)
    }

    /// The loaded configuration (with provenance).
    #[must_use]
    pub fn config(&self) -> &LoadedConfig {
        &self.config
    }

    /// The persisted intent as it will be written.
    #[must_use]
    pub fn intent(&self) -> &StateFile {
        &self.intent
    }

    /// Where the state file is written.
    #[must_use]
    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    /// Show `message` (under `field`) with the warnings for `for_`, replacing
    /// an earlier notice of the same field. Not persisted.
    pub fn report_notice(&mut self, field: &str, message: String, for_: Duration) {
        let now = self.clock.now();
        self.notices
            .retain(|(i, until)| *until > now && i.path != field);
        self.notices
            .push((Issue::warning(field, message), now.saturating_add(for_)));
    }

    /// Current warnings: configuration, state repairs, and a failed
    /// state-file write if any.
    #[must_use]
    pub fn warnings(&self) -> Vec<Issue> {
        let mut w = self.config.warnings.clone();
        w.extend(self.state_warnings.iter().cloned());
        let now = self.clock.now();
        w.extend(
            self.notices
                .iter()
                .filter(|(_, until)| *until > now)
                .map(|(i, _)| i.clone()),
        );
        if let Some(e) = &self.persist_error {
            w.push(Issue::warning(
                "state",
                format!("cannot write the state file, will retry: {e}"),
            ));
        }
        w.extend(self.access.warning());
        // A bridge pins its card's rate but the effective device rate (a
        // value stored at run time) differs: it cannot run until one of the
        // two changes. Validation can only see the configured rate.
        for (i, b) in self.config.config.bridges.iter().enumerate() {
            let (Some(r), true) = (b.sample_rate, b.enabled) else {
                continue;
            };
            if let Some(d) = self.model.device(&b.device).filter(|d| d.sample_rate != r) {
                w.push(Issue::warning(
                    format!("bridge[{i}].sample_rate"),
                    format!(
                        "bridge `{}` runs its card at {r} Hz but device `{}` runs at {} Hz (set at run time): the bridge cannot run until they agree",
                        b.name, b.device, d.sample_rate
                    ),
                ));
            }
        }
        w
    }

    /// Subscribe to events with a bounded queue of `capacity` events. See
    /// [`crate::bus`] for the slow-subscriber behaviour.
    #[must_use]
    pub fn subscribe_events(&self, capacity: usize) -> Subscription {
        self.bus.subscribe(capacity)
    }

    /// A handle for publishing or subscribing from elsewhere.
    #[must_use]
    pub fn event_bus(&self) -> EventBus {
        self.bus.clone()
    }

    /// Answer a read-only request. `Query::Metrics` is not owned by this
    /// crate and returns `Unsupported`.
    ///
    /// # Errors
    /// [`CommandError::NotFound`] for an unknown device,
    /// [`CommandError::Unsupported`] for unknown or foreign queries.
    pub fn query(&self, q: &Query) -> Result<Reply, CommandError> {
        match q {
            Query::Status => Ok(Reply::Status(self.status())),
            Query::Device { device } => self.device_status(device).map(Reply::Device),
            Query::EffectiveConfig => self
                .effective_config()
                .map(Reply::EffectiveConfig)
                .map_err(|message| CommandError::Internal { message }),
            Query::Bridges => Ok(Reply::Bridges(self.bridges())),
            Query::Metrics => Err(CommandError::Unsupported {
                what: "metrics are not provided by the state engine".into(),
            }),
            _ => Err(CommandError::Unsupported {
                what: "query".into(),
            }),
        }
    }

    /// Snapshot of everything.
    #[must_use]
    pub fn status(&self) -> Status {
        let devices = self
            .model
            .devices
            .iter()
            .map(|d| self.device_status_of(d))
            .collect();
        let warnings = self.warnings().iter().map(ToString::to_string).collect();
        let s = Status::new(tessera_control_types::BUILD_VERSION)
            .with_devices(devices)
            .with_clocks(self.current_clocks())
            .with_clock_settings(
                ClockSettings::new(self.model.clock.role, self.model.clock.priority).with_locked(
                    self.model
                        .clock
                        .locked
                        .iter()
                        .map(|l| l.as_str().to_owned())
                        .collect(),
                ),
            )
            .with_warnings(warnings)
            .with_bridges(self.bridges())
            .with_limits(Limits::new(self.config.config.control.max_frame_bytes));
        match self.network_status() {
            Some(n) => s.with_network(n),
            None => s,
        }
    }

    /// The configuration in effect as TOML text: the merged configuration
    /// files with the values stored at run time (names, rates, latencies,
    /// labels, clock role and priority) applied. Subscriptions are not part
    /// of the configuration schema; see [`status`](Self::status).
    ///
    /// # Errors
    /// The serialiser's message.
    pub fn effective_config(&self) -> Result<String, String> {
        let mut cfg = self.config.config.clone();
        for d in &mut cfg.devices {
            if let Some(m) = self.model.device(&d.id) {
                d.name.clone_from(&m.name);
                d.sample_rate = m.sample_rate;
                d.encoding = m.encoding;
                d.latency_us = m.latency_us;
                d.rx_channels = m.rx_channels;
                d.tx_channels = m.tx_channels;
                d.rx_channel_labels.clone_from(&m.rx_labels);
                d.tx_channel_labels.clone_from(&m.tx_labels);
            }
        }
        cfg.flows.clone_from(&self.model.flows);
        cfg.clock.role = self.model.clock.role;
        cfg.clock.priority = self.model.clock.priority;
        if let Some(p) = &self.model.network.selected {
            cfg.network.interfaces.retain(|n| n != p);
            cfg.network.interfaces.insert(0, p.clone());
        }
        cfg.to_toml()
    }

    /// Configuration with the effective flow set, for the runtime reconciler.
    /// Device and clock runtime values remain available through `model()`.
    #[must_use]
    pub fn runtime_config(&self) -> Config {
        let mut config = self.config.config.clone();
        config.flows.clone_from(&self.model.flows);
        config
    }

    /// Snapshot of one device.
    ///
    /// # Errors
    /// [`CommandError::NotFound`].
    pub fn device_status(&self, key: &DeviceKey) -> Result<DeviceStatus, CommandError> {
        let d = self
            .model
            .device(&key.0)
            .ok_or_else(|| not_found(format!("device `{}`", key.0)))?;
        Ok(self.device_status_of(d))
    }

    fn device_status_of(&self, d: &DeviceModel) -> DeviceStatus {
        let subscriptions = d
            .subscriptions
            .iter()
            .filter_map(|(ch, entry)| {
                let (state, eff) = self
                    .runtime_subs
                    .get(&(d.key.0.clone(), *ch))
                    .cloned()
                    .unwrap_or((SubscriptionState::InProgress, None));
                Some(subscription_status(Channel::new(*ch)?, entry, state, eff))
            })
            .collect();
        let mut status = DeviceStatus::new(
            d.key.clone(),
            d.name.as_str(),
            SampleRate(d.sample_rate),
            Encoding(d.encoding),
            LatencyUs(d.latency_us),
        )
        .with_rx_channels(channel_statuses(d.rx_channels, |c| d.rx_label(c)))
        .with_tx_channels(channel_statuses(d.tx_channels, |c| d.tx_label(c)))
        .with_subscriptions(subscriptions)
        .with_locked(d.locked.iter().map(|l| l.as_str().to_owned()).collect())
        .with_formats(crate::formats::options(&self.config.config, &self.env, d))
        .with_access_locked(self.access.is_locked(&d.key.0));
        if !self.config.config.control.locked.is_empty() {
            status.routing_locks = Some(
                self.config
                    .config
                    .control
                    .locked
                    .iter()
                    .map(|lock| lock.as_str().to_owned())
                    .collect(),
            );
        }
        use tessera_control_types::{ReceiverCarrier, ReceiverIntent, ReceiverRoute};
        let configured: BTreeSet<_> = self
            .config
            .config
            .flows
            .iter()
            .map(|flow| &flow.id)
            .collect();
        let mut carriers = BTreeMap::new();
        for flow in self
            .model
            .flows
            .iter()
            .filter(|flow| flow.device == d.key.0 && flow.direction == Direction::Rx)
        {
            let carrier = if configured.contains(&flow.id) {
                ReceiverCarrier::ConfigurationFlow {
                    id: flow.id.clone(),
                }
            } else {
                ReceiverCarrier::ControllerFlow {
                    id: flow.id.clone(),
                }
            };
            if flow.channels.is_empty() {
                for channel in 1..=d.rx_channels {
                    carriers.entry(channel).or_insert_with(|| carrier.clone());
                }
            } else {
                for &channel in &flow.channels {
                    carriers.entry(channel).or_insert_with(|| carrier.clone());
                }
            }
        }
        for channel in &mut status.rx_channels {
            let carrier = carriers
                .remove(&channel.channel.get())
                .unwrap_or(ReceiverCarrier::None);
            let mut route = ReceiverRoute::new(carrier);
            route.intent = d
                .subscriptions
                .get(&channel.channel.get())
                .map(|intent| ReceiverIntent::new(&intent.tx_device, &intent.tx_channel));
            route.media = self
                .runtime_subs
                .get(&(d.key.0.clone(), channel.channel.get()))
                .map(|(state, _)| state.clone());
            if route.intent.is_some()
                || !matches!(route.carrier, ReceiverCarrier::None)
                || route.media.is_some()
            {
                channel.routing = Some(route);
            }
        }
        status
    }

    // ---- runtime observations (not commands, never persisted) ------------

    /// Record the live state of a subscription (receiving, refused, …) as
    /// observed by the control and media layers. This is an observation, not
    /// operator intent: it is not persisted. Publishes
    /// `Event::SubscriptionChanged` if the state differs from the last one.
    ///
    /// # Errors
    /// [`CommandError::NotFound`] if there is no active subscription on that
    /// channel.
    pub fn report_subscription_state(
        &mut self,
        device: &DeviceKey,
        rx_channel: Channel,
        state: SubscriptionState,
        effective_latency: Option<LatencyUs>,
    ) -> Result<(), CommandError> {
        let ch = rx_channel.get();
        let entry = self
            .model
            .device(&device.0)
            .ok_or_else(|| not_found(format!("device `{}`", device.0)))?
            .subscriptions
            .get(&ch)
            .cloned()
            .ok_or_else(|| not_found(format!("subscription on rx channel {ch}")))?;
        let new = (state, effective_latency);
        let key = (device.0.clone(), ch);
        let old = self
            .runtime_subs
            .get(&key)
            .cloned()
            .unwrap_or((SubscriptionState::InProgress, None));
        if old == new {
            return Ok(());
        }
        let status = subscription_status(rx_channel, &entry, new.0.clone(), new.1);
        self.runtime_subs.insert(key, new);
        self.bus.publish(&Event::SubscriptionChanged {
            device: device.clone(),
            status: status.clone(),
        });
        self.bus.publish(&Event::SubscriptionChangedWithOrigin {
            device: device.clone(),
            status,
            origin: Origin::Internal,
        });
        Ok(())
    }

    /// Apply one sweep of subscription observations made by the media layer.
    ///
    /// Every `observed` state is recorded as by
    /// [`report_subscription_state`](Self::report_subscription_state), keeping
    /// the effective latency already on record (the sweep knows none); an
    /// entry for a subscription that does not exist is ignored. A subscription
    /// that was `Receiving` or `NoMedia` and is **not** in `observed` any more
    /// (its flow was stopped or rebuilt) goes back to `InProgress`. A state
    /// set by another source (`Unresolved`, `FlowRefused`, the mismatches) is
    /// left alone while no flow carries the channel; a channel a flow carries
    /// gets the observed state, which replaces it. Sweeps are idempotent: a
    /// lost one is repaired by the next, a repeated one publishes nothing.
    ///
    /// TODO(control plane): once flow control reports `Unresolved`,
    /// `FlowRefused` and the mismatches, decide their precedence over what the
    /// media layer observes (today the media layer wins on a carried channel).
    pub fn report_subscription_sweep(
        &mut self,
        observed: &[(DeviceKey, Channel, SubscriptionState)],
    ) {
        let seen: BTreeSet<(&str, u16)> = observed
            .iter()
            .map(|(d, c, _)| (d.0.as_str(), c.get()))
            .collect();
        let stale: Vec<(DeviceKey, Channel)> = self
            .runtime_subs
            .iter()
            .filter(|(_, (s, _))| {
                matches!(s, SubscriptionState::Receiving | SubscriptionState::NoMedia)
            })
            .filter(|((d, c), _)| !seen.contains(&(d.as_str(), *c)))
            .filter_map(|((d, c), _)| Some((DeviceKey(d.clone()), Channel::new(*c)?)))
            .collect();
        for (d, c, s) in observed {
            let latency = self.observed_latency(&d.0, c.get());
            // `NotFound`: the subscription was removed since the sample.
            let _ = self.report_subscription_state(d, *c, s.clone(), latency);
        }
        for (d, c) in stale {
            let latency = self.observed_latency(&d.0, c.get());
            let _ = self.report_subscription_state(&d, c, SubscriptionState::InProgress, latency);
        }
    }

    /// The effective latency on record for a subscription, if any.
    fn observed_latency(&self, device: &str, ch: u16) -> Option<LatencyUs> {
        self.runtime_subs
            .get(&(device.to_owned(), ch))
            .and_then(|(_, l)| *l)
    }

    /// Record the clock status of an interface (one entry per `interface`).
    /// Publishes `Event::ClockStateChanged` if it changed. Not persisted.
    pub fn report_clock_status(&mut self, status: ClockStatus) {
        match self
            .clocks
            .iter_mut()
            .find(|c| c.interface == status.interface)
        {
            Some(slot) if *slot == status => return,
            Some(slot) => *slot = status.clone(),
            None => self.clocks.push(status.clone()),
        }
        self.bus.publish(&Event::ClockStateChanged { status });
    }

    /// The interfaces that run a PTP port now. Clock rows of other
    /// interfaces (a previous primary) are kept but left out of the status, so
    /// a late report from a stopped port cannot bring one back.
    pub fn set_clock_interfaces(&mut self, interfaces: Vec<String>) {
        self.clocks.retain(|c| interfaces.contains(&c.interface));
        self.clock_interfaces = Some(interfaces);
    }

    fn current_clocks(&self) -> Vec<ClockStatus> {
        self.clocks
            .iter()
            .filter(|c| {
                self.clock_interfaces
                    .as_ref()
                    .is_none_or(|v| v.contains(&c.interface))
            })
            .cloned()
            .collect()
    }

    /// Record what the daemon found on the host: the primary interface in
    /// effect, how it was chosen and the candidates (ADR 0012). Not persisted
    /// and not an event; `status` reports the latest one, with the stored
    /// selection and the lock taken from the model.
    pub fn report_network_status(&mut self, status: NetworkStatus) {
        #[cfg(any(test, feature = "routing-shadow"))]
        if !self.routing_query_context_eq(&self.config.config, &self.model, Some(&status)) {
            self.advance_routing_query_revision();
        }
        self.host_network = Some(status);
        self.revalidate_runtime_binding();
    }

    /// Record what the daemon's bridges are doing: one row per bridge it
    /// runs (state, gauges, counters, last error). Replaces the previous
    /// report. Not persisted and not an event; [`bridges`](Self::bridges)
    /// shows the latest one for every bridge still in the configuration.
    pub fn report_bridge_status(&mut self, rows: Vec<BridgeStatus>) {
        self.bridge_reports = rows.into_iter().map(|r| (r.name.clone(), r)).collect();
    }

    /// One row per configured `[[bridge]]`, in configuration order: the
    /// daemon's latest report for it if it matches the entry (same device,
    /// card and direction), otherwise the configured values with state
    /// `Disabled` or `Starting`.
    #[must_use]
    pub fn bridges(&self) -> Vec<BridgeStatus> {
        self.config
            .config
            .bridges
            .iter()
            .map(|b| {
                let configured = self.configured_bridge(b);
                match self.bridge_reports.get(&b.name) {
                    Some(_) if !b.enabled => configured,
                    // A bridge the daemon could not even plan (no card
                    // details): the configured row with its state and error.
                    Some(r) if r.card.is_empty() => {
                        let mut c = configured;
                        c.state = r.state;
                        c.last_error.clone_from(&r.last_error);
                        c
                    }
                    Some(r)
                        if r.device == configured.device
                            && r.card == configured.card
                            && r.direction == configured.direction =>
                    {
                        r.clone()
                    }
                    _ => configured,
                }
            })
            .collect()
    }

    fn configured_bridge(&self, b: &BridgeConfig) -> BridgeStatus {
        let direction = match b.direction {
            BridgeDirection::Playback => WireBridgeDirection::Playback,
            BridgeDirection::Capture => WireBridgeDirection::Capture,
        };
        let state = if b.enabled {
            BridgeState::Starting
        } else {
            BridgeState::Disabled
        };
        let mut s = BridgeStatus::new(
            b.name.as_str(),
            DeviceKey(b.device.clone()),
            b.alsa_pcm.as_str(),
            direction,
            state,
        );
        let available = self.model.device(&b.device).map_or(0, |d| match direction {
            WireBridgeDirection::Capture => d.tx_channels,
            _ => d.rx_channels,
        });
        s.first_channel = b.first_channel;
        s.channels = b.channel_count(available);
        s.time_constant_s = b.time_constant_s;
        s.max_correction_ppm = b.max_correction_ppm;
        s
    }

    /// The network part of [`status`](Self::status): `None` until the daemon
    /// has reported the host.
    /// Current host-network observation with effective selection and locks.
    /// It is an observation, not an atomic network configuration revision.
    #[must_use]
    pub fn network_status(&self) -> Option<NetworkStatus> {
        let mut n = self.host_network.clone()?;
        n.selected.clone_from(&self.intent.network.primary);
        n.locked = self.model.network.locked;
        n.allow_wireless = self.config.config.network.allow_wireless;
        Some(n)
    }

    // ---- commands --------------------------------------------------------

    /// Validate and apply one command.
    ///
    /// The order is: rate limit (protocol peers only) -> look up targets ->
    /// check locks -> validate values -> compute the new state on a copy ->
    /// commit -> mark dirty for the debounced write -> publish events. A
    /// command that fails leaves the engine exactly as it was. Setting a value
    /// to what it already is succeeds with `changed == false`, no events and
    /// no write. Permission checks (uid, group) are the control socket's
    /// job; this method trusts `origin`.
    ///
    /// **Confirmation.** A change whose computed [`Impact`] restarts
    /// something (scope `Device` or `Daemon`: rate, encoding, latency,
    /// channel counts, primary interface) is refused for `Origin::Cli`,
    /// `Origin::Api` and any origin added later with
    /// [`CommandError::ConfirmationRequired`] carrying the impact, and
    /// nothing changes; send it through
    /// [`apply_confirmed`](Self::apply_confirmed) once the user agreed.
    /// Locks, invalid values and unknown targets are reported before the
    /// confirmation question. `ReloadConfig` is gated the same way when the
    /// reload would restart a device (geometry, interfaces, `[[flow]]` or
    /// `[[bridge]]` entries, MTU, Wi-Fi permission, a removed device; scope
    /// `Device`); changed keys of apply class `restart` are only listed in
    /// `Impact::restart_keys` (they restart nothing now). Not gated: `Origin::Protocol` (a network controller
    /// has its own confirmation and cannot answer ours), `Origin::Config`
    /// (`SIGHUP`: the administrator edited the files) and
    /// `Origin::Internal`. A command that changes nothing has scope `None`
    /// and is never refused for this.
    /// `SetDeviceChannels { dry_run: true }` returns the impact in
    /// [`Applied::impact`] and changes nothing.
    ///
    /// # Errors
    /// The [`CommandError`] variants as documented on each command in
    /// `tessera-control-types`; unknown commands give `Unsupported`.
    pub fn apply(&mut self, origin: Origin, cmd: Command) -> Result<Applied, CommandError> {
        self.apply_inner(origin, cmd, Confirm::No)
    }

    /// [`apply`](Self::apply) for a command the user has confirmed without
    /// a digest (`RequestBody::ConfirmedCommand { expect: None }`, a script
    /// passing `--yes`): never refused with `ConfirmationRequired`.
    ///
    /// # Errors
    /// As [`apply`](Self::apply).
    pub fn apply_confirmed(
        &mut self,
        origin: Origin,
        cmd: Command,
    ) -> Result<Applied, CommandError> {
        self.apply_inner(origin, cmd, Confirm::Blind)
    }

    /// [`apply`](Self::apply) for a command confirmed after seeing the
    /// impact with token `expect` (`RequestBody::ConfirmedCommand {
    /// expect: Some(..) }`). If the impact computed now has another token
    /// (the files changed, a subscription was added in between) and still
    /// needs confirmation, nothing is applied and the answer is
    /// `ConfirmationRequired` with the new impact.
    ///
    /// # Errors
    /// As [`apply`](Self::apply).
    pub fn apply_expecting(
        &mut self,
        origin: Origin,
        cmd: Command,
        expect: &ImpactToken,
    ) -> Result<Applied, CommandError> {
        self.apply_inner(origin, cmd, Confirm::Expecting(expect.clone()))
    }

    /// What `cmd` would do, without changing anything (the dry run of any
    /// command, `ReloadConfig` included). A command that would change
    /// nothing has an impact of scope `None`.
    ///
    /// # Errors
    /// As [`apply`](Self::apply) (validation, locks), except
    /// `ConfirmationRequired` and rate limiting.
    pub fn impact(&self, cmd: &Command) -> Result<Impact, CommandError> {
        if self.access.any_locked().is_some() {
            self.access_refusal(cmd)?;
        }
        if access_guard::is_access_command(cmd) {
            return Ok(crate::impact::no_impact(cmd));
        }
        if matches!(cmd, Command::ReloadConfig) {
            return Ok(self.prepare_reload()?.impact);
        }
        self.guard_managed_intent(cmd)?;
        let mut intent = self.intent.clone();
        if !self.mutate(&mut intent, cmd)? {
            return Ok(crate::impact::no_impact(cmd));
        }
        let (model, _, _) = reconcile(&self.config.config, &mut intent, &self.env);
        Ok(crate::impact::impact_of(
            cmd,
            &self.model,
            &model,
            &self.intent,
            &intent,
            &self.config.config,
        ))
    }

    fn apply_inner(
        &mut self,
        origin: Origin,
        cmd: Command,
        confirm: Confirm,
    ) -> Result<Applied, CommandError> {
        self.apply_inner_from(origin, cmd, confirm, true)
    }

    fn apply_inner_from(
        &mut self,
        origin: Origin,
        cmd: Command,
        confirm: Confirm,
        host_write: bool,
    ) -> Result<Applied, CommandError> {
        // Every surface that can issue commands is limited per principal:
        // network peers, local CLI users (by uid) and API principals. Config
        // reloads and the daemon's own commands are not.
        let principal = match &origin {
            Origin::Protocol { peer } => Some(format!("net:{peer}")),
            Origin::Cli { uid } => Some(format!("cli:{uid}")),
            Origin::Api { principal } => Some(format!("api:{principal}")),
            _ => None,
        };
        if let Some(principal) = principal {
            let now = self.clock.now();
            if let Some(l) = &mut self.limiter {
                if !l.allow(&principal, now) {
                    return Err(CommandError::RateLimited);
                }
            }
        }
        // The device access lock comes before any other validation
        // (`device-lock.md` §9.2 L5).
        self.access_guard(&origin, &cmd)?;
        if access_guard::is_access_command(&cmd) {
            return self.apply_access(origin, &cmd);
        }
        let gated = !matches!(
            origin,
            Origin::Protocol { .. } | Origin::Config | Origin::Internal
        );
        let gate = if gated { Some(&confirm) } else { None };
        if matches!(cmd, Command::ReloadConfig) {
            return self.reload(gate);
        }

        let dry_run = matches!(cmd, Command::SetDeviceChannels { dry_run: true, .. });
        self.guard_managed_intent(&cmd)?;
        let mut intent = self.intent.clone();
        let changed = match self.mutate(&mut intent, &cmd) {
            Ok(changed) => changed,
            Err(error) => {
                if !dry_run {
                    if let CommandError::Locked { field, .. } = &error {
                        let (field, attempted) = command_field(&cmd, field);
                        self.bus.publish(&Event::LockedValueRefused {
                            field,
                            origin,
                            attempted,
                        });
                    }
                }
                return Err(error);
            }
        };
        if !changed {
            if host_write && !dry_run {
                self.record_runtime_host_write(&cmd);
            }
            #[cfg(any(test, feature = "routing-shadow"))]
            if !dry_run {
                if let Some(probe) = &mut self.admission_probe {
                    probe.accepted(
                        &origin,
                        &cmd,
                        false,
                        &self.config.config,
                        &self.model.devices,
                        &intent,
                    );
                }
            }
            return Ok(Applied {
                impact: dry_run.then(|| crate::impact::no_impact(&cmd)),
                ..Applied::default()
            });
        }
        let (model, warnings, _) = reconcile(&self.config.config, &mut intent, &self.env);
        let impact = crate::impact::impact_of(
            &cmd,
            &self.model,
            &model,
            &self.intent,
            &intent,
            &self.config.config,
        );
        if dry_run {
            return Ok(Applied {
                impact: Some(impact),
                ..Applied::default()
            });
        }
        if let Some(c) = gate {
            c.check(&impact)?;
        }
        #[cfg(any(test, feature = "routing-shadow"))]
        if let Some(probe) = &mut self.admission_probe {
            probe.accepted(
                &origin,
                &cmd,
                true,
                &self.config.config,
                &model.devices,
                &intent,
            );
        }
        let mut events = self.diff_models(&self.model, &model, &origin);
        events.extend(runtime_changes(&self.intent, &intent, &origin));
        let effects: Vec<Effect> = model
            .devices
            .iter()
            .filter(|n| {
                self.model.device(&n.key.0).is_some_and(|o| {
                    (o.rx_channels, o.tx_channels) != (n.rx_channels, n.tx_channels)
                })
            })
            .map(|n| Effect::DeviceRestartRequired {
                device: n.key.clone(),
                reason: RestartReason::ChannelCount,
            })
            .collect();

        // Commit: nothing below can fail.
        #[cfg(any(test, feature = "routing-shadow"))]
        if !self.routing_query_context_eq(&self.config.config, &model, self.host_network.as_ref()) {
            self.advance_routing_query_revision();
        }
        self.intent = intent;
        self.prune_runtime(&model);
        self.model = model;
        self.state_warnings = warnings;
        if host_write {
            self.record_runtime_host_write(&cmd);
        }
        self.revalidate_runtime_binding();
        self.debounce.mark_dirty(self.clock.now());
        for e in &events {
            self.bus.publish(e);
        }
        Ok(Applied {
            changed: true,
            events,
            effects,
            impact: Some(impact),
        })
    }

    fn device_for<'a>(&'a self, key: &DeviceKey) -> Result<&'a DeviceModel, CommandError> {
        self.model
            .device(&key.0)
            .ok_or_else(|| not_found(format!("device `{}`", key.0)))
    }

    /// Validate `cmd` completely, then modify `intent`. Returns whether
    /// anything changed. Never modifies `intent` on error.
    fn mutate(&self, intent: &mut StateFile, cmd: &Command) -> Result<bool, CommandError> {
        if matches!(cmd, Command::Subscribe { .. } | Command::Unsubscribe { .. })
            && self
                .config
                .config
                .control
                .locked
                .contains(&ControlLock::Subscriptions)
        {
            return Err(locked("subscriptions"));
        }
        match cmd {
            Command::SetProtocolFlows { flows } => {
                if self
                    .config
                    .config
                    .control
                    .locked
                    .contains(&ControlLock::Flows)
                {
                    return Err(locked("flows"));
                }
                let converted = crate::flows::from_specs(flows, &self.env).map_err(flow_invalid)?;
                let mut config = crate::flows::effective_validation_config(
                    &self.config.config,
                    &self.model.devices,
                );
                config.flows.extend(converted);
                let issues: Vec<Issue> = crate::validate::validate(&config, &self.env)
                    .into_iter()
                    .filter(Issue::is_error)
                    .collect();
                if !issues.is_empty() {
                    return Err(flow_invalid(issues));
                }
                if intent.protocol_flows.as_ref() == Some(flows) {
                    return Ok(false);
                }
                intent.protocol_flows = Some(flows.clone());
                Ok(true)
            }
            Command::SetDeviceName { device, name } => {
                let d = self.device_for(device)?;
                if d.locked.contains(&DeviceLock::Name) {
                    return Err(locked(DeviceLock::Name.as_str()));
                }
                // An unchanged legacy name is a no-op, not a migration. New
                // names must meet the separate device naming rules.
                let stored = intent.device.get(&device.0).and_then(|s| s.name.as_ref());
                if stored.map_or(*name == d.name, |s| s == name) {
                    return Ok(false);
                }
                validate_device_name(name, self.env.max_name_bytes)
                    .map_err(|e| invalid("name", format!("name {e}")))?;
                let lower = name.to_ascii_lowercase();
                let clash = self
                    .model
                    .devices
                    .iter()
                    .any(|o| o.key != d.key && o.name.to_ascii_lowercase() == lower)
                    || self
                        .config
                        .config
                        .devices
                        .iter()
                        .any(|o| o.id != d.key.0 && o.name.to_ascii_lowercase() == lower)
                    // Names other devices hold in state (also inactive ones,
                    // and those of devices currently absent from the config)
                    // count too: otherwise the owner would silently lose its
                    // name when it becomes active again.
                    || intent.device.iter().any(|(k, s)| {
                        *k != d.key.0
                            && s.name
                                .as_deref()
                                .is_some_and(|n| n.to_ascii_lowercase() == lower)
                    });
                if clash {
                    return Err(invalid(
                        "name",
                        format!("`{name}` is already used by another device"),
                    ));
                }
                intent.device.entry(device.0.clone()).or_default().name = Some(name.clone());
                Ok(true)
            }
            Command::SetChannelLabel {
                device,
                direction,
                channel,
                label,
            } => {
                let d = self.device_for(device)?;
                let (lock, count) = match direction {
                    Direction::Rx => (DeviceLock::RxChannelLabels, d.rx_channels),
                    Direction::Tx => (DeviceLock::TxChannelLabels, d.tx_channels),
                };
                if d.locked.contains(&lock) {
                    return Err(locked(lock.as_str()));
                }
                let ch = channel.get();
                // Clearing a label (`None`) is allowed for any channel number
                // so that a stored label beyond the current channel count
                // (kept but inactive) can still be removed.
                if label.is_some() && ch > count {
                    return Err(invalid(
                        "channel",
                        format!("channel {ch} is beyond the device's {count} channel(s)"),
                    ));
                }
                let key = ch.to_string();
                let stored = intent.device.get(&device.0).and_then(|s| match direction {
                    Direction::Rx => s.rx_channel_labels.get(&key),
                    Direction::Tx => s.tx_channel_labels.get(&key),
                });
                let current = match direction {
                    Direction::Rx => d.rx_label(ch),
                    Direction::Tx => d.tx_label(ch),
                };
                if label.as_ref().map_or(stored.is_none(), |l| {
                    stored.map_or(current == *l, |s| s == l)
                }) {
                    return Ok(false);
                }
                let wanted = if let Some(l) = label {
                    validate_channel_label(l, self.env.max_name_bytes)
                        .map_err(|e| invalid("label", format!("label {e}")))?;
                    l.clone()
                } else {
                    // Reset restores configuration before the factory label.
                    self.config
                        .config
                        .devices
                        .iter()
                        .find(|c| c.id == device.0)
                        .and_then(|c| match direction {
                            Direction::Rx => c.rx_channel_labels.get(&ch),
                            Direction::Tx => c.tx_channel_labels.get(&ch),
                        })
                        .cloned()
                        .unwrap_or_else(|| match direction {
                            Direction::Rx => format!("RX {ch}"),
                            Direction::Tx => format!("TX {ch}"),
                        })
                };
                if ch <= count {
                    let clash = (1..=count).find(|other| {
                        *other != ch
                            && match direction {
                                Direction::Rx => d.rx_label(*other),
                                Direction::Tx => d.tx_label(*other),
                            }
                            .eq_ignore_ascii_case(&wanted)
                    });
                    if let Some(other) = clash {
                        return Err(invalid(
                            "label",
                            format!(
                                "`{wanted}` is already used by channel {other} in this direction (names ignore case)"
                            ),
                        ));
                    }
                }
                let dev = intent.device.entry(device.0.clone()).or_default();
                let map = match direction {
                    Direction::Rx => &mut dev.rx_channel_labels,
                    Direction::Tx => &mut dev.tx_channel_labels,
                };
                match label {
                    Some(l) => {
                        map.insert(key, l.clone());
                        Ok(true)
                    }
                    None => Ok(map.remove(&key).is_some()),
                }
            }
            Command::Subscribe {
                device,
                rx_channel,
                tx_channel,
                tx_device,
            } => {
                let d = self.device_for(device)?;
                let ch = rx_channel.get();
                if ch > d.rx_channels {
                    return Err(invalid(
                        "rx_channel",
                        format!(
                            "channel {ch} is beyond the device's {} rx channel(s)",
                            d.rx_channels
                        ),
                    ));
                }
                // These strings come from the network or a client and are
                // stored in the model and in state.toml: same rules as names.
                for (field, v) in [("tx_channel", tx_channel), ("tx_device", tx_device)] {
                    validate_name(v, self.env.max_name_bytes)
                        .map_err(|e| invalid(field, format!("{field} {e}")))?;
                }
                let wanted = SubscriptionEntry {
                    tx_channel: tx_channel.clone(),
                    tx_device: tx_device.clone(),
                };
                if d.subscriptions.get(&ch) == Some(&wanted) {
                    return Ok(false);
                }
                let dev = intent.device.entry(device.0.clone()).or_default();
                dev.subscription.retain(|s| s.rx_channel != ch);
                dev.subscription.push(SubscriptionIntent {
                    rx_channel: ch,
                    tx_channel: tx_channel.clone(),
                    tx_device: tx_device.clone(),
                });
                Ok(true)
            }
            Command::Unsubscribe { device, rx_channel } => {
                self.device_for(device)?;
                let ch = rx_channel.get();
                let dev = intent.device.entry(device.0.clone()).or_default();
                let before = dev.subscription.len();
                dev.subscription.retain(|s| s.rx_channel != ch);
                Ok(dev.subscription.len() != before)
            }
            Command::SetSampleRate { device, rate } => {
                let d = self.device_for(device)?;
                // The same rule gives the status's list of rates
                // (`crate::formats`). A bridge that pins its card's rate
                // cannot follow: refuse instead of leaving it unable to run.
                match crate::formats::sample_rate_refusal(&self.config.config, &self.env, d, rate.0)
                {
                    None => {}
                    Some(Refusal::Locked) => return Err(locked(DeviceLock::SampleRate.as_str())),
                    Some(Refusal::Unsupported) => {
                        return Err(invalid(
                            "sample_rate",
                            format!("{} Hz is not supported", rate.0),
                        ));
                    }
                    Some(Refusal::Bridge { name, sample_rate }) => {
                        return Err(invalid(
                            "sample_rate",
                            format!(
                                "bridge `{name}` runs its card at {sample_rate} Hz (`sample_rate` in its configuration); change or remove that first"
                            ),
                        ));
                    }
                }
                let stored = intent.device.get(&device.0).and_then(|s| s.sample_rate);
                if stored.map_or(rate.0 == d.sample_rate, |s| s == rate.0) {
                    return Ok(false);
                }
                intent
                    .device
                    .entry(device.0.clone())
                    .or_default()
                    .sample_rate = Some(rate.0);
                Ok(true)
            }
            Command::SetEncoding { device, encoding } => {
                let d = self.device_for(device)?;
                match crate::formats::encoding_refusal(&self.env, d, encoding.0) {
                    // A bridge never pins the encoding: it converts samples.
                    None | Some(Refusal::Bridge { .. }) => {}
                    Some(Refusal::Locked) => return Err(locked(DeviceLock::Encoding.as_str())),
                    Some(Refusal::Unsupported) => {
                        return Err(invalid(
                            "encoding",
                            format!("{} bits is not supported", encoding.0),
                        ));
                    }
                }
                let stored = intent.device.get(&device.0).and_then(|s| s.encoding);
                if stored.map_or(encoding.0 == d.encoding, |s| s == encoding.0) {
                    return Ok(false);
                }
                intent.device.entry(device.0.clone()).or_default().encoding = Some(encoding.0);
                Ok(true)
            }
            Command::SetLatency { device, latency } => {
                let d = self.device_for(device)?;
                if d.locked.contains(&DeviceLock::Latency) {
                    return Err(locked(DeviceLock::Latency.as_str()));
                }
                let dc = self
                    .config
                    .config
                    .device(&device.0)
                    .ok_or_else(|| not_found(format!("device `{}`", device.0)))?;
                if !latency_allowed(latency.0, dc, &self.env) {
                    let [lo, hi] = d.latency_bounds_us;
                    let reason = if latency.0 < lo || latency.0 > hi {
                        format!("{} us is outside the allowed range [{lo}, {hi}]", latency.0)
                    } else {
                        format!(
                            "{} us is below this platform's timing floor of {} us",
                            latency.0, self.env.latency_floor_us
                        )
                    };
                    return Err(invalid("latency", reason));
                }
                let stored = intent.device.get(&device.0).and_then(|s| s.latency_us);
                if stored.map_or(latency.0 == d.latency_us, |s| s == latency.0) {
                    return Ok(false);
                }
                intent
                    .device
                    .entry(device.0.clone())
                    .or_default()
                    .latency_us = Some(latency.0);
                Ok(true)
            }
            Command::SetClockRole { role } => {
                if self.model.clock.locked.contains(&ClockLock::Role) {
                    return Err(self.clock_locked(ClockLock::Role));
                }
                if *role == self.model.clock.role {
                    return Ok(false);
                }
                // Preferred leader and follower-only exclude each other
                // (device-settings.md §2.3).
                if *role == ClockRole::Follower
                    && self.model.clock.priority < PREFERRED_LEADER_BELOW
                {
                    return Err(invalid(
                        "clock.role",
                        "this device is a preferred leader; clear preferred leader \
                         (priority 64 or more) before making it follower-only",
                    ));
                }
                intent.clock.role = Some(*role);
                Ok(true)
            }
            Command::SetClockPriority { priority } => {
                if self.model.clock.locked.contains(&ClockLock::Priority) {
                    return Err(self.clock_locked(ClockLock::Priority));
                }
                if *priority == self.model.clock.priority {
                    return Ok(false);
                }
                if *priority < PREFERRED_LEADER_BELOW
                    && self.model.clock.role == ClockRole::Follower
                {
                    return Err(invalid(
                        "clock.priority",
                        "a follower-only device cannot be a preferred leader; set the \
                         clock role to auto or leader first",
                    ));
                }
                intent.clock.priority = Some(*priority);
                Ok(true)
            }
            Command::SetPrimaryInterface { interface } => {
                self.select_primary(intent, interface.as_deref())
            }
            Command::SetDeviceChannels { device, rx, tx, .. } => {
                self.set_channels(intent, device, *rx, *tx)
            }
            Command::ClearConfiguration {
                device,
                keep_network,
            } => self.clear_configuration(intent, device, *keep_network),
            // ReloadConfig is handled by the caller; anything else is a
            // command added to control-types after this crate was written.
            _ => Err(CommandError::Unsupported {
                what: "command".into(),
            }),
        }
    }

    /// Live keys whose configured value a reload changes while a value set
    /// at run time (in `state.toml`) keeps winning: say so instead of
    /// silently ignoring the edit.
    fn hidden_live_changes(&self, loaded: &LoadedConfig) -> Vec<Issue> {
        let mut out = Vec::new();
        let hint = |key: &str| {
            format!(
                "the configured value changed, but the value set at run time (stored in state.toml) stays in effect; add `{key}` to `locked` to make the file win, or change it at run time"
            )
        };
        for (i, n) in loaded.config.devices.iter().enumerate() {
            let (Some(o), Some(s)) = (
                self.config.config.device(&n.id),
                self.intent.device.get(&n.id),
            ) else {
                continue;
            };
            for (key, lock, changed, stored) in [
                ("name", DeviceLock::Name, o.name != n.name, s.name.is_some()),
                (
                    "sample_rate",
                    DeviceLock::SampleRate,
                    o.sample_rate != n.sample_rate,
                    s.sample_rate.is_some(),
                ),
                (
                    "encoding",
                    DeviceLock::Encoding,
                    o.encoding != n.encoding,
                    s.encoding.is_some(),
                ),
                (
                    "latency_us",
                    DeviceLock::Latency,
                    o.latency_us != n.latency_us,
                    s.latency_us.is_some(),
                ),
                (
                    "rx_channels",
                    DeviceLock::RxChannels,
                    o.rx_channels != n.rx_channels,
                    s.rx_channels.is_some(),
                ),
                (
                    "tx_channels",
                    DeviceLock::TxChannels,
                    o.tx_channels != n.tx_channels,
                    s.tx_channels.is_some(),
                ),
            ] {
                if changed && stored && !n.locked.contains(&lock) {
                    out.push(Issue::warning(
                        format!("device[{i}].{key}"),
                        hint(lock.as_str()),
                    ));
                }
            }
        }
        let (o, n, s) = (
            &self.config.config.clock,
            &loaded.config.clock,
            &self.intent.clock,
        );
        for (key, lock, changed, stored) in [
            ("role", ClockLock::Role, o.role != n.role, s.role.is_some()),
            (
                "priority",
                ClockLock::Priority,
                o.priority != n.priority,
                s.priority.is_some(),
            ),
        ] {
            if changed && stored && !n.locked.contains(&lock) {
                out.push(Issue::warning(format!("clock.{key}"), hint(key)));
            }
        }
        out
    }

    /// `Locked` for a clock value; a value set on the command line names
    /// that source.
    fn clock_locked(&self, l: ClockLock) -> CommandError {
        let path = format!("clock.{}", l.as_str());
        if self.config.overridden.contains(&path) {
            CommandError::locked_by(l.as_str(), crate::loader::OVERRIDES_SOURCE)
        } else {
            locked(l.as_str())
        }
    }

    /// `SetDeviceChannels`: refused if a locked count would change, for 0 x
    /// 0, when a `[[flow]]`/`[[bridge]]` entry uses a channel that would go
    /// away, or over the ring budget. Subscriptions on RX channels beyond
    /// the new count are deleted from `state.toml` (the one exception to
    /// "kept but inactive": the user confirmed removing those channels).
    fn set_channels(
        &self,
        intent: &mut StateFile,
        device: &DeviceKey,
        rx: u16,
        tx: u16,
    ) -> Result<bool, CommandError> {
        let d = self.device_for(device)?;
        if rx < d.rx_channels
            && self
                .config
                .config
                .control
                .locked
                .contains(&ControlLock::Subscriptions)
        {
            return Err(locked("subscriptions"));
        }
        for (lock, now, want) in [
            (DeviceLock::RxChannels, d.rx_channels, rx),
            (DeviceLock::TxChannels, d.tx_channels, tx),
        ] {
            if d.locked.contains(&lock) && now != want {
                return Err(locked(lock.as_str()));
            }
        }
        let dc = self
            .config
            .config
            .device(&device.0)
            .ok_or_else(|| not_found(format!("device `{}`", device.0)))?;
        // A no-op is decided by the *effective* counts: a stored pair that
        // is kept inactive (it failed a check) and is sent again must report
        // why, not "nothing changed".
        let stored = intent.device.get(&device.0);
        let stored_matches = |s: Option<u16>, v: u16| s.is_none_or(|s| s == v);
        if (rx, tx) == (d.rx_channels, d.tx_channels)
            && stored_matches(stored.and_then(|s| s.rx_channels), rx)
            && stored_matches(stored.and_then(|s| s.tx_channels), tx)
        {
            return Ok(false);
        }
        let (problems, _) = crate::impact::channel_problems(
            &self.runtime_config(),
            dc,
            d.sample_rate,
            d.latency_us,
            rx,
            tx,
            Some((d.rx_channels, d.tx_channels)),
        );
        if let Some(first) = problems.first() {
            return Err(invalid(
                &first.path,
                problems
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        let ds = intent.device.entry(device.0.clone()).or_default();
        if !d.locked.contains(&DeviceLock::RxChannels) {
            ds.rx_channels = Some(rx);
        }
        if !d.locked.contains(&DeviceLock::TxChannels) {
            ds.tx_channels = Some(tx);
        }
        // Only when RX channels really go away: with unchanged effective
        // counts (an inactive stored pair replaced by the effective one)
        // nothing is deleted.
        if rx < d.rx_channels {
            ds.subscription.retain(|s| s.rx_channel <= rx);
        }
        Ok(true)
    }

    /// `SetPrimaryInterface` (ADR 0012): the name must be valid, must not be
    /// the secondary, and must exist on the host now; Wi-Fi only with
    /// `allow_wireless`. Storing the selection already stored is a no-op.
    fn select_primary(
        &self,
        intent: &mut StateFile,
        interface: Option<&str>,
    ) -> Result<bool, CommandError> {
        if self.model.network.locked {
            return Err(locked(NetworkLock::Primary.as_str()));
        }
        let Some(name) = interface else {
            return Ok(intent.network.primary.take().is_some());
        };
        crate::validate::valid_interface_name(name).map_err(|e| invalid("interface", e))?;
        let net = &self.config.config.network;
        if net.secondary.as_deref() == Some(name) {
            return Err(invalid(
                "interface",
                format!("`{name}` is the secondary interface"),
            ));
        }
        let info = self
            .env
            .interfaces
            .lookup(name)
            .ok_or_else(|| not_found(format!("network interface `{name}`")))?;
        if info.wireless && !net.allow_wireless {
            return Err(invalid(
                "interface",
                format!(
                    "`{name}` is a Wi-Fi interface; set `[network] allow_wireless = true` to use it"
                ),
            ));
        }
        if intent.network.primary.as_deref() == Some(name) {
            return Ok(false);
        }
        intent.network.primary = Some(name.to_owned());
        Ok(true)
    }

    /// `ClearConfiguration` (`device-settings.md` §6, factory reset and
    /// clear configuration): drop every value stored at run time for
    /// `device`, so that the configuration applies again. Its wire identity
    /// and channel counts stay: they are not controller settings. The clock
    /// values are the host's, so they go too; the primary interface
    /// selection unless `keep_network`. Refused unless `[control]
    /// allow_factory_reset`, and when it would change subscriptions the host
    /// owns (`[control] locked`). The device access lock refuses it before
    /// this runs, and it never clears that lock (`device-lock.md` §9.1 L3).
    fn clear_configuration(
        &self,
        intent: &mut StateFile,
        device: &DeviceKey,
        keep_network: bool,
    ) -> Result<bool, CommandError> {
        let control = &self.config.config.control;
        if !control.allow_factory_reset {
            return Err(CommandError::Unsupported {
                what: "clearing the configuration is disabled on this host \
                       ([control] allow_factory_reset)"
                    .into(),
            });
        }
        self.device_for(device)?;
        let mut next = intent.clone();
        if let Some(stored) = next.device.get_mut(&device.0) {
            *stored = DeviceIntent {
                wire_device_id: stored.wire_device_id.take(),
                rx_channels: stored.rx_channels,
                tx_channels: stored.tx_channels,
                ..DeviceIntent::default()
            };
        }
        if control.locked.contains(&ControlLock::Subscriptions)
            && intent
                .device
                .get(&device.0)
                .is_some_and(|d| !d.subscription.is_empty())
        {
            return Err(locked("subscriptions"));
        }
        // The device's controller flows, manual multicast flows included;
        // other devices keep theirs. With `locked = ["flows"]` there are none.
        if !control.locked.contains(&ControlLock::Flows) {
            match &intent.protocol_flows {
                Some(flows) => {
                    next.protocol_flows = Some(
                        flows
                            .iter()
                            .filter(|f| f.device != *device)
                            .cloned()
                            .collect(),
                    );
                }
                None => {
                    let legacy = &self.config.config.legacy_protocol_flows;
                    if legacy.iter().any(|f| f.device == device.0) {
                        let kept = legacy
                            .iter()
                            .filter(|f| f.device != device.0)
                            .map(crate::flows::to_spec)
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(|e| invalid("flows", e))?;
                        next.protocol_flows = Some(kept);
                    }
                }
            }
        }
        next.clock = ClockIntent::default();
        if !keep_network {
            next.network = NetworkIntent::default();
        }
        if next == *intent {
            return Ok(false);
        }
        *intent = next;
        Ok(true)
    }

    // ---- reload ----------------------------------------------------------

    /// A reload: for a gated origin (`gate`), refused with
    /// `ConfirmationRequired` unless confirmed (and, with a token, the
    /// impact is still the one confirmed).
    fn reload(&mut self, gate: Option<&Confirm>) -> Result<Applied, CommandError> {
        let PreparedReload {
            loaded,
            intent,
            model,
            warnings,
            events,
            effects,
            impact,
        } = self.prepare_reload()?;
        if let Some(c) = gate {
            c.check(&impact)?;
        }
        #[cfg(any(test, feature = "routing-shadow"))]
        if let Some(probe) = &mut self.admission_probe {
            // Reload is the configuration authority's transition here. The
            // caller origin is not retained by this existing reload interface.
            probe.accepted(
                &Origin::Config,
                &Command::ReloadConfig,
                true,
                &loaded.config,
                &model.devices,
                &intent,
            );
        }
        #[cfg(any(test, feature = "routing-shadow"))]
        if !self.routing_query_context_eq(&loaded.config, &model, self.host_network.as_ref()) {
            self.advance_routing_query_revision();
        }
        let intent_changed = intent != self.intent;
        self.prune_runtime(&model);
        self.intent = intent;
        self.model = model;
        self.state_warnings = warnings;
        self.config = loaded;
        self.revalidate_runtime_binding();
        if intent_changed {
            self.debounce.mark_dirty(self.clock.now());
        }
        for e in &events {
            self.bus.publish(e);
        }
        Ok(Applied {
            changed: true,
            events,
            effects,
            impact: Some(impact),
        })
    }

    /// Everything a reload would do, without doing it.
    fn prepare_reload(&self) -> Result<PreparedReload, CommandError> {
        let mut loaded = read_sources(&self.paths.config)
            .and_then(|sources| {
                load_from_sources_for_state(
                    &sources,
                    &self.env,
                    &self.overrides,
                    self.intent.protocol_flows.is_some(),
                )
            })
            .map_err(|e| {
                let reason = match &e {
                    crate::error::ConfigError::Invalid { issues } => issues
                        .iter()
                        .filter(|i| i.is_error())
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; "),
                    other => other.to_string(),
                };
                invalid("config", reason)
            })?;
        let host: Vec<String> = validate_for_start(&loaded.config, &self.env)
            .iter()
            .filter(|i| i.is_error())
            .map(ToString::to_string)
            .collect();
        if !host.is_empty() {
            return Err(invalid("config", host.join("; ")));
        }

        if self.state_path_derived
            && loaded.config.daemon.state_dir != self.config.config.daemon.state_dir
        {
            return Err(invalid(
                "config",
                format!(
                    "daemon.state_dir changed from {} to {}: the state file location is fixed while running, restart the daemon to move it",
                    self.config.config.daemon.state_dir.display(),
                    loaded.config.daemon.state_dir.display()
                ),
            ));
        }

        // A device that has no explicit `name` is named after the host, with
        // `-<id>` appended once there are several devices. Say so when a
        // reload changes such a name only because the device count changed.
        for (i, new) in loaded.config.devices.iter().enumerate() {
            let Some(old) = self.config.config.device(&new.id) else {
                continue;
            };
            if old.name != new.name
                && loaded.defaulted_names.contains(&new.id)
                && self.config.defaulted_names.contains(&new.id)
            {
                let mut w = Issue::warning(
                    format!("device[{i}].name"),
                    format!(
                        "the default name of device `{}` changed from `{}` to `{}` because the number of devices changed; set an explicit `name` on the device to keep its name stable on the network",
                        new.id, old.name, new.name
                    ),
                );
                w.source = loaded
                    .provenance
                    .source_of(&format!("device[{i}].id"))
                    .map(Path::to_path_buf);
                loaded.warnings.push(w);
            }
        }
        let hidden = self.hidden_live_changes(&loaded);
        loaded.warnings.extend(hidden);

        let mut intent = self.intent.clone();
        let (model, warnings, _) = reconcile(&loaded.config, &mut intent, &self.env);

        // CFG-ZERO: a running device cannot be reloaded from some channels
        // to 0 in one direction yet (starting with 0 stays allowed).
        let zero: Vec<String> = model
            .devices
            .iter()
            .enumerate()
            .filter_map(|(i, n)| Some((i, n, self.model.device(&n.key.0)?)))
            .flat_map(|(i, n, o)| {
                [
                    ("rx_channels", o.rx_channels, n.rx_channels, "RX"),
                    ("tx_channels", o.tx_channels, n.tx_channels, "TX"),
                ]
                .into_iter()
                .filter(|(_, was, now, _)| *was != 0 && *now == 0)
                .map(move |(k, was, _, d)| {
                    format!(
                        "device[{i}].{k}: {}",
                        crate::impact::zero_message(&n.key.0, was, d)
                    )
                })
            })
            .collect();
        if let Some(first) = zero.first() {
            let field = first.split(':').next().unwrap_or("config").to_owned();
            return Err(invalid(&field, zero.join("; ")));
        }

        // A pinned bridge rate is checked against the EFFECTIVE device rate
        // (the files plus a rate stored at run time), which validation of
        // the files alone cannot see. Also when a reload enables a pinned
        // bridge that was disabled.
        let mismatches: Vec<(String, String)> = loaded
            .config
            .bridges
            .iter()
            .enumerate()
            .filter(|(_, b)| b.enabled)
            .filter_map(|(i, b)| {
                let r = b.sample_rate?;
                let d = model.device(&b.device)?;
                (d.sample_rate != r).then(|| {
                    (
                        format!("bridge[{i}].sample_rate"),
                        format!(
                            "bridge `{}` pins {r} Hz but device `{}` runs at {} Hz (a rate set at run time); set the rate or remove `sample_rate`",
                            b.name, b.device, d.sample_rate
                        ),
                    )
                })
            })
            .collect();
        if let Some((field, _)) = mismatches.first() {
            let reason = mismatches
                .iter()
                .map(|(p, m)| format!("{p}: {m}"))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(invalid(field, reason));
        }

        // Events caused by a reload carry `Origin::Config` ("config file load
        // or reload"), whoever asked for the reload.
        let origin = Origin::Config;
        let mut events = Vec::new();
        let mut effects = Vec::new();
        let interfaces_changed = loaded.config.network.interfaces
            != self.config.config.network.interfaces
            || loaded.config.network.secondary != self.config.config.network.secondary;
        for new in &model.devices {
            match self.model.device(&new.key.0) {
                None => effects.push(Effect::DeviceAdded(new.key.clone())),
                Some(old) => {
                    events.extend(Self::diff_device(old, new, &origin));
                    if old.rx_channels != new.rx_channels || old.tx_channels != new.tx_channels {
                        effects.push(Effect::DeviceRestartRequired {
                            device: new.key.clone(),
                            reason: RestartReason::ChannelCount,
                        });
                    } else if interfaces_changed {
                        effects.push(Effect::DeviceRestartRequired {
                            device: new.key.clone(),
                            reason: RestartReason::Interface,
                        });
                    }
                }
            }
        }
        for old in &self.model.devices {
            if model.device(&old.key.0).is_none() {
                effects.push(Effect::DeviceRemoved(old.key.clone()));
            }
        }
        let (a, b) = (&self.config.config, &loaded.config);
        for (name, same) in [
            ("daemon", a.daemon == b.daemon),
            ("control", a.control == b.control),
            ("audio", a.audio == b.audio),
            ("network", a.network == b.network),
            ("clock", a.clock == b.clock),
            ("engine", a.engine == b.engine),
            ("discovery", a.discovery == b.discovery),
            ("backends", a.backends == b.backends),
            ("aes67", a.aes67 == b.aes67),
            ("defaults", a.defaults == b.defaults),
            ("bridge", a.bridges == b.bridges),
            ("flow", a.flows == b.flows),
        ] {
            if !same {
                effects.push(Effect::ConfigSectionChanged(name));
            }
        }
        let restart_keys = crate::impact::changed_restart_keys(a, b)
            .map_err(|message| CommandError::Internal { message })?;
        if !restart_keys.is_empty() {
            effects.push(Effect::RestartKeysChanged(restart_keys));
        }
        events.extend(Self::diff_clock(&self.model, &model, &origin));
        if self.model.flows != model.flows {
            effects.push(Effect::ConfigSectionChanged("flow"));
            events.push(Event::ProtocolFlowsChanged {
                flows: model
                    .flows
                    .iter()
                    .filter(|f| !loaded.config.flows.iter().any(|host| host.id == f.id))
                    .map(|f| f.id.clone())
                    .collect(),
                origin: origin.clone(),
            });
        }
        events.extend(runtime_changes(&self.intent, &intent, &origin));
        events.push(Event::ConfigReloadedWithOrigin { origin });
        events.push(Event::ConfigReloaded);
        let impact = crate::impact::reload_impact(
            &self.config.config,
            &loaded.config,
            &self.model,
            &model,
            &self.intent,
            &intent,
        )
        .map_err(|message| CommandError::Internal { message })?;
        Ok(PreparedReload {
            loaded,
            intent,
            model,
            warnings,
            events,
            effects,
            impact,
        })
    }

    // ---- diff -------------------------------------------------------------

    fn diff_models(&self, old: &Model, new: &Model, origin: &Origin) -> Vec<Event> {
        let mut out = Vec::new();
        for n in &new.devices {
            if let Some(o) = old.device(&n.key.0) {
                out.extend(Self::diff_device(o, n, origin));
            }
        }
        out.extend(Self::diff_clock(old, new, origin));
        if old.flows != new.flows {
            out.push(Event::ProtocolFlowsChanged {
                flows: new
                    .flows
                    .iter()
                    .filter(|f| !self.config.config.flows.iter().any(|host| host.id == f.id))
                    .map(|f| f.id.clone())
                    .collect(),
                origin: origin.clone(),
            });
        }
        if old.network.selected != new.network.selected {
            out.push(Event::NetworkSettingsChanged {
                selected: new.network.selected.clone(),
                origin: origin.clone(),
            });
        }
        out
    }

    /// `ClockSettingsChanged` if role or priority differ (a change of the
    /// lock set alone is not a settings change).
    fn diff_clock(old: &Model, new: &Model, origin: &Origin) -> Option<Event> {
        ((old.clock.role, old.clock.priority) != (new.clock.role, new.clock.priority)).then(|| {
            Event::ClockSettingsChanged {
                role: new.clock.role,
                priority: new.clock.priority,
                origin: origin.clone(),
            }
        })
    }

    fn diff_device(old: &DeviceModel, new: &DeviceModel, origin: &Origin) -> Vec<Event> {
        let mut ev = Vec::new();
        if old.name != new.name {
            ev.push(Event::DeviceRenamed {
                device: new.key.clone(),
                name: new.name.clone(),
                origin: origin.clone(),
            });
        }
        for (direction, o, n, count) in [
            (
                Direction::Rx,
                &old.rx_labels,
                &new.rx_labels,
                new.rx_channels,
            ),
            (
                Direction::Tx,
                &old.tx_labels,
                &new.tx_labels,
                new.tx_channels,
            ),
        ] {
            let keys: BTreeSet<u16> = o.keys().chain(n.keys()).copied().collect();
            for k in keys {
                let Some(channel) = Channel::new(k).filter(|_| k <= count) else {
                    continue;
                };
                let label_of = |m: &BTreeMap<u16, String>| {
                    m.get(&k).cloned().unwrap_or_else(|| match direction {
                        Direction::Rx => format!("RX {k}"),
                        Direction::Tx => format!("TX {k}"),
                    })
                };
                let (a, b) = (label_of(o), label_of(n));
                if a != b {
                    ev.push(Event::ChannelLabelChanged {
                        device: new.key.clone(),
                        direction,
                        channel,
                        label: b,
                        origin: origin.clone(),
                    });
                }
            }
        }
        if (old.rx_channels, old.tx_channels) != (new.rx_channels, new.tx_channels) {
            ev.push(Event::DeviceChannelsChanged {
                device: new.key.clone(),
                rx_channels: new.rx_channels,
                tx_channels: new.tx_channels,
                origin: origin.clone(),
            });
        }
        if (old.sample_rate, old.encoding, old.latency_us)
            != (new.sample_rate, new.encoding, new.latency_us)
        {
            ev.push(Event::DeviceSettingsChanged {
                device: new.key.clone(),
                sample_rate: SampleRate(new.sample_rate),
                encoding: Encoding(new.encoding),
                latency: LatencyUs(new.latency_us),
                origin: origin.clone(),
            });
        }
        for (ch, entry) in &new.subscriptions {
            if old.subscriptions.get(ch) != Some(entry) {
                if let Some(rx_channel) = Channel::new(*ch) {
                    ev.push(Event::SubscriptionChanged {
                        device: new.key.clone(),
                        status: subscription_status(
                            rx_channel,
                            entry,
                            SubscriptionState::InProgress,
                            None,
                        ),
                    });
                }
            }
        }
        for ch in old.subscriptions.keys() {
            if !new.subscriptions.contains_key(ch) {
                if let Some(rx_channel) = Channel::new(*ch) {
                    ev.push(Event::SubscriptionRemoved {
                        device: new.key.clone(),
                        rx_channel,
                        origin: origin.clone(),
                    });
                }
            }
        }
        let companions: Vec<Event> = ev
            .iter()
            .filter_map(|event| match event {
                Event::SubscriptionChanged { device, status } => {
                    Some(Event::SubscriptionChangedWithOrigin {
                        device: device.clone(),
                        status: status.clone(),
                        origin: origin.clone(),
                    })
                }
                _ => None,
            })
            .collect();
        ev.extend(companions);
        ev
    }

    /// Forget observed subscription states that no longer describe a current
    /// subscription (removed, or pointing at a different transmitter).
    fn prune_runtime(&mut self, new: &Model) {
        let old = &self.model;
        self.runtime_subs.retain(|(dev, ch), _| {
            let n = new.device(dev).and_then(|d| d.subscriptions.get(ch));
            let o = old.device(dev).and_then(|d| d.subscriptions.get(ch));
            n.is_some() && n == o
        });
    }

    // ---- persistence ------------------------------------------------------

    fn persist_now(&self) -> Result<(), StateError> {
        let text = self.intent.render()?;
        write_atomic(&self.state_path, text.as_bytes())
    }

    /// When [`poll_persist`](Self::poll_persist) is next due, on the engine
    /// clock's timeline; `None` if nothing is waiting to be written.
    #[must_use]
    pub fn next_persist_deadline(&self) -> Option<Duration> {
        self.debounce.deadline()
    }

    /// Change the debounce window of `state.toml` writes: what the daemon
    /// does when a reload changes `[daemon] persist_delay_ms` (the engine
    /// starts with [`Environment::persist_delay`]). A write that is already
    /// pending keeps its deadline.
    pub fn set_persist_delay(&mut self, delay: Duration) {
        self.debounce.set_delay(delay);
    }

    /// Change the per-principal command rate limit (`None`: off): what the
    /// daemon does when a reload changes `[control] rate_burst` or
    /// `rate_per_second` (the engine starts with
    /// [`Environment::rate_limit`]). The buckets of known principals are
    /// kept: a larger burst refills them over time, a smaller one caps them
    /// at their next command (the refill is `min(capacity, ..)`); a
    /// smaller `max_peers` applies as new principals arrive.
    pub fn set_rate_limit(&mut self, limit: Option<RateLimit>) {
        match (&mut self.limiter, limit) {
            (Some(l), Some(cfg)) => l.cfg = cfg,
            (l, cfg) => *l = cfg.map(RateLimiter::new),
        }
    }

    /// Are there changes not yet written?
    #[must_use]
    pub fn persist_pending(&self) -> bool {
        self.debounce.is_pending()
    }

    /// Write the state file if the debounce window has elapsed. Returns
    /// whether a write happened. On failure the change stays pending and is
    /// retried after another window; the failure is also visible in
    /// [`warnings`](Self::warnings).
    ///
    /// # Errors
    /// [`StateError`] from the write.
    pub fn poll_persist(&mut self) -> Result<bool, StateError> {
        let now = self.clock.now();
        if !self.debounce.poll(now) {
            return Ok(false);
        }
        self.write_or_retry(now).map(|()| true)
    }

    /// Write now if anything is pending (shutdown, or before restarting a
    /// device). Returns whether a write happened.
    ///
    /// # Errors
    /// [`StateError`] from the write; the change stays pending.
    pub fn flush(&mut self) -> Result<bool, StateError> {
        if !self.debounce.take() {
            return Ok(false);
        }
        let now = self.clock.now();
        self.write_or_retry(now).map(|()| true)
    }

    fn write_or_retry(&mut self, now: Duration) -> Result<(), StateError> {
        match self.persist_now() {
            Ok(()) => {
                self.persist_error = None;
                Ok(())
            }
            Err(e) => {
                self.persist_error = Some(e.to_string());
                self.debounce.mark_dirty(now);
                Err(e)
            }
        }
    }

    // ---- invariants -------------------------------------------------------

    /// Check every invariant the engine promises; for tests and debug
    /// assertions.
    ///
    /// * the model has exactly the configured devices;
    /// * names are valid and unique (ignoring case);
    /// * rate and encoding are supported; latency is within bounds and the
    ///   platform floor (or `allow_unsafe_latency`);
    /// * locked values equal the configured ones;
    /// * labels and active subscriptions are within channel counts;
    /// * [`reconcile`] on the current intent is a no-op that reproduces the
    ///   model, and the rendered state file parses back to the same intent.
    ///
    /// # Errors
    /// A description of the first violated invariant.
    pub fn check_invariants(&self) -> Result<(), String> {
        let cfg: &Config = &self.config.config;
        if self.model.devices.len() != cfg.devices.len() {
            return Err("device count differs from configuration".into());
        }
        let mut names = BTreeSet::new();
        for (d, dc) in self.model.devices.iter().zip(&cfg.devices) {
            let id = &d.key.0;
            if *id != dc.id {
                return Err(format!("device order differs at `{id}`"));
            }
            validate_name(&d.name, self.env.max_name_bytes)
                .map_err(|e| format!("{id}: name {e}"))?;
            if !names.insert(d.name.to_ascii_lowercase()) {
                return Err(format!("{id}: duplicate name `{}`", d.name));
            }
            if !self.env.supported_sample_rates.contains(&d.sample_rate) {
                return Err(format!("{id}: unsupported rate {}", d.sample_rate));
            }
            if !self.env.supported_encodings.contains(&d.encoding) {
                return Err(format!("{id}: unsupported encoding {}", d.encoding));
            }
            if !latency_allowed(d.latency_us, dc, &self.env) {
                return Err(format!("{id}: latency {} not allowed", d.latency_us));
            }
            let lock = |l| d.locked.contains(&l);
            if lock(DeviceLock::Name) && d.name != dc.name {
                return Err(format!("{id}: locked name differs"));
            }
            if lock(DeviceLock::SampleRate) && d.sample_rate != dc.sample_rate {
                return Err(format!("{id}: locked sample_rate differs"));
            }
            if lock(DeviceLock::Encoding) && d.encoding != dc.encoding {
                return Err(format!("{id}: locked encoding differs"));
            }
            if lock(DeviceLock::Latency) && d.latency_us != dc.latency_us {
                return Err(format!("{id}: locked latency differs"));
            }
            if lock(DeviceLock::RxChannels) && d.rx_channels != dc.rx_channels {
                return Err(format!("{id}: locked rx_channels differs"));
            }
            if lock(DeviceLock::TxChannels) && d.tx_channels != dc.tx_channels {
                return Err(format!("{id}: locked tx_channels differs"));
            }
            if d.rx_channels == 0
                && d.tx_channels == 0
                && (dc.rx_channels, dc.tx_channels) != (0, 0)
            {
                return Err(format!("{id}: 0 x 0 channels from state"));
            }
            let within = |m: &BTreeMap<u16, String>, n: u16| -> BTreeMap<u16, String> {
                m.iter()
                    .filter(|(k, _)| **k <= n)
                    .map(|(k, v)| (*k, v.clone()))
                    .collect()
            };
            if lock(DeviceLock::RxChannelLabels)
                && d.rx_labels != within(&dc.rx_channel_labels, d.rx_channels)
            {
                return Err(format!("{id}: locked rx labels differ"));
            }
            if lock(DeviceLock::TxChannelLabels)
                && d.tx_labels != within(&dc.tx_channel_labels, d.tx_channels)
            {
                return Err(format!("{id}: locked tx labels differ"));
            }
            if d.rx_labels.keys().any(|k| *k == 0 || *k > d.rx_channels)
                || d.tx_labels.keys().any(|k| *k == 0 || *k > d.tx_channels)
            {
                return Err(format!("{id}: label outside channel range"));
            }
            if d.rx_labels
                .values()
                .chain(d.tx_labels.values())
                .any(|l| validate_name(l, self.env.max_name_bytes).is_err())
            {
                return Err(format!("{id}: invalid label"));
            }
            if d.subscriptions
                .keys()
                .any(|k| *k == 0 || *k > d.rx_channels)
            {
                return Err(format!("{id}: active subscription outside channel range"));
            }
        }
        if self.model.clock.locked.contains(&ClockLock::Role)
            && self.model.clock.role != cfg.clock.role
        {
            return Err("locked clock role differs".into());
        }
        if self.model.clock.locked.contains(&ClockLock::Priority)
            && self.model.clock.priority != cfg.clock.priority
        {
            return Err("locked clock priority differs".into());
        }
        if self.model.network.locked
            && (self.intent.network.primary.is_some() || self.model.network.selected.is_some())
        {
            return Err("locked primary interface has a runtime selection".into());
        }
        let mut effective = self.runtime_config();
        for dc in &mut effective.devices {
            if let Some(d) = self.model.device(&dc.id) {
                dc.rx_channels = d.rx_channels;
                dc.tx_channels = d.tx_channels;
            }
        }
        if let Some(issue) = crate::validate::validate(&effective, &self.env)
            .into_iter()
            .find(|i| i.is_error() && i.path.starts_with("flow["))
        {
            return Err(issue.to_string());
        }
        let mut again = self.intent.clone();
        let (model, _, changed) = reconcile(cfg, &mut again, &self.env);
        if changed || model != self.model {
            return Err("reconcile is not idempotent".into());
        }
        let text = self.intent.render().map_err(|e| e.to_string())?;
        let parsed = StateFile::parse(&text)?;
        if parsed != self.intent {
            return Err("state file does not round-trip".into());
        }
        Ok(())
    }
}

/// A reload computed but not committed.
struct PreparedReload {
    loaded: LoadedConfig,
    intent: StateFile,
    model: Model,
    warnings: Vec<Issue>,
    events: Vec<Event>,
    effects: Vec<Effect>,
    impact: Impact,
}

/// How a command arrived with respect to confirmation.
#[derive(Debug, Clone)]
enum Confirm {
    /// Not confirmed (`RequestBody::Command`).
    No,
    /// Confirmed without a digest.
    Blind,
    /// Confirmed for the impact with this token.
    Expecting(ImpactToken),
}

fn flow_invalid(issues: Vec<Issue>) -> CommandError {
    let field = issues
        .iter()
        .find(|i| i.is_error())
        .map_or("flows", |i| i.path.as_str());
    invalid(
        field,
        issues
            .iter()
            .filter(|i| i.is_error())
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; "),
    )
}

/// Expand legacy short error fields into paths without changing their wire shape.
fn command_field(cmd: &Command, refused: &str) -> (String, Option<String>) {
    let (path, value) = match cmd {
        Command::SetProtocolFlows { .. } => ("flows".into(), None),
        Command::SetClockRole { role } => ("clock.role".into(), Some(format!("{role:?}"))),
        Command::SetClockPriority { priority } => {
            ("clock.priority".into(), Some(priority.to_string()))
        }
        Command::SetPrimaryInterface { interface } => {
            ("network.primary".into(), Some(format!("{interface:?}")))
        }
        Command::SetDeviceName { device, name } => {
            (format!("device.{}.name", device.0), Some(name.clone()))
        }
        Command::SetChannelLabel {
            device,
            direction,
            channel,
            label,
        } => {
            let side = match direction {
                Direction::Rx => "rx",
                Direction::Tx => "tx",
            };
            (
                format!("device.{}.{side}_channel_labels.{channel}", device.0),
                Some(format!("{label:?}")),
            )
        }
        Command::SetSampleRate { device, rate } => (
            format!("device.{}.sample_rate", device.0),
            Some(rate.0.to_string()),
        ),
        Command::SetEncoding { device, encoding } => (
            format!("device.{}.encoding", device.0),
            Some(encoding.0.to_string()),
        ),
        Command::SetLatency { device, latency } => (
            format!("device.{}.latency_us", device.0),
            Some(latency.0.to_string()),
        ),
        Command::SetDeviceChannels { device, rx, tx, .. } => (
            format!(
                "device.{}.{}",
                device.0,
                if refused == "subscriptions" {
                    "rx_channels"
                } else {
                    refused
                }
            ),
            match refused {
                "rx_channels" | "subscriptions" => Some(rx.to_string()),
                "tx_channels" => Some(tx.to_string()),
                _ => None,
            },
        ),
        _ => (refused.into(), None),
    };
    (path, value.filter(|s| s.len() <= 128))
}

fn runtime_changes(before: &StateFile, after: &StateFile, origin: &Origin) -> Vec<Event> {
    let mut fields = Vec::new();
    if before.protocol_flows != after.protocol_flows {
        fields.push("flows".to_owned());
    }
    if before.clock.role != after.clock.role {
        fields.push("clock.role".to_owned());
    }
    if before.clock.priority != after.clock.priority {
        fields.push("clock.priority".to_owned());
    }
    if before.network.primary != after.network.primary {
        fields.push("network.primary".to_owned());
    }
    let keys: BTreeSet<&String> = before.device.keys().chain(after.device.keys()).collect();
    for key in keys {
        let empty = crate::persist::DeviceIntent::default();
        let a = before.device.get(key).unwrap_or(&empty);
        let b = after.device.get(key).unwrap_or(&empty);
        for (name, changed) in [
            ("name", a.name != b.name),
            ("sample_rate", a.sample_rate != b.sample_rate),
            ("encoding", a.encoding != b.encoding),
            ("latency_us", a.latency_us != b.latency_us),
            ("rx_channels", a.rx_channels != b.rx_channels),
            ("tx_channels", a.tx_channels != b.tx_channels),
            (
                "rx_channel_labels",
                a.rx_channel_labels != b.rx_channel_labels,
            ),
            (
                "tx_channel_labels",
                a.tx_channel_labels != b.tx_channel_labels,
            ),
            ("subscription", a.subscription != b.subscription),
        ] {
            if changed {
                fields.push(format!("device.{key}.{name}"));
            }
        }
    }
    fields
        .into_iter()
        .map(|field| Event::RuntimeValueChanged {
            field,
            origin: origin.clone(),
        })
        .collect()
}

impl Confirm {
    /// For a gated origin: may a change with `impact` go ahead?
    fn check(&self, impact: &Impact) -> Result<(), CommandError> {
        if !impact.needs_confirmation() {
            return Ok(());
        }
        let ok = match self {
            Self::No => false,
            Self::Blind => true,
            // The daemon's sealed token (it covers the command's arguments
            // and, for a reload, the changed entries).
            Self::Expecting(t) => impact.token.as_ref() == Some(t),
        };
        if ok {
            Ok(())
        } else {
            Err(CommandError::ConfirmationRequired {
                impact: Box::new(impact.clone()),
            })
        }
    }
}

#[cfg(test)]
mod query_revision_tests {
    use super::*;
    #[test]
    fn exhausted_query_context_stays_unavailable_across_network_and_reload() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "tessera-query-revision-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        let config = directory.join("config.toml");
        std::fs::write(&config, "").unwrap();
        let (mut engine, _) = StateEngine::start(
            crate::Environment::new("query-revision-test"),
            Paths::new(
                crate::ConfigPaths::new(config, directory.join("conf.d")),
                directory.join("state.toml"),
            ),
            Arc::new(crate::ManualClock::new()),
        )
        .unwrap();
        engine.routing_query_revision = Some(u64::MAX);
        engine.report_network_status(NetworkStatus::new(
            None,
            tessera_control_types::PrimarySource::Automatic,
        ));
        assert_eq!(engine.routing_query_revision(), None);
        engine
            .apply_confirmed(Origin::Internal, Command::ReloadConfig)
            .unwrap();
        assert_eq!(engine.routing_query_revision(), None);
    }
}

#[cfg(test)]
mod notice_tests {
    use super::*;

    #[test]
    fn a_notice_shows_with_the_warnings_until_it_expires_and_replaces_its_field() {
        let directory = std::env::temp_dir().join(format!("tessera-notice-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let config = directory.join("config.toml");
        std::fs::write(&config, "").unwrap();
        let clock = Arc::new(crate::ManualClock::new());
        let (mut engine, _) = StateEngine::start(
            crate::Environment::new("notice-test"),
            Paths::new(
                crate::ConfigPaths::new(config, directory.join("conf.d")),
                directory.join("state.toml"),
            ),
            Arc::clone(&clock) as Arc<dyn Clock>,
        )
        .unwrap();
        let shown = |e: &StateEngine| {
            e.warnings()
                .iter()
                .filter(|i| i.path == "identify")
                .map(|i| i.message.clone())
                .collect::<Vec<_>>()
        };
        engine.report_notice("identify", "first".into(), Duration::from_secs(10));
        engine.report_notice("identify", "second".into(), Duration::from_secs(10));
        assert_eq!(shown(&engine), ["second"]);
        clock.advance(Duration::from_secs(11));
        assert!(shown(&engine).is_empty());
        drop(engine);
        let _ = std::fs::remove_dir_all(&directory);
    }
}
