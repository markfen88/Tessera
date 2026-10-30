//! The state engine on its own thread (ADR 0004, ADR 0008 amendment).
//!
//! `tessera_state::StateEngine` is synchronous and does blocking file I/O
//! (`start`, `poll_persist`, `flush`, reload). It therefore never runs on the
//! async executor: one plain `std::thread` owns it and serves a bounded
//! command channel. The async side uses [`EngineHandle`], which never blocks:
//! a full queue is reported to the caller as a busy error, and each request
//! carries a `oneshot` for the answer.
//!
//! The thread also drives the debounced `state.toml` write: it sleeps until
//! the engine's persist deadline (or the next message), so it never spins.
//! Events are *not* routed through this thread: the [`EventBus`] handle is
//! taken before the engine moves here, and subscribers read it directly.
//!
//! If the thread ever ends without a shutdown request (a panic is a bug), the
//! `fatal` token is cancelled so the daemon exits and the service manager
//! restarts it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tessera_control_types::{
    BridgeStatus, Channel, ClockStatus, Command, CommandError, DeviceKey, ImpactToken,
    NetworkStatus, Origin, Query, Reply, SubscriptionState,
};
use tessera_state::{Applied, Config, MonotonicClock, StateEngine};

use crate::tunables::Reloadable;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

/// Values the control plane needs that can change on reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// `[daemon] admin_group`.
    pub admin_group: String,
    /// The gid of `admin_group` (`None` if the group does not exist),
    /// resolved here on the state thread so that connection tasks never
    /// touch the user database.
    pub admin_gid: Option<u32>,
    /// Trusted portal account, resolved off connection tasks.
    pub routing_gateway_uid: Option<u32>,
    /// `[daemon] log_level`.
    pub log_level: String,
    /// The tunables of apply class `reload` the daemon applies itself.
    pub tunables: Reloadable,
    /// Keys of apply class `restart` whose value in the files differs from
    /// the one this process started with: they take effect at the next
    /// service restart (shown in the status warnings).
    pub restart_pending: Vec<String>,
}

impl Settings {
    /// Read the settings from an engine. Blocking (resolves the admin group).
    /// `started` is the configuration the process started with.
    #[must_use]
    pub fn of(engine: &StateEngine, started: &Config) -> Self {
        let cfg = &engine.config().config;
        let d = &cfg.daemon;
        Self {
            admin_group: d.admin_group.clone(),
            admin_gid: crate::peer::resolve_admin_group(&d.admin_group),
            routing_gateway_uid: nix::unistd::User::from_name("tessera-web")
                .ok()
                .flatten()
                .map(|u| u.uid.as_raw()),
            log_level: d.log_level.clone(),
            tunables: Reloadable::of(cfg),
            restart_pending: tessera_state::impact::changed_restart_keys(started, cfg)
                .unwrap_or_else(|e| {
                    tracing::warn!(
                        "cannot compare the configuration with the one at start: {}",
                        crate::sanitize::log_safe(&e, 200)
                    );
                    Vec::new()
                }),
        }
    }

    /// The status warning for [`restart_pending`](Self::restart_pending).
    #[must_use]
    pub fn restart_warning(&self) -> Option<String> {
        (!self.restart_pending.is_empty()).then(|| {
            format!(
                "changed in the configuration, takes effect at the next service restart: {}",
                self.restart_pending.join(", ")
            )
        })
    }
}

/// How a command was confirmed (the control socket's request form).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// `RequestBody::Command`: a change that restarts something is refused
    /// with `ConfirmationRequired` (`StateEngine::apply`).
    No,
    /// `RequestBody::ConfirmedCommand { expect: None }`
    /// (`StateEngine::apply_confirmed`).
    Blind,
    /// `RequestBody::ConfirmedCommand { expect: Some(token) }`
    /// (`StateEngine::apply_expecting`).
    Expecting(ImpactToken),
}

/// What an audio client needs to know about a logical device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Config key.
    pub key: String,
    /// RX channel count.
    pub rx_channels: u16,
    /// TX channel count.
    pub tx_channels: u16,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Current latency in microseconds.
    pub latency_us: u32,
    /// Upper latency bound in microseconds (sizes the ring).
    pub latency_max_us: u32,
}

/// A copy of everything the runtime reconciler derives its work from: the
/// effective values (configuration combined with runtime changes) and the
/// loaded configuration.
#[derive(Debug, Clone)]
pub struct RuntimePlan {
    /// Effective devices and clock settings.
    pub model: tessera_state::Model,
    /// The merged configuration.
    pub config: tessera_state::Config,
}

/// Requests served by the engine thread.
#[derive(Debug)]
pub(crate) enum Msg {
    RuntimeStart {
        start: crate::runtime_host::Start,
        reply: oneshot::Sender<Result<(), CommandError>>,
    },
    RuntimeReport {
        evidence: crate::runtime_host::Evidence,
        flows: Vec<tessera_control_types::FlowSpec>,
        reply: oneshot::Sender<Result<Applied, CommandError>>,
    },
    RuntimeEcho {
        evidence: crate::runtime_host::Evidence,
        reply: oneshot::Sender<Result<Applied, CommandError>>,
    },
    RuntimeReadStart {
        reply: oneshot::Sender<Result<Option<crate::runtime_host::ReadJob>, CommandError>>,
    },
    RuntimeReadFinish {
        job: crate::runtime_host::ReadJob,
        selections: Option<Vec<(Channel, Option<tessera_control_types::ReceiverSelection>)>>,
        reply: oneshot::Sender<Result<Applied, CommandError>>,
    },
    ReceiverTarget {
        query: tessera_control_types::RoutingReceiverQuery,
        reply: oneshot::Sender<Result<crate::routing_receiver::Target, CommandError>>,
    },
    ReceiverResult {
        principal: crate::routing_operation::accounting::Principal,
        target: crate::routing_receiver::Target,
        page: crate::routing_receiver::PreparedPage,
        deadline: Instant,
        max_frame: usize,
        reply: oneshot::Sender<Result<Reply, CommandError>>,
    },
    ReceiverOperation {
        principal: crate::routing_operation::accounting::Principal,
        token: tessera_control_types::ReceiverExpectation,
        change: tessera_control_types::ReceiverChange,
        engine: EngineHandle,
        executor: tokio::runtime::Handle,
        reply: oneshot::Sender<Result<Reply, CommandError>>,
    },
    ReceiverOperationStatus {
        principal: crate::routing_operation::accounting::Principal,
        operation: tessera_control_types::ReceiverExpectation,
        retire: bool,
        reply: oneshot::Sender<Result<Reply, CommandError>>,
    },
    ReceiverRecoveryStart {
        uid: u32,
        operation: Option<tessera_control_types::ReceiverExpectation>,
        review: Option<tessera_control_types::ReceiverExpectation>,
        reply: oneshot::Sender<Result<crate::routing_operation::recovery::Start, CommandError>>,
    },
    RuntimeCarrierRecovery {
        uid: u32,
        review: Option<tessera_control_types::ReceiverExpectation>,
        next_run: bool,
        reply: oneshot::Sender<Result<Reply, CommandError>>,
    },
    ReceiverRecoveryFinish {
        uid: u32,
        snapshot: Box<crate::routing_operation::recovery::Snapshot>,
        deadline: Instant,
        reviewed: Option<Vec<tessera_control_types::ReceiverRecoverySelection>>,
        observed: Vec<tessera_control_types::ReceiverRecoverySelection>,
        reply: oneshot::Sender<Result<Reply, CommandError>>,
    },
    ReceiverDispatch {
        job: Arc<crate::routing_operation::actor::Job>,
        reply: oneshot::Sender<Result<(), CommandError>>,
    },
    ReceiverFinish {
        job: Arc<crate::routing_operation::actor::Job>,
        outcome: crate::routing_operation::RunResult,
        reply: oneshot::Sender<Result<Reply, CommandError>>,
    },
    #[cfg(all(test, feature = "routing-shadow"))]
    QueryProbeTest {
        reply: oneshot::Sender<Vec<query_probe::Snapshot>>,
    },
    #[cfg(all(test, feature = "routing-shadow"))]
    QueryProbeFixture {
        group: String,
        reply: oneshot::Sender<()>,
    },
    #[cfg(feature = "routing-shadow")]
    QuerySent {
        tag: query_probe::Tag,
        original_success: bool,
    },
    #[cfg(feature = "routing-shadow")]
    QueryClosed(Arc<query_probe::Connection>),
    /// Test premises cannot be constructed by the production socket decoder.
    #[cfg(test)]
    RoutingTest {
        event: tessera_state::routing_shadow_test_hooks::TestEvent,
        reply: oneshot::Sender<
            Result<
                tessera_state::routing_shadow_test_hooks::TestReply,
                tessera_state::routing_shadow_test_hooks::TestError,
            >,
        >,
    },
    /// Apply a command.
    Apply {
        /// Who asked.
        origin: Origin,
        /// What to do.
        cmd: Command,
        /// How it was confirmed.
        confirm: Confirm,
        /// Kernel evidence retained from this accepted control connection.
        /// Availability alone is not registered-holder authentication.
        protocol_evidence: Option<ProtocolEvidence>,
        #[cfg(feature = "routing-shadow")]
        connection: Option<query_probe::Tag>,
        #[cfg(test)]
        observed: Option<oneshot::Sender<ConnectorObservation>>,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<Applied, CommandError>>,
    },
    /// Answer a read-only query.
    Query {
        /// The query.
        query: Query,
        /// Limit of this requesting connection, including its envelope.
        max_frame: usize,
        #[cfg(feature = "routing-shadow")]
        evidence: Option<query_probe::Tag>,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<Reply, CommandError>>,
    },
    /// A coalesced available discovery-cache snapshot; never persisted.
    ReportDiscovery(crate::routing_inventory::DiscoverySnapshot),
    /// Look up a device (`None` = the first one).
    DeviceInfo {
        /// Device key.
        key: Option<String>,
        /// Where the answer goes.
        reply: oneshot::Sender<Option<DeviceInfo>>,
    },
    /// The effective model and configuration (for the runtime reconciler).
    RuntimePlan {
        /// Where the answer goes.
        reply: oneshot::Sender<RuntimePlan>,
    },
    /// A clock observation from the PTP thread.
    ReportClock(ClockStatus),
    /// The primary interface and the candidates, from the reconciler.
    ReportNetwork(NetworkStatus),
    /// The interfaces that run a PTP port now, from the reconciler.
    ClockInterfaces(Vec<String>),
    /// What the bridges are doing, from the bridge manager.
    ReportBridges(Vec<BridgeStatus>),
    /// One sweep of what the media layer sees of each rx subscription.
    ReportSubscriptions(Vec<(DeviceKey, Channel, SubscriptionState)>),
    ReportSubscriptionStates(Vec<(DeviceKey, Channel, SubscriptionState)>),
    ReportNotice {
        field: String,
        message: String,
        duration: Duration,
    },
    /// Flush and stop.
    Shutdown {
        /// Signalled after the final flush; carries the flush error, if any.
        reply: oneshot::Sender<Option<String>>,
    },
}

/// Internal transport only; no wire schema, lifetime claim or routing right.
#[derive(Debug, Clone)]
pub(crate) struct ProtocolEvidence {
    connector: Option<Arc<crate::platform::ConnectedProcess>>,
    credentials: crate::peer::PeerCred,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectorObservation {
    Unavailable,
    Live(crate::platform::ProcessKey),
    DeadOrMismatched,
}
impl ProtocolEvidence {
    pub(crate) fn new(
        connector: Option<Arc<crate::platform::ConnectedProcess>>,
        credentials: crate::peer::PeerCred,
    ) -> Self {
        Self {
            connector,
            credentials,
        }
    }
    fn observe(&self) -> ConnectorObservation {
        match &self.connector {
            None => ConnectorObservation::Unavailable,
            Some(process) => match process.matches_credentials(&self.credentials) {
                Ok(()) => ConnectorObservation::Live(process.key()),
                Err(_) => ConnectorObservation::DeadOrMismatched,
            },
        }
    }

    fn runtime_unit(&self) -> Result<bool, CommandError> {
        let process = self
            .connector
            .as_ref()
            .ok_or_else(|| crate::runtime_host::unavailable("flow sender identity unavailable"))?;
        process
            .matches_credentials(&self.credentials)
            .map_err(|_| crate::runtime_host::unavailable("flow sender changed or exited"))?;
        process
            .in_runtime_unit()
            .map_err(|_| crate::runtime_host::unavailable("cannot classify flow sender"))
    }
}

/// Runtime reports must not become intentional whole-set host writes, even
/// when an old adapter uses the legacy command or claims a protocol origin.
/// Classify again after queue delay; unavailable identity never means host.
fn host_flow_write(
    origin: &Origin,
    command: &Command,
    evidence: Option<&ProtocolEvidence>,
) -> Result<(), CommandError> {
    if !matches!(command, Command::SetProtocolFlows { .. }) {
        return Ok(());
    }
    match evidence {
        Some(evidence) => host_flow_sender(evidence.runtime_unit()),
        None if matches!(origin, Origin::Internal | Origin::Config) => Ok(()),
        None => Err(crate::runtime_host::unavailable(
            "flow sender identity unavailable",
        )),
    }
}

fn host_flow_sender(runtime: Result<bool, CommandError>) -> Result<(), CommandError> {
    if runtime? {
        return Err(crate::runtime_host::unavailable(
            "runtime flows require the guarded RuntimeReport request; update the adapter",
        ));
    }
    Ok(())
}

/// Tells the watchdog whether the engine thread is stuck inside one
/// operation (a file system that stopped answering), without depending on how
/// long ordinary state writes take.
#[derive(Debug, Default)]
pub struct Pulse {
    /// Milliseconds since `origin` at which the current operation began, or 0
    /// when idle.
    busy_since_ms: AtomicU64,
    origin: OnceLock<Instant>,
}

impl Pulse {
    fn origin(&self) -> Instant {
        *self.origin.get_or_init(Instant::now)
    }

    fn enter(&self) {
        let ms = u64::try_from(self.origin().elapsed().as_millis()).unwrap_or(u64::MAX);
        self.busy_since_ms.store(ms.max(1), Ordering::Relaxed);
    }

    fn leave(&self) {
        self.busy_since_ms.store(0, Ordering::Relaxed);
    }

    /// How long the thread has been inside its current operation, if it is in
    /// one.
    #[must_use]
    pub fn busy_for(&self) -> Option<Duration> {
        let since = self.busy_since_ms.load(Ordering::Relaxed);
        if since == 0 {
            return None;
        }
        let now = u64::try_from(self.origin().elapsed().as_millis()).unwrap_or(u64::MAX);
        Some(Duration::from_millis(now.saturating_sub(since)))
    }
}

/// Cloneable, non-blocking client of the engine thread.
#[derive(Debug, Clone)]
pub struct EngineHandle {
    tx: SyncSender<Msg>,
    timeout: Duration,
    pulse: Arc<Pulse>,
    receiver_reads: Arc<crate::routing_receiver::ReadLimits>,
}

fn busy(what: &str) -> CommandError {
    CommandError::Internal {
        message: format!("state engine {what}"),
    }
}

impl EngineHandle {
    pub(crate) async fn runtime_start(
        &self,
        start: crate::runtime_host::Start,
    ) -> Result<(), CommandError> {
        self.ask(|reply| Msg::RuntimeStart { start, reply }).await?
    }

    pub(crate) async fn runtime_report(
        &self,
        evidence: crate::runtime_host::Evidence,
        flows: Vec<tessera_control_types::FlowSpec>,
    ) -> Result<Applied, CommandError> {
        self.ask(|reply| Msg::RuntimeReport {
            evidence,
            flows,
            reply,
        })
        .await?
    }

    pub(crate) async fn runtime_echo(
        &self,
        evidence: crate::runtime_host::Evidence,
    ) -> Result<Applied, CommandError> {
        self.ask(|reply| Msg::RuntimeEcho { evidence, reply })
            .await?
    }

    pub(crate) async fn reconcile_runtime(&self) -> Result<(), CommandError> {
        while let Some(job) = self.ask(|reply| Msg::RuntimeReadStart { reply }).await?? {
            let selections = job
                .read()
                .await
                .inspect_err(|_| {
                    tracing::warn!("runtime: receiver read failed; saved subscriptions retained");
                })
                .ok();
            let result = self
                .ask(|reply| Msg::RuntimeReadFinish {
                    job,
                    selections,
                    reply,
                })
                .await?;
            if result.is_err() {
                tracing::debug!("runtime: saved subscription reconciliation refused or superseded");
            }
        }
        Ok(())
    }
    /// The engine thread's busy indicator.
    #[must_use]
    pub fn pulse(&self) -> &Arc<Pulse> {
        &self.pulse
    }

    fn send(&self, msg: Msg) -> Result<(), CommandError> {
        self.tx.try_send(msg).map_err(|e| match e {
            TrySendError::Full(_) => busy("is busy, retry"),
            TrySendError::Disconnected(_) => busy("is not running"),
        })
    }

    async fn ask<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> Msg,
    ) -> Result<T, CommandError> {
        let (tx, rx) = oneshot::channel();
        self.send(make(tx))?;
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => Err(busy("stopped before answering")),
            Err(_) => Err(busy("did not answer in time")),
        }
    }

    /// Apply a command as a plain (unconfirmed) request: a change that
    /// restarts something is refused for gated origins (CLI, API) with
    /// `ConfirmationRequired`; `Origin::Config`, `Origin::Protocol` and
    /// `Origin::Internal` are never asked.
    ///
    /// # Errors
    /// The engine's refusal, or `Internal` if the engine is busy, stopped or
    /// too slow.
    pub async fn apply(&self, origin: Origin, cmd: Command) -> Result<Applied, CommandError> {
        self.apply_with(origin, cmd, Confirm::No).await
    }

    /// Apply a command with the confirmation it came with.
    ///
    /// # Errors
    /// As [`apply`](Self::apply).
    pub async fn apply_with(
        &self,
        origin: Origin,
        cmd: Command,
        confirm: Confirm,
    ) -> Result<Applied, CommandError> {
        self.ask(|reply| Msg::Apply {
            origin,
            cmd,
            confirm,
            protocol_evidence: None,
            #[cfg(feature = "routing-shadow")]
            connection: None,
            #[cfg(test)]
            observed: None,
            reply,
        })
        .await?
    }

    /// Carry the original connector/credentials through the existing queue.
    /// Legacy runtime whole-set writes are refused at the actor boundary.
    #[cfg(not(feature = "routing-shadow"))]
    pub(crate) async fn apply_connected(
        &self,
        origin: Origin,
        cmd: Command,
        confirm: Confirm,
        evidence: ProtocolEvidence,
    ) -> Result<Applied, CommandError> {
        self.ask(|reply| Msg::Apply {
            origin,
            cmd,
            confirm,
            protocol_evidence: Some(evidence),
            #[cfg(feature = "routing-shadow")]
            connection: None,
            #[cfg(test)]
            observed: None,
            reply,
        })
        .await?
    }

    #[cfg(feature = "routing-shadow")]
    pub(crate) async fn apply_observed(
        &self,
        origin: Origin,
        cmd: Command,
        confirm: Confirm,
        protocol_evidence: Option<ProtocolEvidence>,
        connection: Option<query_probe::Tag>,
    ) -> Result<Applied, CommandError> {
        self.ask(|reply| Msg::Apply {
            origin,
            cmd,
            confirm,
            protocol_evidence,
            connection,
            #[cfg(test)]
            observed: None,
            reply,
        })
        .await?
    }

    /// Answer a query.
    ///
    /// # Errors
    /// As for [`apply`](Self::apply).
    pub async fn query(&self, query: Query) -> Result<Reply, CommandError> {
        self.query_bounded(query, crate::frame::DEFAULT_MAX_FRAME_LEN)
            .await
    }

    /// Answer a query using the caller's actual frame limit for paging.
    pub async fn query_bounded(
        &self,
        query: Query,
        max_frame: usize,
    ) -> Result<Reply, CommandError> {
        if matches!(query, Query::InspectRuntimeCarriers) {
            return self
                .runtime_carrier_recovery(0, crate::peer::Access::Admin, None, false)
                .await;
        }
        if let Query::InspectReceiverOperation { operation } = query {
            return self
                .receiver_recovery(0, crate::peer::Access::Admin, operation, None)
                .await;
        }
        if let Query::RoutingReceiver(query) = query {
            return self
                .receiver_read(query, max_frame, 0, crate::peer::Access::Admin)
                .await;
        }
        self.ask(|reply| Msg::Query {
            query,
            max_frame,
            #[cfg(feature = "routing-shadow")]
            evidence: None,
            reply,
        })
        .await?
    }

    #[cfg(feature = "routing-shadow")]
    pub(crate) async fn query_observed(
        &self,
        query: Query,
        max_frame: usize,
        evidence: Option<query_probe::Tag>,
    ) -> Result<Reply, CommandError> {
        if matches!(query, Query::InspectRuntimeCarriers) {
            return self
                .runtime_carrier_recovery(0, crate::peer::Access::Admin, None, false)
                .await;
        }
        if let Query::InspectReceiverOperation { operation } = query {
            return self
                .receiver_recovery(0, crate::peer::Access::Admin, operation, None)
                .await;
        }
        if let Query::RoutingReceiver(query) = query {
            return self
                .receiver_read(query, max_frame, 0, crate::peer::Access::Admin)
                .await;
        }
        self.ask(|reply| Msg::Query {
            query,
            max_frame,
            evidence,
            reply,
        })
        .await?
    }

    pub(crate) async fn receiver_read(
        &self,
        query: tessera_control_types::RoutingReceiverQuery,
        max_frame: usize,
        uid: u32,
        access: crate::peer::Access,
    ) -> Result<Reply, CommandError> {
        self.receiver_read_as(query, max_frame, uid.into(), access)
            .await
    }

    pub(crate) async fn receiver_read_as(
        &self,
        query: tessera_control_types::RoutingReceiverQuery,
        max_frame: usize,
        principal: crate::routing_operation::accounting::Principal,
        access: crate::peer::Access,
    ) -> Result<Reply, CommandError> {
        if query.first == 0 {
            return Err(CommandError::Invalid {
                field: "first".into(),
                reason: "receiver page index is one-based".into(),
            });
        }
        // One per uid, two ordinary slots plus one reserved for administrators.
        let _permit = self.receiver_reads.acquire(principal.uid, access)?;
        let first = query.first;
        let target = self
            .ask(|reply| Msg::ReceiverTarget { query, reply })
            .await??;
        let deadline = Instant::now() + Duration::from_secs(3);
        let page = crate::routing_receiver::read(&target, first, deadline, max_frame).await?;
        // Another device's access lock (`device-lock.md` §9.3 L14); this
        // host's own receiver answers from the state engine instead.
        let page = if *target.peer.ip() == target.source {
            page
        } else {
            let locked = crate::routing_receiver::lock_state::read(&target, deadline).await;
            page.with_access_locked(locked)
        };
        self.ask(|reply| Msg::ReceiverResult {
            principal,
            target,
            page,
            deadline,
            max_frame,
            reply,
        })
        .await?
    }

    pub(crate) async fn receiver_operation(
        &self,
        uid: u32,
        token: tessera_control_types::ReceiverExpectation,
        change: tessera_control_types::ReceiverChange,
    ) -> Result<Reply, CommandError> {
        self.receiver_operation_as(uid.into(), token, change).await
    }

    pub(crate) async fn receiver_operation_as(
        &self,
        principal: crate::routing_operation::accounting::Principal,
        token: tessera_control_types::ReceiverExpectation,
        change: tessera_control_types::ReceiverChange,
    ) -> Result<Reply, CommandError> {
        self.ask(|reply| Msg::ReceiverOperation {
            principal,
            token,
            change,
            engine: self.clone(),
            executor: tokio::runtime::Handle::current(),
            reply,
        })
        .await?
    }

    pub(crate) async fn receiver_operation_status(
        &self,
        principal: crate::routing_operation::accounting::Principal,
        operation: tessera_control_types::ReceiverExpectation,
        retire: bool,
    ) -> Result<Reply, CommandError> {
        self.ask(|reply| Msg::ReceiverOperationStatus {
            principal,
            operation,
            retire,
            reply,
        })
        .await?
    }

    pub(crate) async fn runtime_carrier_recovery(
        &self,
        uid: u32,
        access: crate::peer::Access,
        review: Option<tessera_control_types::ReceiverExpectation>,
        next_run: bool,
    ) -> Result<Reply, CommandError> {
        if !access.can_mutate() {
            return Err(CommandError::PermissionDenied);
        }
        self.ask(|reply| Msg::RuntimeCarrierRecovery {
            uid,
            review,
            next_run,
            reply,
        })
        .await?
    }

    pub(crate) async fn receiver_recovery(
        &self,
        uid: u32,
        access: crate::peer::Access,
        operation: Option<tessera_control_types::ReceiverExpectation>,
        review: Option<tessera_control_types::ReceiverExpectation>,
    ) -> Result<Reply, CommandError> {
        if !access.can_mutate() {
            return Err(CommandError::PermissionDenied);
        }
        let _permit = self.receiver_reads.acquire(uid, access)?;
        let (snapshot, reviewed) = self
            .ask(|reply| Msg::ReceiverRecoveryStart {
                uid,
                operation,
                review,
                reply,
            })
            .await??;
        let deadline = Instant::now() + Duration::from_secs(3);
        let observed = snapshot.read(deadline).await?;
        self.ask(|reply| Msg::ReceiverRecoveryFinish {
            uid,
            snapshot: Box::new(snapshot),
            deadline,
            reviewed,
            observed,
            reply,
        })
        .await?
    }

    pub(crate) async fn receiver_dispatch(
        &self,
        job: Arc<crate::routing_operation::actor::Job>,
    ) -> Result<(), CommandError> {
        self.ask(|reply| Msg::ReceiverDispatch { job, reply })
            .await?
    }

    pub(crate) async fn receiver_finish(
        &self,
        job: Arc<crate::routing_operation::actor::Job>,
        outcome: crate::routing_operation::RunResult,
    ) -> Result<Reply, CommandError> {
        self.ask(|reply| Msg::ReceiverFinish {
            job,
            outcome,
            reply,
        })
        .await?
    }

    pub(crate) fn report_discovery(
        &self,
        snapshot: crate::routing_inventory::DiscoverySnapshot,
    ) -> bool {
        self.tx.try_send(Msg::ReportDiscovery(snapshot)).is_ok()
    }

    /// Look up a device by key, or the first device when `key` is `None`.
    ///
    /// # Errors
    /// `Internal` if the engine is unavailable.
    pub async fn device_info(
        &self,
        key: Option<String>,
    ) -> Result<Option<DeviceInfo>, CommandError> {
        self.ask(|reply| Msg::DeviceInfo { key, reply }).await
    }

    /// The current effective model and configuration.
    ///
    /// # Errors
    /// `Internal` if the engine is unavailable.
    pub async fn runtime_plan(&self) -> Result<RuntimePlan, CommandError> {
        self.ask(|reply| Msg::RuntimePlan { reply }).await
    }

    /// Report a clock observation (non-blocking; dropped if the queue is
    /// full, as the next observation supersedes it).
    pub fn report_clock(&self, status: ClockStatus) {
        let _ = self.tx.try_send(Msg::ReportClock(status));
    }

    /// Report the interfaces that run a PTP port now (non-blocking). `false`
    /// if the queue was full: the caller reports again on its next pass.
    pub fn report_clock_interfaces(&self, interfaces: Vec<String>) -> bool {
        self.tx.try_send(Msg::ClockInterfaces(interfaces)).is_ok()
    }

    /// Report what the reconciler found on the host (non-blocking). `false`
    /// if the queue was full: the caller reports again on its next pass.
    pub fn report_network(&self, status: NetworkStatus) -> bool {
        self.tx.try_send(Msg::ReportNetwork(status)).is_ok()
    }

    /// Report the bridges' status (non-blocking). `false` if the queue was
    /// full: the caller reports again shortly.
    pub fn report_bridges(&self, rows: Vec<BridgeStatus>) -> bool {
        self.tx.try_send(Msg::ReportBridges(rows)).is_ok()
    }

    /// Report one sweep of subscription states (non-blocking). `false` if the
    /// queue was full: the next sweep, a second later, supersedes it.
    pub fn report_subscriptions(&self, rows: Vec<(DeviceKey, Channel, SubscriptionState)>) -> bool {
        self.tx.try_send(Msg::ReportSubscriptions(rows)).is_ok()
    }

    /// Show a short-lived notice with the status warnings (non-blocking;
    /// `false` if the queue was full).
    pub fn report_notice(&self, field: &str, message: String, duration: Duration) -> bool {
        self.tx
            .try_send(Msg::ReportNotice {
                field: field.to_owned(),
                message,
                duration,
            })
            .is_ok()
    }

    /// Report states the media sweep cannot see (unresolved, refused,
    /// mismatched) for individual subscriptions, without the sweep's
    /// completeness rule. Non-blocking; `false` if the queue was full.
    pub fn report_subscription_states(
        &self,
        rows: Vec<(DeviceKey, Channel, SubscriptionState)>,
    ) -> bool {
        self.tx
            .try_send(Msg::ReportSubscriptionStates(rows))
            .is_ok()
    }

    /// Ask the thread to flush and stop; returns the flush error, if any.
    pub async fn shutdown(&self) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        // A blocking send is fine here: shutdown is not on a request path and
        // the thread drains its queue.
        let tx_chan = self.tx.clone();
        let sent = tokio::task::spawn_blocking(move || {
            tx_chan.send(Msg::Shutdown { reply: tx }).map_err(|_| ())
        })
        .await
        .ok()
        .and_then(Result::ok);
        sent?;
        rx.await.ok().flatten()
    }
}

/// Disarmed on a clean exit; otherwise cancels `fatal` when the thread ends.
struct FatalGuard {
    token: CancellationToken,
    armed: bool,
}

impl Drop for FatalGuard {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

/// Start the engine thread.
///
/// `clock` must be the clock the engine was started with: the persist
/// deadline is on its timeline.
///
/// # Errors
/// The thread could not be spawned.
pub fn spawn(
    engine: StateEngine,
    clock: Arc<MonotonicClock>,
    settings: watch::Sender<Settings>,
    fatal: CancellationToken,
    queue: usize,
    request_timeout: Duration,
) -> std::io::Result<(EngineHandle, JoinHandle<()>)> {
    #[cfg(feature = "routing-shadow")]
    let engine = {
        let mut engine = engine;
        let mut bytes = [0_u8; 16];
        match getrandom::fill(&mut bytes) {
            Ok(()) => {
                if !engine.enable_routing_shadow(u128::from_ne_bytes(bytes)) {
                    tracing::warn!("routing shadow disabled: context unavailable or over budget");
                }
            }
            Err(error) => {
                tracing::warn!("routing shadow disabled: cannot allocate instance: {error}")
            }
        }
        engine
    };
    let inventory = crate::routing_inventory::Inventory::new();
    let operations = crate::routing_operation::actor::Actor::new()?;
    let (tx, rx) = mpsc::sync_channel(queue.max(1));
    let pulse = Arc::new(Pulse::default());
    let thread_pulse = Arc::clone(&pulse);
    let join = std::thread::Builder::new()
        .name("tessera-state".into())
        .spawn(move || {
            let mut guard = FatalGuard {
                token: fatal,
                armed: true,
            };
            run(
                engine,
                inventory,
                operations,
                &clock,
                &rx,
                &settings,
                &thread_pulse,
            );
            guard.armed = false;
        })?;
    Ok((
        EngineHandle {
            tx,
            timeout: request_timeout,
            pulse,
            receiver_reads: Arc::default(),
        },
        join,
    ))
}

fn run(
    mut engine: StateEngine,
    mut inventory: crate::routing_inventory::Inventory,
    mut operations: crate::routing_operation::actor::Actor,
    clock: &MonotonicClock,
    rx: &mpsc::Receiver<Msg>,
    settings: &watch::Sender<Settings>,
    pulse: &Pulse,
) {
    use tessera_state::Clock as _;
    let mut shutdown_reply = None;
    #[cfg(feature = "routing-shadow")]
    let mut query_probe = query_probe::Probe::default();
    // What this process started with: restart-class keys that differ from it
    // are pending until the next service start.
    let started = engine.config().config.clone();
    let mut runtime_host = crate::runtime_host::Authority::default();
    loop {
        let wait = engine
            .next_persist_deadline()
            .map(|d| d.saturating_sub(clock.now()));
        #[cfg(feature = "routing-shadow")]
        let wait = wait
            .into_iter()
            .chain(
                query_probe
                    .next_deadline()
                    .map(|deadline| deadline.saturating_duration_since(Instant::now())),
            )
            .min();
        let msg = match wait {
            Some(t) => match rx.recv_timeout(t) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break,
            },
        };
        pulse.enter();
        runtime_host.retire_dead(&mut engine);
        operations.retire_undispatched(&engine);
        if let Some(m) = msg {
            match m {
                Msg::RuntimeStart { start, reply } => {
                    let result = operations.grant_start_ready(&engine).and_then(|()| {
                        runtime_host.register(&mut engine, start, operations.grant_epoch())
                    });
                    let _ = reply.send(result);
                }
                Msg::RuntimeReport {
                    evidence,
                    flows,
                    reply,
                } => {
                    let _ = reply.send(runtime_host.report(&mut engine, &evidence, flows));
                }
                Msg::RuntimeEcho { evidence, reply } => {
                    let _ = reply.send(runtime_host.echo(&mut engine, &evidence));
                }
                Msg::RuntimeReadStart { reply } => {
                    let _ = reply.send(runtime_host.begin_read(&mut engine));
                }
                Msg::RuntimeReadFinish {
                    job,
                    selections,
                    reply,
                } => {
                    let _ = reply.send(runtime_host.finish_read(&mut engine, job, selections));
                }
                Msg::ReceiverTarget { query, reply } => {
                    let _ = reply.send(inventory.receiver_target(&engine, &query.endpoint));
                }
                Msg::ReceiverResult {
                    principal,
                    target,
                    page,
                    deadline,
                    max_frame,
                    reply,
                } => {
                    let result = if Instant::now() >= deadline {
                        Err(CommandError::ReceiverReadTimeout)
                    } else {
                        inventory
                            .receiver_result(&engine, &target, page, max_frame)
                            .and_then(|mut result| {
                                if let Reply::RoutingReceiver(page) = &mut result {
                                    operations.observe(
                                        principal,
                                        &engine,
                                        &mut inventory,
                                        &runtime_host,
                                        &target,
                                        page,
                                    );
                                    crate::routing_receiver::check_response(page, max_frame)?;
                                }
                                Ok(result)
                            })
                    };
                    let _ = reply.send(result);
                }
                Msg::ReceiverOperation {
                    principal,
                    token,
                    change,
                    engine: handle,
                    executor,
                    reply,
                } => {
                    match operations.start(
                        principal,
                        &token,
                        change,
                        &engine,
                        &mut inventory,
                        &runtime_host,
                    ) {
                        Ok(crate::routing_operation::actor::Start::New(job)) => {
                            crate::routing_operation::live::launch(&executor, handle, job, reply);
                        }
                        Ok(crate::routing_operation::actor::Start::Existing(result)) => {
                            let _ = reply.send(Ok(Reply::ReceiverOperation(result)));
                        }
                        Err(error) => {
                            let _ = reply.send(Err(error));
                        }
                    }
                }
                Msg::ReceiverOperationStatus {
                    principal,
                    operation,
                    retire,
                    reply,
                } => {
                    let result = if retire {
                        operations.retire(&principal, &operation)
                    } else {
                        operations.status(&principal, &operation)
                    };
                    let _ = reply.send(result);
                }
                Msg::RuntimeCarrierRecovery {
                    uid,
                    review,
                    next_run,
                    reply,
                } => {
                    let result = if let Some(review) = review {
                        if next_run {
                            operations.grant_next_run(uid, &review, &mut engine, &runtime_host)
                        } else {
                            operations.release_carriers(uid, &review, &mut engine, &runtime_host)
                        }
                    } else {
                        operations.inspect_carriers(uid, &engine, &runtime_host)
                    };
                    let _ = reply.send(result);
                }
                Msg::ReceiverRecoveryStart {
                    uid,
                    operation,
                    review,
                    reply,
                } => {
                    let result = if let Some(token) = review {
                        operations
                            .reconcile_start(uid, &token, &engine, &runtime_host)
                            .map(|(snapshot, observed)| (snapshot, Some(observed)))
                    } else {
                        operations
                            .inspect_start(operation.as_ref(), &engine, &runtime_host)
                            .map(|snapshot| (snapshot, None))
                    };
                    let _ = reply.send(result);
                }
                Msg::ReceiverRecoveryFinish {
                    uid,
                    snapshot,
                    deadline,
                    reviewed,
                    observed,
                    reply,
                } => {
                    let result = if Instant::now() >= deadline {
                        Err(CommandError::ReceiverReadTimeout)
                    } else if let Some(reviewed) = reviewed {
                        operations.reconcile_finish(
                            *snapshot,
                            reviewed,
                            observed,
                            &mut engine,
                            &runtime_host,
                        )
                    } else {
                        operations.inspect_finish(uid, *snapshot, observed, &engine, &runtime_host)
                    };
                    let _ = reply.send(result);
                }
                Msg::ReceiverDispatch { job, reply } => {
                    let _ = reply.send(operations.dispatch(
                        &job,
                        &mut engine,
                        &mut inventory,
                        &runtime_host,
                    ));
                }
                Msg::ReceiverFinish {
                    job,
                    outcome,
                    reply,
                } => {
                    let _ = reply.send(operations.finish(
                        &job,
                        outcome,
                        &mut engine,
                        &mut inventory,
                        &runtime_host,
                    ));
                }
                #[cfg(all(test, feature = "routing-shadow"))]
                Msg::QueryProbeTest { reply } => {
                    query_probe.refresh(&engine, Instant::now());
                    let _ = reply.send(query_probe.test_state());
                }
                #[cfg(all(test, feature = "routing-shadow"))]
                Msg::QueryProbeFixture { group, reply } => {
                    query_probe.fixture_context(group);
                    let _ = reply.send(());
                }
                #[cfg(feature = "routing-shadow")]
                Msg::QuerySent {
                    tag,
                    original_success,
                } => {
                    query_probe.sent(tag, original_success, &engine, Instant::now());
                }
                #[cfg(feature = "routing-shadow")]
                Msg::QueryClosed(connection) => {
                    query_probe.closed(connection, &engine, Instant::now());
                }
                Msg::Apply {
                    origin,
                    cmd,
                    confirm,
                    protocol_evidence,
                    #[cfg(feature = "routing-shadow")]
                    connection,
                    #[cfg(test)]
                    observed,
                    reply,
                } => {
                    // Reobserve AFTER queue delay using the original retained
                    // socket-bound process handle and full kernel credentials.
                    // Even Live identifies only the connector, not its eventual
                    // byte writer or a registered runtime. No role is opened.
                    let connector = protocol_evidence
                        .as_ref()
                        .map_or(ConnectorObservation::Unavailable, ProtocolEvidence::observe);
                    if protocol_evidence.is_some() {
                        tracing::debug!(
                            ?connector,
                            holder_verified = false,
                            kind = cmd.kind(),
                            "state: protocol connector observation"
                        );
                    }
                    #[cfg(feature = "routing-shadow")]
                    let role = connection.as_ref().map(query_probe::Tag::observe_role);
                    #[cfg(feature = "routing-shadow")]
                    let operation = query_probe::Operation::of(&origin, &cmd, &confirm, &engine);
                    #[cfg(feature = "routing-shadow")]
                    let association = if operation == Some(query_probe::Operation::AdapterReport) {
                        connection
                            .as_ref()
                            .map(|tag| query_probe.associate(tag, &engine))
                            .unwrap_or(query_probe::Association::Unavailable)
                    } else {
                        query_probe::Association::Unavailable
                    };
                    #[cfg(test)]
                    if let Some(observed) = observed {
                        let _ = observed.send(connector);
                    }
                    #[cfg(feature = "routing-shadow")]
                    if protocol_evidence.is_some() && matches!(&origin, Origin::Protocol { .. }) {
                        if let Command::SetProtocolFlows { flows } = &cmd {
                            // No concrete launch association exists yet. Even a live
                            // connector is an UNVERIFIED proposal; do not evaluate
                            // it as a holder or invoke foreign-write semantics here.
                            engine.observe_routing_proposal(flows);
                        }
                    }
                    let is_reload = matches!(cmd, Command::ReloadConfig);
                    let r =
                        host_flow_write(&origin, &cmd, protocol_evidence.as_ref()).and_then(|()| {
                            match confirm {
                                Confirm::No => engine.apply(origin, cmd),
                                Confirm::Blind => engine.apply_confirmed(origin, cmd),
                                Confirm::Expecting(t) => engine.apply_expecting(origin, cmd, &t),
                            }
                        });
                    if is_reload && r.is_ok() {
                        // Resolve first (the user database may block): the watch
                        // channel's lock is held while the closure runs, and
                        // every connection reads it.
                        let new = Settings::of(&engine, &started);
                        engine.set_persist_delay(new.tunables.persist_delay);
                        engine.set_rate_limit(Some(new.tunables.rate));
                        settings.send_if_modified(|s| {
                            let changed = *s != new;
                            if changed {
                                *s = new;
                            }
                            changed
                        });
                    }
                    #[cfg(feature = "routing-shadow")]
                    let success = r.is_ok();
                    let delivered = reply.send(r).is_ok();
                    #[cfg(feature = "routing-shadow")]
                    if let Some(tag) = connection {
                        query_probe.applied(
                            tag,
                            operation,
                            query_probe::Receipt {
                                success,
                                delivered,
                                association,
                                role: role.unwrap_or(query_probe::RoleObservation::Pending),
                            },
                            &engine,
                            Instant::now(),
                        );
                    }
                    #[cfg(not(feature = "routing-shadow"))]
                    let _ = delivered;
                }
                #[cfg(test)]
                Msg::RoutingTest { event, reply } => {
                    // Same serialized owner; no apply, persistence or executor.
                    // A dropped caller does not cancel accounting or retry.
                    let _ = reply.send(engine.routing_shadow_test_event(event));
                }
                Msg::Query {
                    query,
                    max_frame,
                    #[cfg(feature = "routing-shadow")]
                    evidence,
                    reply,
                } => {
                    #[cfg(feature = "routing-shadow")]
                    let observation_query = query.clone();
                    #[cfg(feature = "routing-shadow")]
                    let role = evidence.as_ref().map(query_probe::Tag::observe_role);
                    let result =
                        if matches!(&query, Query::RoutingDevices(_) | Query::RoutingChannels(_)) {
                            inventory.query(&engine, query, max_frame)
                        } else {
                            engine.query(&query)
                        };
                    #[cfg(feature = "routing-shadow")]
                    let query_success = result.is_ok();
                    let delivered = reply.send(result).is_ok();
                    #[cfg(feature = "routing-shadow")]
                    if let Some(tag) = evidence {
                        query_probe.queried_role(
                            tag,
                            &observation_query,
                            query_probe::Receipt {
                                success: query_success,
                                delivered,
                                association: query_probe::Association::Unavailable,
                                role: role.unwrap_or(query_probe::RoleObservation::Pending),
                            },
                            &engine,
                            Instant::now(),
                        );
                    }
                    #[cfg(not(feature = "routing-shadow"))]
                    let _ = delivered;
                }
                Msg::ReportDiscovery(snapshot) => inventory.report(snapshot, &engine),
                Msg::DeviceInfo { key, reply } => {
                    let _ = reply.send(device_info(&engine, key.as_deref()));
                }
                Msg::RuntimePlan { reply } => {
                    let _ = reply.send(RuntimePlan {
                        model: engine.model().clone(),
                        config: engine.runtime_config(),
                    });
                }
                Msg::ReportClock(status) => engine.report_clock_status(status),
                Msg::ReportNetwork(status) => engine.report_network_status(status),
                Msg::ClockInterfaces(v) => engine.set_clock_interfaces(v),
                Msg::ReportBridges(rows) => engine.report_bridge_status(rows),
                Msg::ReportSubscriptions(rows) => engine.report_subscription_sweep(&rows),
                Msg::ReportNotice {
                    field,
                    message,
                    duration,
                } => engine.report_notice(&field, message, duration),
                Msg::ReportSubscriptionStates(rows) => {
                    for (device, channel, state) in rows {
                        // `NotFound`: removed since the planner looked.
                        let _ = engine.report_subscription_state(&device, channel, state, None);
                    }
                }
                Msg::Shutdown { reply } => {
                    shutdown_reply = Some(reply);
                    break;
                }
            }
        }
        #[cfg(feature = "routing-shadow")]
        query_probe.refresh(&engine, Instant::now());
        if let Err(e) = engine.poll_persist() {
            tracing::warn!("cannot write the state file, will retry: {e}");
        }
        pulse.leave();
    }
    let err = engine.flush().err().map(|e| {
        tracing::error!("final state flush failed: {e}");
        e.to_string()
    });
    if let Some(reply) = shutdown_reply {
        let _ = reply.send(err);
    }
}

#[cfg(feature = "routing-shadow")]
pub(crate) mod query_probe;

fn device_info(engine: &StateEngine, key: Option<&str>) -> Option<DeviceInfo> {
    let model = engine.model();
    let d = match key {
        Some(k) => model
            .device(k)
            .or_else(|| model.devices.iter().find(|d| d.name == k))?,
        None => model.devices.first()?,
    };
    Some(DeviceInfo {
        key: d.key.0.clone(),
        rx_channels: d.rx_channels,
        tx_channels: d.tx_channels,
        sample_rate: d.sample_rate,
        latency_us: d.latency_us,
        latency_max_us: d.latency_bounds_us[1].max(d.latency_us),
    })
}
