//! Explicit review and acceptance of fresh names. No mutation transport exists
//! on this path. Carrier ownership and vacancy fences are never transferred.
use super::actor::Actor;
use crate::{
    routing_receiver::{self, Target},
    runtime_host,
};
use std::{
    collections::BTreeMap,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
use tessera_control_types::{
    Channel, CommandError, ReceiverExpectation, ReceiverOperationResult, ReceiverOperationState,
    ReceiverRecoveryInspection, ReceiverRecoverySelection, Reply,
};
use tessera_state::{StateEngine, persist::StateFile};

const TTL: Duration = Duration::from_secs(30);
const QUIET: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Snapshot {
    target: Target,
    channels: u16,
    restore_observed: bool,
    operation_binding_matches: bool,
    context: (String, u64, u64),
    intent: StateFile,
    config: tessera_state::Config,
    device: tessera_state::model::DeviceModel,
}

pub(crate) type Start = (Snapshot, Option<Vec<ReceiverRecoverySelection>>);

#[derive(Debug)]
struct Review {
    uid: u32,
    created: Instant,
    snapshot: Snapshot,
    observed: Vec<ReceiverRecoverySelection>,
}

pub(crate) struct Reviews {
    namespace: String,
    serial: u64,
    started: Instant,
    rows: BTreeMap<String, Review>,
}

impl Reviews {
    pub(super) fn new(namespace: [u8; 16]) -> Self {
        Self {
            namespace: namespace.iter().map(|b| format!("{b:02x}")).collect(),
            serial: 0,
            started: Instant::now(),
            rows: BTreeMap::new(),
        }
    }

    fn issue(&mut self, review: Review) -> Result<ReceiverExpectation, CommandError> {
        self.rows
            .retain(|_, r| r.created.elapsed() < TTL && r.uid != review.uid);
        if self.rows.len() >= 32 {
            return Err(CommandError::RateLimited);
        }
        self.serial = self
            .serial
            .checked_add(1)
            .ok_or(CommandError::RateLimited)?;
        let token = ReceiverExpectation(format!("{}-r{:016x}", self.namespace, self.serial));
        self.rows.insert(token.0.clone(), review);
        Ok(token)
    }

    fn consume(&mut self, uid: u32, token: &ReceiverExpectation) -> Result<Review, CommandError> {
        if token.0.len() > 64 || self.rows.get(&token.0).is_none_or(|r| r.uid != uid) {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        let review = self.rows.remove(&token.0).expect("checked review");
        if review.created.elapsed() >= TTL {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        Ok(review)
    }
}

impl Snapshot {
    fn capture(
        engine: &StateEngine,
        runtime: &runtime_host::Authority,
    ) -> Result<Self, CommandError> {
        if !engine.receiver_operation_uncertain() {
            return Err(runtime_host::unavailable(
                "no unresolved receiver operation",
            ));
        }
        let runtime_host::RecoveryTarget {
            target,
            channels,
            context,
            restore_observed,
        } = runtime.recovery_target(engine)?;
        let device = engine
            .model()
            .devices
            .first()
            .cloned()
            .ok_or(CommandError::RoutingSnapshotExpired)?;
        let operation_binding_matches =
            engine
                .intent()
                .receiver_operation
                .as_ref()
                .is_none_or(|operation| {
                    operation.receiver_identity == device.wire_device_id.to_hex()
                        && operation.device == device.key
                        && operation.rx_channels == channels
                        && operation.sample_rate == device.sample_rate
                        && operation.encoding == device.encoding
                        && operation.interface == target.interface
                        && operation.address == target.source.to_string()
                });
        bounded(&(engine.intent(), &engine.config().config))?;
        Ok(Self {
            target,
            channels,
            context,
            restore_observed,
            operation_binding_matches,
            intent: engine.intent().clone(),
            config: engine.config().config.clone(),
            device,
        })
    }

    pub(crate) async fn read(
        &self,
        deadline: Instant,
    ) -> Result<Vec<ReceiverRecoverySelection>, CommandError> {
        read_complete(&mut LivePages, &self.target, self.channels, deadline).await
    }
}

// The absolute deadline and aggregate byte budget cover every page. Empty or
// incomplete reads can never become an accepted empty saved slot.
trait Pages {
    async fn page(
        &mut self,
        target: &Target,
        first: u16,
        deadline: Instant,
        budget: usize,
    ) -> Result<(tessera_control_types::RoutingReceiverPage, usize), CommandError>;
}
struct LivePages;
impl Pages for LivePages {
    async fn page(
        &mut self,
        target: &Target,
        first: u16,
        deadline: Instant,
        budget: usize,
    ) -> Result<(tessera_control_types::RoutingReceiverPage, usize), CommandError> {
        let page = routing_receiver::read(target, first, deadline, budget).await?;
        let size = page.wire_size();
        let Reply::RoutingReceiver(page) = page.into_reply(budget)? else {
            unreachable!()
        };
        Ok((page, size))
    }
}

async fn read_complete(
    reader: &mut impl Pages,
    target: &Target,
    channels: u16,
    deadline: Instant,
) -> Result<Vec<ReceiverRecoverySelection>, CommandError> {
    let mut first = 1;
    let mut rows = Vec::new();
    let mut budget: usize = 1024 * 1024;
    loop {
        if Instant::now() >= deadline {
            return Err(CommandError::ReceiverReadTimeout);
        }
        let (page, size) = tokio::time::timeout_at(
            deadline.into(),
            reader.page(target, first, deadline, budget),
        )
        .await
        .map_err(|_| CommandError::ReceiverReadTimeout)??;
        budget = budget
            .checked_sub(size)
            .ok_or_else(|| runtime_host::unavailable("complete recovery read exceeds budget"))?;
        if rows.len().saturating_add(page.channels.len()) > usize::from(channels) {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        rows.extend(
            page.channels
                .into_iter()
                .map(|r| ReceiverRecoverySelection {
                    rx: r.id,
                    source: r.source,
                    raw_status: r.raw_status,
                }),
        );
        match page.next_first {
            Some(next) if next > first => first = next,
            Some(_) => return Err(CommandError::RoutingSnapshotExpired),
            None => break,
        }
    }
    complete(&rows, channels)?;
    Ok(rows)
}

fn complete(rows: &[ReceiverRecoverySelection], channels: u16) -> Result<(), CommandError> {
    if channels == 0
        || rows.len() != usize::from(channels)
        || rows.iter().map(|r| r.rx).ne(1..=channels)
    {
        return Err(runtime_host::unavailable(
            "recovery requires every RX channel exactly once in order",
        ));
    }
    Ok(())
}

pub(super) fn bounded(value: &impl serde::Serialize) -> Result<(), CommandError> {
    struct Limit(usize);
    impl std::io::Write for Limit {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("recovery snapshot budget"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Limit(1024 * 1024), value)
        .map_err(|_| runtime_host::unavailable("recovery snapshot exceeds budget"))
}

fn worker_retired(exited: bool, deadline: Instant, now: Instant) -> bool {
    exited && now >= deadline + QUIET
}

fn validate_baseline(
    reviewed: &Snapshot,
    current: &Snapshot,
    before: &[ReceiverRecoverySelection],
    after: &[ReceiverRecoverySelection],
) -> Result<(), CommandError> {
    complete(after, reviewed.channels)?;
    if current != reviewed || before != after {
        return Err(CommandError::RoutingSnapshotExpired);
    }
    Ok(())
}

impl Actor {
    pub(super) fn retired(&self) -> Result<(), CommandError> {
        let now = Instant::now();
        if let Some(job) = &self.active_job {
            if !worker_retired(job.exited.load(Ordering::Acquire), job.deadline, now) {
                return Err(runtime_host::unavailable(
                    "operation worker is not retired or the quiet interval is incomplete",
                ));
            }
        } else if now.duration_since(self.recovery.started) < QUIET {
            return Err(runtime_host::unavailable(
                "restart quiet interval is incomplete",
            ));
        }
        Ok(())
    }

    pub(crate) fn inspect_start(
        &self,
        operation: Option<&ReceiverExpectation>,
        engine: &StateEngine,
        runtime: &runtime_host::Authority,
    ) -> Result<Snapshot, CommandError> {
        let snapshot = Snapshot::capture(engine, runtime)?;
        if operation.is_some()
            && snapshot
                .intent
                .receiver_operation
                .as_ref()
                .map(|c| &c.operation)
                != operation
        {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        Ok(snapshot)
    }

    pub(crate) fn inspect_finish(
        &mut self,
        uid: u32,
        snapshot: Snapshot,
        observed: Vec<ReceiverRecoverySelection>,
        engine: &StateEngine,
        runtime: &runtime_host::Authority,
    ) -> Result<Reply, CommandError> {
        if Snapshot::capture(engine, runtime)? != snapshot {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        complete(&observed, snapshot.channels)?;
        let unavailable = self
            .retired()
            .err()
            .map(|e| e.to_string())
            .or_else(|| {
                (!snapshot.restore_observed)
                    .then(|| "runtime startup restore has not been witnessed".into())
            })
            .or_else(|| {
                (!snapshot.operation_binding_matches).then(|| {
                    "original receiver identity/binding changed; explicit migration required".into()
                })
            })
            .or_else(|| {
                use tessera_state::config::ControlLock;
                engine
                    .config()
                    .config
                    .control
                    .locked
                    .iter()
                    .find(|lock| matches!(lock, ControlLock::Flows | ControlLock::Subscriptions))
                    .map(|lock| format!("routing lock {} is held", lock.as_str()))
            });
        let review = if unavailable.is_none() {
            Some(self.recovery.issue(Review {
                uid,
                created: Instant::now(),
                snapshot: snapshot.clone(),
                observed: observed.clone(),
            })?)
        } else {
            None
        };
        Ok(Reply::ReceiverRecovery(ReceiverRecoveryInspection {
            context: snapshot.intent.receiver_operation.clone(),
            legacy_context_unknown: snapshot.intent.receiver_operation.is_none(),
            receiver: snapshot.target.endpoint.clone(),
            label: snapshot.target.label.clone(),
            observed,
            saved: snapshot
                .device
                .subscriptions
                .iter()
                .map(|(channel, s)| ReceiverRecoverySelection {
                    rx: *channel,
                    source: Some(tessera_control_types::ReceiverSelection::new(
                        &s.tx_device,
                        &s.tx_channel,
                    )),
                    raw_status: 0,
                })
                .collect(),
            configured_carriers: engine.receiver_configured_carriers()?,
            stored_carriers: snapshot.intent.protocol_flows.clone().unwrap_or_default(),
            runtime_reports_fenced: snapshot.intent.runtime_reports_fenced,
            review,
            unavailable,
        }))
    }

    pub(crate) fn reconcile_start(
        &mut self,
        uid: u32,
        token: &ReceiverExpectation,
        engine: &StateEngine,
        runtime: &runtime_host::Authority,
    ) -> Result<(Snapshot, Vec<ReceiverRecoverySelection>), CommandError> {
        // Consumption precedes any async read; a lost response cannot reuse it.
        let review = self.recovery.consume(uid, token)?;
        self.retired()?;
        if !review.snapshot.restore_observed || !review.snapshot.operation_binding_matches {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        if Snapshot::capture(engine, runtime)? != review.snapshot {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        Ok((review.snapshot, review.observed))
    }

    pub(crate) fn reconcile_finish(
        &mut self,
        snapshot: Snapshot,
        reviewed: Vec<ReceiverRecoverySelection>,
        observed: Vec<ReceiverRecoverySelection>,
        engine: &mut StateEngine,
        runtime: &runtime_host::Authority,
    ) -> Result<Reply, CommandError> {
        self.retired()?;
        validate_baseline(
            &snapshot,
            &Snapshot::capture(engine, runtime)?,
            &reviewed,
            &observed,
        )?;
        let context = snapshot.intent.receiver_operation.as_ref();
        let operation = context.map(|c| &c.operation);
        let selections = observed
            .iter()
            .map(|r| {
                Ok((
                    Channel::new(r.rx).ok_or(CommandError::RoutingSnapshotExpired)?,
                    r.source.clone(),
                ))
            })
            .collect::<Result<Vec<_>, CommandError>>()?;
        engine.reconcile_receiver_mutation(operation, selections)?;
        self.table.reconcile(operation);
        self.recovery.rows.clear();
        let result = ReceiverOperationResult::new(operation.cloned().unwrap_or_else(|| ReceiverExpectation("legacy-context-unknown".into())),
            snapshot.target.endpoint, context.map_or(1, |c| c.affected_rx),
            ReceiverOperationState::Reconciled { original_unknown: "Original operation outcome remains unknown; fresh baseline accepted without mutation".into() });
        engine
            .event_bus()
            .publish(&tessera_control_types::Event::ReceiverOperationChanged {
                result: result.clone(),
            });
        Ok(Reply::ReceiverOperation(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tessera_control_types::{EndpointRef, RoutingReceiverChannel, RoutingReceiverPage};

    fn target() -> Target {
        Target {
            endpoint: EndpointRef::new("local", "run"),
            label: "Speaker".into(),
            interface: "audio0".into(),
            source: "192.0.2.2".parse().unwrap(),
            peer: "192.0.2.2:4440".parse().unwrap(),
            profile: tessera_control::arc::Legacy::from_advertised_version("2.7.41").unwrap(),
        }
    }
    fn row(rx: u16) -> ReceiverRecoverySelection {
        ReceiverRecoverySelection {
            rx,
            source: None,
            raw_status: 8,
        }
    }
    fn snapshot() -> Snapshot {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tessera-recovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        let (engine, _) = StateEngine::start(
            tessera_state::Environment::new("recovery-test")
                .with_ids(tessera_state::env::CountingIds::default()),
            tessera_state::Paths::new(
                tessera_state::ConfigPaths::new(path.join("config"), path.join("conf.d")),
                path.join("state"),
            ),
            std::sync::Arc::new(tessera_state::ManualClock::new()),
        )
        .unwrap();
        let snapshot = Snapshot {
            target: target(),
            channels: 2,
            restore_observed: true,
            operation_binding_matches: true,
            context: ("run".into(), 0, 0),
            intent: engine.intent().clone(),
            config: engine.config().config.clone(),
            device: engine.model().devices[0].clone(),
        };
        drop(engine);
        std::fs::remove_dir_all(path).unwrap();
        snapshot
    }

    #[test]
    fn review_tokens_are_principal_boot_episode_and_time_scoped_and_single_use() {
        let mut reviews = Reviews::new([1; 16]);
        let snapshot = snapshot();
        let issue = |snapshot: Snapshot| Review {
            uid: 7,
            created: Instant::now(),
            snapshot,
            observed: vec![row(1), row(2)],
        };
        let first = reviews.issue(issue(snapshot.clone())).unwrap();
        assert!(reviews.consume(8, &first).is_err());
        assert!(reviews.consume(7, &first).is_ok());
        assert!(reviews.consume(7, &first).is_err());
        let old = reviews.issue(issue(snapshot.clone())).unwrap();
        let fresh = reviews.issue(issue(snapshot.clone())).unwrap();
        assert!(reviews.consume(7, &old).is_err());
        assert!(Reviews::new([2; 16]).consume(7, &fresh).is_err());
        reviews.rows.get_mut(&fresh.0).unwrap().created = Instant::now() - TTL;
        assert!(reviews.consume(7, &fresh).is_err());
        let old = snapshot.clone();
        let mut new = snapshot.clone();
        new.intent.receiver_operation_uncertain = true;
        assert!(validate_baseline(&old, &new, &[row(1), row(2)], &[row(1), row(2)]).is_err());
    }

    #[test]
    fn acceptance_revalidates_names_status_slots_lifetime_host_revision_and_configuration() {
        let before = snapshot();
        let rows = vec![row(1), row(2)];
        assert!(validate_baseline(&before, &before, &rows, &rows).is_ok());
        let mut changed = rows.clone();
        changed[1].source = Some(tessera_control_types::ReceiverSelection::new("source", "B"));
        assert!(validate_baseline(&before, &before, &rows, &changed).is_err());
        changed = rows.clone();
        changed[1].raw_status = 1;
        assert!(validate_baseline(&before, &before, &rows, &changed).is_err());
        assert!(validate_baseline(&before, &before, &rows, &rows[..1]).is_err());
        for variant in 0..6 {
            let mut current = before.clone();
            match variant {
                0 => current.context.0 = "new-runtime".into(),
                1 => current.context.1 += 1, // byte-identical accepted host write
                2 => current.context.2 += 1, // a newer echo, including change-back
                3 => current.intent.runtime_reports_fenced = true,
                4 => {
                    current
                        .config
                        .control
                        .locked
                        .insert(tessera_state::config::ControlLock::Subscriptions);
                }
                _ => current.device.rx_channels += 1,
            }
            assert!(validate_baseline(&before, &current, &rows, &rows).is_err());
        }
    }

    #[test]
    fn elapsed_deadline_is_not_a_worker_retirement_fence_and_snapshot_size_is_bounded() {
        let now = Instant::now();
        assert!(!worker_retired(false, now - QUIET * 2, now));
        assert!(!worker_retired(true, now, now));
        assert!(worker_retired(true, now - QUIET, now));
        assert!(bounded(&"small").is_ok());
        assert!(bounded(&"\u{1}".repeat(200_000)).is_err());
    }

    #[test]
    fn restart_requires_quiet_interval_before_review_can_be_accepted() {
        let mut actor = Actor::new().unwrap();
        assert!(actor.retired().is_err());
        actor.recovery.started = Instant::now() - QUIET;
        assert!(actor.retired().is_ok());
    }

    struct FakePages {
        pages: VecDeque<Result<(RoutingReceiverPage, usize), CommandError>>,
        calls: Vec<u16>,
    }
    impl Pages for FakePages {
        async fn page(
            &mut self,
            _: &Target,
            first: u16,
            _: Instant,
            _: usize,
        ) -> Result<(RoutingReceiverPage, usize), CommandError> {
            self.calls.push(first);
            self.pages.pop_front().expect("unexpected extra query")
        }
    }
    fn page(ids: &[u16], next: Option<u16>) -> Result<(RoutingReceiverPage, usize), CommandError> {
        let mut page = RoutingReceiverPage::new(target().endpoint, "Speaker".into(), 1, 2);
        page.channels = ids
            .iter()
            .map(|id| RoutingReceiverChannel::new(*id, None, None, 8))
            .collect();
        page.next_first = next;
        Ok((page, 128))
    }

    #[tokio::test]
    async fn complete_read_paginates_once_and_refuses_partial_duplicate_and_failed_reads_without_retry()
     {
        let mut good = FakePages {
            pages: [page(&[1], Some(2)), page(&[2], None)].into(),
            calls: vec![],
        };
        assert_eq!(
            read_complete(&mut good, &target(), 2, Instant::now() + QUIET)
                .await
                .unwrap(),
            vec![row(1), row(2)]
        );
        assert_eq!(good.calls, [1, 2]);
        for pages in [
            vec![page(&[], None)],
            vec![page(&[1], None)],
            vec![page(&[1, 1], None)],
            vec![page(&[1], Some(1))],
            vec![Err(CommandError::ReceiverReadTimeout)],
        ] {
            let mut fake = FakePages {
                pages: pages.into(),
                calls: vec![],
            };
            assert!(
                read_complete(&mut fake, &target(), 2, Instant::now() + QUIET)
                    .await
                    .is_err()
            );
            assert_eq!(fake.calls, [1]);
        }
        let mut over = FakePages {
            pages: [Ok((page(&[1, 2], None).unwrap().0, 1024 * 1024 + 1))].into(),
            calls: vec![],
        };
        assert!(
            read_complete(&mut over, &target(), 2, Instant::now() + QUIET)
                .await
                .is_err()
        );
        assert_eq!(over.calls, [1]);
        let mut expired = FakePages {
            pages: VecDeque::new(),
            calls: vec![],
        };
        assert!(
            read_complete(&mut expired, &target(), 2, Instant::now())
                .await
                .is_err()
        );
        assert!(expired.calls.is_empty());
    }
}
