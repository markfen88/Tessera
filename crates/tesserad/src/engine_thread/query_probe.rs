//! Private query/transport observations on the existing serialized authority.
//! A completed sequence is not a launch witness, holder identity or permission.
use super::*;
use crate::platform::connector::launch;
use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU8};

#[derive(Debug)]
struct Facts {
    observation: launch::Observation,
    _retention: tokio::sync::OwnedSemaphorePermit,
}
impl std::ops::Deref for Facts {
    type Target = launch::Observation;
    fn deref(&self) -> &Self::Target {
        &self.observation
    }
}
type RoleFacts = Result<Arc<Facts>, launch::Failure>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RoleObservation {
    Pending,
    Observed(Result<launch::Image, launch::Failure>),
}
impl RoleObservation {
    fn result(self) -> Result<launch::Image, launch::Failure> {
        match self {
            Self::Pending => Err(launch::Failure::Unavailable),
            Self::Observed(result) => result,
        }
    }
}

// A stalled filesystem read can outlive the async observation deadline. Keep
// its owned permit in the worker so timeouts cannot create unbounded workers.
async fn collect(
    evidence: ProtocolEvidence,
    supplied: Option<Arc<launch::RoleTable>>,
) -> RoleFacts {
    static WORKERS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    static RETAINED: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    static ROLES: OnceLock<Result<launch::RoleTable, launch::Failure>> = OnceLock::new();
    let retention = RETAINED
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_RECORDS)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| launch::Failure::Capacity)?;
    let permit = WORKERS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| launch::Failure::Capacity)?;
    let job = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let roles = match supplied.as_deref() {
            Some(roles) => roles,
            None => ROLES
                .get_or_init(launch::RoleTable::installed)
                .as_ref()
                .map_err(|failure| *failure)?,
        };
        let process = evidence
            .connector
            .as_ref()
            .ok_or(launch::Failure::Unavailable)?;
        process
            .matches_credentials(&evidence.credentials)
            .map_err(|_| launch::Failure::Credentials)?;
        process.role_snapshot(roles).map(|observation| {
            Arc::new(Facts {
                observation,
                _retention: retention,
            })
        })
    });
    tokio::time::timeout(Duration::from_millis(300), job)
        .await
        .map_err(|_| launch::Failure::Deadline)?
        .map_err(|_| launch::Failure::Unavailable)?
}

const MAX_RECORDS: usize = 64;
const MAX_TEXT: usize = 4096;
const DEADLINE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub(crate) enum Termination {
    CleanEof = 1,
    Abandoned = 2,
    PartialEof = 3,
    ReadError = 4,
    Timeout = 5,
    Shutdown = 6,
    WriteFailed = 7,
}

#[derive(Debug)]
pub(crate) struct Connection {
    evidence: ProtocolEvidence,
    role: OnceLock<RoleFacts>,
    candidate: AtomicBool,
    frames: AtomicU64,
    // 0 open, otherwise a Termination discriminant. EOF includes half-close;
    // it never proves that all descriptor copies have been closed.
    end: AtomicU8,
    lost: AtomicBool,
}

#[derive(Debug, Clone)]
pub(crate) struct Tag {
    connection: Arc<Connection>,
    ordinal: u64,
}

/// Owned by one control task. A dropped event makes this connection's
/// diagnostics unavailable, without changing real command/query behavior.
pub(crate) struct Session {
    connection: Arc<Connection>,
    engine: EngineHandle,
    worker: Option<tokio::task::JoinHandle<()>>,
}
impl Session {
    pub(crate) fn new(engine: EngineHandle, evidence: ProtocolEvidence) -> Self {
        Self {
            engine,
            worker: None,
            connection: Arc::new(Connection {
                evidence,
                role: OnceLock::new(),
                candidate: AtomicBool::new(false),
                frames: AtomicU64::new(0),
                end: AtomicU8::new(0),
                lost: AtomicBool::new(false),
            }),
        }
    }
    /// Start once at a possible first frame. Status never waits for metadata.
    /// Only adapter/command forwarding and final lifecycle collection await it.
    pub(crate) fn start_facts(&mut self, #[cfg(test)] supplied: Option<Arc<launch::RoleTable>>) {
        if self.worker.is_some() || self.connection.role.get().is_some() {
            return;
        }
        self.connection.candidate.store(true, Ordering::SeqCst);
        let connection = self.connection.clone();
        self.worker = Some(tokio::spawn(async move {
            #[cfg(not(test))]
            let supplied = None;
            let result = collect(connection.evidence.clone(), supplied).await;
            let _ = connection.role.set(result);
        }));
    }
    pub(crate) async fn finish_facts(&mut self) {
        if let Some(worker) = self.worker.take() {
            if worker.await.is_err() {
                let _ = self.connection.role.set(Err(launch::Failure::Unavailable));
            }
        }
    }
    /// Count every request/response frame, including malformed/refused/rate-
    /// limited frames. Switching to event streaming abandons this sequence.
    pub(crate) fn frame(&self) -> Option<Tag> {
        let mut previous = self.connection.frames.load(Ordering::SeqCst);
        let ordinal = loop {
            let Some(next) = previous.checked_add(1) else {
                self.connection.lost.store(true, Ordering::SeqCst);
                return None;
            };
            match self.connection.frames.compare_exchange_weak(
                previous,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break next,
                Err(current) => previous = current,
            }
        };
        Some(Tag {
            connection: self.connection.clone(),
            ordinal,
        })
    }
    pub(crate) fn sent(&self, tag: Tag, original_success: bool) {
        if tag.ordinal > 2 || !self.connection.candidate.load(Ordering::SeqCst) {
            return;
        }
        if self
            .engine
            .send(Msg::QuerySent {
                tag,
                original_success,
            })
            .is_err()
        {
            self.connection.lost.store(true, Ordering::SeqCst);
        }
    }
    pub(crate) fn terminated(&self, reason: Termination) {
        self.connection.end.store(reason as u8, Ordering::SeqCst);
    }
}

impl Tag {
    pub(crate) fn is_first(&self) -> bool {
        self.ordinal == 1
    }
    /// Read-only queue-delay check, using retained directories. No subtree or
    /// installed-image lookup runs on the serialized state actor.
    pub(super) fn observe_role(&self) -> RoleObservation {
        if self.connection.role.get().is_none() {
            return RoleObservation::Pending;
        }
        RoleObservation::Observed(self.recheck_role())
    }
    fn recheck_role(&self) -> Result<launch::Image, launch::Failure> {
        let observation = self
            .connection
            .role
            .get()
            .ok_or(launch::Failure::Unavailable)?
            .as_ref()
            .map_err(|failure| *failure)?;
        let process = self
            .connection
            .evidence
            .connector
            .as_ref()
            .ok_or(launch::Failure::Unavailable)?;
        process
            .matches_credentials(&self.connection.evidence.credentials)
            .map_err(|_| launch::Failure::Credentials)?;
        process.recheck_role(observation)?;
        Ok(observation.snapshot.image_kind)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if !self.connection.candidate.load(Ordering::SeqCst) {
            return;
        }
        let _ = self.connection.end.compare_exchange(
            0,
            Termination::Abandoned as u8,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        if self
            .engine
            .send(Msg::QueryClosed(self.connection.clone()))
            .is_err()
        {
            self.connection.lost.store(true, Ordering::SeqCst);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Binding {
    device: String,
    wire_device_id: [u8; 8],
    interface: String,
    address: Ipv4Addr,
    mac: [u8; 6],
    rx: u16,
    tx: u16,
    rate: u32,
    encoding: u8,
    latency: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    Connector,
    Context,
    Capacity,
    Deadline,
    Binding,
    Sequence,
    QueryFailed,
    ActorReplyLost,
    ReplyNotWritten,
    Transport,
    EventLost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Operation {
    AdapterReport,
    ForwardedSubscription,
}
impl Operation {
    pub(super) fn of(
        origin: &Origin,
        command: &Command,
        confirm: &Confirm,
        engine: &StateEngine,
    ) -> Option<Self> {
        match (origin, command) {
            (Origin::Protocol { .. }, Command::SetProtocolFlows { .. }) => {
                Some(Self::AdapterReport)
            }
            (
                Origin::Cli { .. },
                Command::Subscribe { device, .. } | Command::Unsubscribe { device, .. },
            ) if *confirm != Confirm::No
                && engine
                    .model()
                    .devices
                    .first()
                    .is_some_and(|first| first.key == *device) =>
            {
                Some(Self::ForwardedSubscription)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sequence {
    Launcher,
    AdapterReport,
    ForwardedSubscription,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Classification {
    LauncherPair,
    AdapterReport,
    CliSubscription,
    Unclassified,
    Unavailable,
}

/// These unavailable premises are mandatory and cannot be cleared by this
/// diagnostic. In particular a matching ancestry observation is not review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MissingPremise {
    ManagerUnit,
    MainPidIncarnation,
    EffectiveUnitConfig,
    KillMode,
    PreExecOrdering,
    PriorRuntimeAbsence,
    ReviewedChain,
    DescriptorWriter,
    ReportCompleteness,
}
const MISSING: [MissingPremise; 9] = [
    MissingPremise::ManagerUnit,
    MissingPremise::MainPidIncarnation,
    MissingPremise::EffectiveUnitConfig,
    MissingPremise::KillMode,
    MissingPremise::PreExecOrdering,
    MissingPremise::PriorRuntimeAbsence,
    MissingPremise::ReviewedChain,
    MissingPremise::DescriptorWriter,
    MissingPremise::ReportCompleteness,
];
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Association {
    Unavailable,
    Candidate {
        instance: u128,
        revision: u64,
        missing: [MissingPremise; 9],
    },
}
#[derive(Debug, Clone, Copy)]
enum AssociationFailure {
    Ambiguous,
    Lifecycle,
    NoLauncher,
    Role,
    Context,
    Cgroup,
    Ancestry,
}
fn unavailable_association(reason: AssociationFailure) -> Association {
    tracing::debug!(
        ?reason,
        association_verified = false,
        "state: pre-Apply diagnostic association unavailable"
    );
    Association::Unavailable
}

pub(super) struct Receipt {
    pub success: bool,
    pub delivered: bool,
    pub role: RoleObservation,
    pub association: Association,
}

struct LauncherCandidate {
    process: crate::platform::connector::RetainedProcess,
    revision: u64,
    binding: Binding,
    cgroup: crate::platform::connector::ProcessKey,
    group: String,
    _cgroup_directory: std::fs::File,
}

/// Pure, deliberately narrower than every possible forwarded tesseractl
/// command. Other command shapes remain unclassified and execute normally.
fn classify(record: &Record) -> Classification {
    let count_matches = match record.sequence {
        Sequence::Launcher => record.queried == 2,
        Sequence::AdapterReport => record.queried == 1,
        Sequence::ForwardedSubscription => matches!(record.queried, 1 | 2),
    };
    if !record.closed
        || record.failure.is_some()
        || record.role.result().is_err()
        || !count_matches
        || record.queried != record.written
    {
        return Classification::Unavailable;
    }
    match (record.sequence, record.role.result()) {
        (Sequence::Launcher, Ok(launch::Image::Launcher)) => Classification::LauncherPair,
        (Sequence::AdapterReport, Ok(launch::Image::ReportAdapter)) => {
            Classification::AdapterReport
        }
        (Sequence::ForwardedSubscription, Ok(launch::Image::ForwardedCli)) => {
            Classification::CliSubscription
        }
        _ => Classification::Unclassified,
    }
}

fn binding(engine: &StateEngine) -> Result<Binding, Failure> {
    let first = engine.model().devices.first().ok_or(Failure::Binding)?;
    let network = engine.routing_query_network().ok_or(Failure::Binding)?;
    let primary = network.primary.as_deref().ok_or(Failure::Binding)?;
    // A new selected/configured primary may still be waiting for reconciler
    // correspondence. Equal query revisions alone cannot settle that gap.
    let configured = engine
        .config()
        .config
        .network
        .interfaces
        .first()
        .map(String::as_str);
    let expected = if engine.model().network.locked {
        configured
    } else {
        engine.model().network.selected.as_deref().or(configured)
    };
    if expected.is_some_and(|expected| expected != primary) {
        return Err(Failure::Binding);
    }
    let mut matching = network.interfaces.iter().filter(|i| i.name == primary);
    let interface = matching.next().ok_or(Failure::Binding)?;
    if matching.next().is_some() || interface.wireless || !interface.carrier {
        return Err(Failure::Binding);
    }
    let address = interface
        .ipv4
        .as_deref()
        .and_then(|s| s.parse::<Ipv4Addr>().ok())
        .ok_or(Failure::Binding)?;
    let mac_text = interface.mac.as_deref().ok_or(Failure::Binding)?;
    let mut parts = mac_text.split(':');
    let mut mac = [0; 6];
    for byte in &mut mac {
        let part = parts.next().ok_or(Failure::Binding)?;
        if part.len() != 2 {
            return Err(Failure::Binding);
        }
        *byte = u8::from_str_radix(part, 16).map_err(|_| Failure::Binding)?;
    }
    if parts.next().is_some() {
        return Err(Failure::Binding);
    }
    if first
        .key
        .0
        .len()
        .checked_add(primary.len())
        .is_none_or(|n| n > MAX_TEXT)
    {
        return Err(Failure::Capacity);
    }
    Ok(Binding {
        device: first.key.0.clone(),
        wire_device_id: first.wire_device_id.0,
        interface: primary.to_owned(),
        address,
        mac,
        rx: first.rx_channels,
        tx: first.tx_channels,
        rate: first.sample_rate,
        encoding: first.encoding,
        latency: first.latency_us,
    })
}

struct Record {
    instance: u128,
    connection_id: u64,
    connection: Arc<Connection>,
    start: Instant,
    revision: u64,
    binding: Binding,
    queried: u8,
    written: u8,
    closed: bool,
    failure: Option<Failure>,
    role: RoleObservation,
    sequence: Sequence,
    association: Association,
}

/// At most 64 connection records and 64 fact bundles, each with at most three
/// parent process/directory pairs. Binding text is at most 256 KiB; cgroup paths
/// at most 1 MiB. Records expire after ten seconds without renewal. One separate
/// launcher candidate retains a process and cgroup directory, binding and
/// revision until death/change (or rival ambiguity). The actor includes expiry
/// and candidate liveness in its existing wait.
/// No route content, authority, registration or persistent store is retained.
pub(super) struct Probe {
    instance: Option<u128>,
    next_connection: Option<u64>,
    records: VecDeque<Record>,
    dropped: u64,
    slot: Option<LauncherCandidate>,
    ambiguous_revision: Option<u64>,
    expected_cgroup: String,
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub connection: u64,
    pub queried: u8,
    pub written: u8,
    pub closed: bool,
    pub unavailable: bool,
    pub classification: Classification,
    pub association: Association,
}
impl Default for Probe {
    fn default() -> Self {
        let mut bytes = [0; 16];
        let instance = getrandom::fill(&mut bytes)
            .ok()
            .map(|()| u128::from_ne_bytes(bytes))
            .filter(|n| *n != 0);
        Self {
            instance,
            next_connection: Some(1),
            records: VecDeque::new(),
            dropped: 0,
            slot: None,
            ambiguous_revision: None,
            // Exact packaged default instance path, merely a diagnostic
            // expectation. Other unit layouts remain unavailable.
            expected_cgroup:
                "/system.slice/system-tessera\\x2dcontroller.slice/tessera-controller@main.service"
                    .into(),
        }
    }
}
impl Probe {
    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.records
            .front()
            .map(|record| record.start + DEADLINE)
            .into_iter()
            .chain(
                self.slot
                    .as_ref()
                    .map(|_| Instant::now() + Duration::from_secs(1)),
            )
            .min()
    }
    #[cfg(test)]
    pub(super) fn test_state(&self) -> Vec<Snapshot> {
        self.records
            .iter()
            .map(|r| Snapshot {
                connection: r.connection_id,
                queried: r.queried,
                written: r.written,
                closed: r.closed,
                unavailable: r.failure.is_some(),
                classification: classify(r),
                association: r.association,
            })
            .collect()
    }
    pub(super) fn refresh(&mut self, engine: &StateEngine, now: Instant) {
        if self.slot.as_ref().is_some_and(|slot| {
            engine.routing_query_revision() != Some(slot.revision)
                || slot.process.still_live().is_err()
        }) {
            self.slot = None;
        }
        if self
            .ambiguous_revision
            .is_some_and(|revision| engine.routing_query_revision() != Some(revision))
        {
            self.ambiguous_revision = None;
        }
        for record in &mut self.records {
            if record.closed || record.failure.is_some() {
                continue;
            }
            let failure = if record.connection.lost.load(Ordering::SeqCst) {
                Some(Failure::EventLost)
            } else if !matches!(
                record.connection.evidence.observe(),
                ConnectorObservation::Live(_)
            ) {
                Some(Failure::Connector)
            } else if engine.routing_query_revision() != Some(record.revision) {
                Some(Failure::Context)
            } else {
                None
            };
            record.failure = record.failure.or(failure);
        }
        while self
            .records
            .front()
            .is_some_and(|r| now.duration_since(r.start) >= DEADLINE)
        {
            if let Some(record) = self.records.pop_front() {
                Self::log(&record, Some(Failure::Deadline));
            }
        }
    }
    fn record_for(&mut self, tag: &Tag, engine: &StateEngine, now: Instant) -> Option<usize> {
        self.refresh(engine, now);
        let index = self
            .records
            .iter()
            .position(|r| Arc::ptr_eq(&r.connection, &tag.connection));
        let index = match index {
            Some(index) => index,
            None => {
                let (Some(instance), Some(connection_id)) = (self.instance, self.next_connection)
                else {
                    tag.connection.lost.store(true, Ordering::SeqCst);
                    return None;
                };
                if self.records.len() >= MAX_RECORDS {
                    tag.connection.lost.store(true, Ordering::SeqCst);
                    self.dropped = self.dropped.saturating_add(1);
                    tracing::debug!(
                        dropped = self.dropped,
                        association_verified = false,
                        "state: query candidate capacity unavailable"
                    );
                    return None;
                }
                let Some(revision) = engine.routing_query_revision() else {
                    tag.connection.lost.store(true, Ordering::SeqCst);
                    return None;
                };
                let binding = match binding(engine) {
                    Ok(binding) => binding,
                    Err(cause) => {
                        tag.connection.lost.store(true, Ordering::SeqCst);
                        tracing::debug!(
                            ?cause,
                            holder_verified = false,
                            "state: query context unavailable"
                        );
                        return None;
                    }
                };
                self.next_connection = connection_id.checked_add(1);
                self.records.push_back(Record {
                    instance,
                    connection_id,
                    connection: tag.connection.clone(),
                    start: now,
                    revision,
                    binding,
                    queried: 0,
                    written: 0,
                    closed: false,
                    failure: None,
                    role: RoleObservation::Pending,
                    sequence: Sequence::Launcher,
                    association: Association::Unavailable,
                });
                self.records.len() - 1
            }
        };
        Some(index)
    }
    #[cfg(test)]
    pub(super) fn queried(
        &mut self,
        tag: Tag,
        query: &Query,
        success: bool,
        actor_delivered: bool,
        engine: &StateEngine,
        now: Instant,
    ) {
        let role = tag.observe_role();
        self.queried_role(
            tag,
            query,
            Receipt {
                success,
                delivered: actor_delivered,
                role,
                association: Association::Unavailable,
            },
            engine,
            now,
        );
    }
    pub(super) fn queried_role(
        &mut self,
        tag: Tag,
        query: &Query,
        receipt: Receipt,
        engine: &StateEngine,
        now: Instant,
    ) {
        if !self
            .records
            .iter()
            .any(|r| Arc::ptr_eq(&r.connection, &tag.connection))
            && !(tag.ordinal == 1 && matches!(query, Query::Status))
        {
            return;
        }
        let Some(index) = self.record_for(&tag, engine, now) else {
            return;
        };
        let record = &mut self.records[index];
        Self::role(record, receipt.role);
        let expected = match query {
            Query::Status if record.queried == 0 && tag.ordinal == 1 => 1,
            Query::EffectiveConfig
                if record.queried == 1 && record.written == 1 && tag.ordinal == 2 =>
            {
                2
            }
            _ => {
                record.failure = record.failure.or(Some(Failure::Sequence));
                return;
            }
        };
        if !matches!(
            tag.connection.evidence.observe(),
            ConnectorObservation::Live(_)
        ) {
            record.failure = record.failure.or(Some(Failure::Connector));
        }
        if !receipt.success {
            record.failure = record.failure.or(Some(Failure::QueryFailed));
        }
        if !receipt.delivered {
            record.failure = record.failure.or(Some(Failure::ActorReplyLost));
        }
        record.queried = expected;
    }
    fn role(record: &mut Record, role: RoleObservation) {
        use RoleObservation::{Observed, Pending};
        record.role = match (record.role, role) {
            (Pending, role) => role,
            (old, Pending) => old,
            (Observed(Err(cause)), _) | (_, Observed(Err(cause))) => Observed(Err(cause)),
            (Observed(Ok(old)), Observed(Ok(new))) if old == new => Observed(Ok(old)),
            _ => Observed(Err(launch::Failure::Changed)),
        };
    }
    pub(super) fn applied(
        &mut self,
        tag: Tag,
        operation: Option<Operation>,
        receipt: Receipt,
        engine: &StateEngine,
        now: Instant,
    ) {
        if !self
            .records
            .iter()
            .any(|r| Arc::ptr_eq(&r.connection, &tag.connection))
            && !(tag.ordinal == 1 && operation.is_some())
        {
            return;
        }
        let Some(index) = self.record_for(&tag, engine, now) else {
            return;
        };
        let record = &mut self.records[index];
        Self::role(record, receipt.role);
        record.association = receipt.association;
        match operation {
            Some(Operation::AdapterReport) if tag.ordinal == 1 && record.queried == 0 => {
                record.sequence = Sequence::AdapterReport;
                record.queried = 1;
            }
            Some(Operation::ForwardedSubscription)
                if (tag.ordinal == 1 && record.queried == 0)
                    || (tag.ordinal == 2 && record.queried == 1 && record.written == 1) =>
            {
                record.sequence = Sequence::ForwardedSubscription;
                record.queried += 1;
            }
            _ => record.failure = record.failure.or(Some(Failure::Sequence)),
        }
        if !receipt.success {
            record.failure = record.failure.or(Some(Failure::QueryFailed));
        }
        if !receipt.delivered {
            record.failure = record.failure.or(Some(Failure::ActorReplyLost));
        }
    }
    pub(super) fn sent(&mut self, tag: Tag, success: bool, engine: &StateEngine, now: Instant) {
        self.refresh(engine, now);
        let Some(record) = self
            .records
            .iter_mut()
            .find(|r| Arc::ptr_eq(&r.connection, &tag.connection))
        else {
            return;
        };
        if !success {
            record.failure = record.failure.or(Some(Failure::ReplyNotWritten));
        } else if u64::from(record.queried) != tag.ordinal || record.written + 1 != record.queried {
            record.failure = record.failure.or(Some(Failure::Sequence));
        } else {
            record.written = record.queried;
        }
    }
    pub(super) fn closed(
        &mut self,
        connection: Arc<Connection>,
        engine: &StateEngine,
        now: Instant,
    ) {
        self.refresh(engine, now);
        let Some(index) = self
            .records
            .iter()
            .position(|r| Arc::ptr_eq(&r.connection, &connection))
        else {
            return;
        };
        let record = &mut self.records[index];
        if record.role == RoleObservation::Pending {
            // Collection may have finished after both queries. Recheck the
            // retained incarnation and image here before using its cached role.
            // Observed failures remain sticky, including a changed exec image.
            let tag = Tag {
                connection: connection.clone(),
                ordinal: connection.frames.load(Ordering::SeqCst),
            };
            Self::role(record, tag.observe_role());
        }
        record.closed = true;
        if connection.end.load(Ordering::SeqCst) != Termination::CleanEof as u8 {
            record.failure = record.failure.or(Some(Failure::Transport));
        } else if (record.sequence == Sequence::Launcher && record.queried != 2)
            || record.written != record.queried
            || connection.frames.load(Ordering::SeqCst) != u64::from(record.queried)
        {
            record.failure = record.failure.or(Some(Failure::Sequence));
        }
        Self::log(record, record.failure);
        if classify(record) == Classification::LauncherPair {
            self.seed_launcher(index);
        }
        if self.records[index].failure.is_some() {
            self.records.remove(index);
        }
    }

    fn seed_launcher(&mut self, index: usize) {
        let record = &self.records[index];
        if self.ambiguous_revision.is_some() {
            return;
        }
        let Some(Ok(facts)) = record.connection.role.get() else {
            return;
        };
        if facts.snapshot.unified_cgroup != self.expected_cgroup
            || !facts.snapshot.observed_subtree_singleton
        {
            return;
        }
        let Some(process) = record
            .connection
            .evidence
            .connector
            .as_ref()
            .and_then(|p| p.retain_process().ok())
        else {
            return;
        };
        let Ok(cgroup_directory) = facts.retain_cgroup() else {
            return;
        };
        if let Some(old) = &self.slot {
            if old.process.key() != process.key() {
                self.slot = None;
                self.ambiguous_revision = Some(record.revision);
            }
            return;
        }
        self.slot = Some(LauncherCandidate {
            process,
            revision: record.revision,
            binding: record.binding.clone(),
            cgroup: facts.snapshot.cgroup,
            group: facts.snapshot.unified_cgroup.clone(),
            _cgroup_directory: cgroup_directory,
        });
    }

    /// Runs before the existing Apply handler. This result has no conversion
    /// into a registration, grant, reducer holder or executable operation.
    pub(super) fn associate(&mut self, tag: &Tag, engine: &StateEngine) -> Association {
        self.refresh(engine, Instant::now());
        if self.ambiguous_revision.is_some() {
            return unavailable_association(AssociationFailure::Ambiguous);
        }
        if tag.ordinal != 1
            || tag.connection.frames.load(Ordering::SeqCst) != 1
            || tag.connection.lost.load(Ordering::SeqCst)
        {
            return unavailable_association(AssociationFailure::Lifecycle);
        }
        let (Some(instance), Some(slot), Some(Ok(facts))) =
            (self.instance, &self.slot, tag.connection.role.get())
        else {
            return unavailable_association(AssociationFailure::NoLauncher);
        };
        if tag.observe_role().result() != Ok(launch::Image::ReportAdapter) {
            return unavailable_association(AssociationFailure::Role);
        }
        if engine.routing_query_revision() != Some(slot.revision)
            || binding(engine).as_ref() != Ok(&slot.binding)
        {
            return unavailable_association(AssociationFailure::Context);
        }
        if facts.snapshot.cgroup != slot.cgroup || facts.snapshot.unified_cgroup != slot.group {
            return unavailable_association(AssociationFailure::Cgroup);
        }
        let Some(runtime) = facts.parents.last() else {
            return unavailable_association(AssociationFailure::Ancestry);
        };
        if runtime.process.key() != slot.process.key()
            || runtime.image_kind != launch::Image::Runtime
            || facts
                .parents
                .iter()
                .any(|p| p.unified_cgroup != slot.group || p.recheck().is_err())
        {
            return unavailable_association(AssociationFailure::Ancestry);
        }
        let association = Association::Candidate {
            instance,
            revision: slot.revision,
            missing: MISSING,
        };
        tracing::debug!(
            ?association,
            association_verified = false,
            "state: pre-Apply diagnostic association; all authority premises unavailable"
        );
        association
    }

    #[cfg(test)]
    pub(super) fn fixture_context(&mut self, group: String) {
        assert!(self.records.is_empty() && self.slot.is_none());
        self.expected_cgroup = group;
    }
    fn log(record: &Record, cause: Option<Failure>) {
        tracing::debug!(
            ?cause,
            instance = %record.instance,
            connection = record.connection_id,
            revision = record.revision,
            queried = record.queried,
            written = record.written,
            closed = record.closed,
            sequence = ?record.sequence,
            role = ?record.role,
            classification = ?classify(record),
            association = ?record.association,
            association_verified = false,
            termination = record.connection.end.load(Ordering::SeqCst),
            binding_text_bytes = record.binding.device.len() + record.binding.interface.len(),
            holder_verified = false,
            "state: query sequence observation; unit/absence/descriptor association unavailable"
        );
    }
}
