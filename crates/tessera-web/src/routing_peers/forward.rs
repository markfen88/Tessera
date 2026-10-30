//! Central-side reference translation and at-most-once forwarding.
use super::*;
use std::time::{Duration, Instant};
use tessera_control_types::{
    Command, Direction, EndpointRef, Query, ReceiverChange, ReceiverExpectation,
    ReceiverOperationResult, ReceiverOperationState, RoutingChannel, RoutingChannelsQuery,
    RoutingDevice, RoutingDevicesQuery, RoutingReceiverQuery, RoutingSourceRef,
};

#[derive(Debug, Clone)]
pub(super) struct Record {
    peer: config::Peer,
    receiver: EndpointRef,
    target: EndpointRef,
    target_token: ReceiverExpectation,
    rx: u16,
    created: Instant,
    used: bool,
    change: Option<ReceiverChange>,
    state: ReceiverOperationState,
    revision: u64,
    retired: bool,
}

struct Attempt<'a> {
    records: &'a Mutex<BTreeMap<String, Record>>,
    token: &'a str,
    completed: bool,
    revision: u64,
    mutation_started: bool,
}
impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if !self.completed {
            if let Ok(mut records) = self.records.lock() {
                if let Some(record) = records.get_mut(self.token) {
                    if record.revision == self.revision {
                        record.revision = record.revision.saturating_add(1);
                        record.state = if self.mutation_started {
                            ReceiverOperationState::Unknown { reason: "Portal request interrupted; inspect receiver-host accounting. No resend was made.".into() }
                        } else {
                            ReceiverOperationState::Refused { reason: "Portal preflight interrupted before forwarding; no mutation request sent.".into() }
                        };
                    }
                }
            }
        }
    }
}

fn unavailable(reason: &str) -> ClientError {
    ClientError::Command(CommandError::Unsupported {
        what: reason.into(),
    })
}
fn stale() -> ClientError {
    ClientError::Command(CommandError::RoutingSnapshotExpired)
}
fn remote(error: TransportError) -> ClientError {
    match error {
        TransportError::Refused(error) => ClientError::Command(error),
        TransportError::Unavailable => {
            ClientError::Command(CommandError::ReceiverReadUnavailable {
                reason: "receiver-host API unavailable or identity/TLS/credential checks failed"
                    .into(),
            })
        }
    }
}
fn reference(row: &Record, token: &str) -> Reply {
    Reply::ReceiverOperation(ReceiverOperationResult::new(
        ReceiverExpectation(token.into()),
        row.receiver.clone(),
        row.rx,
        row.state.clone(),
    ))
}

impl Federation {
    fn busy(record: &Record) -> bool {
        record.used
            && !record.retired
            && matches!(
                record.state,
                ReceiverOperationState::Pending | ReceiverOperationState::Unknown { .. }
            )
    }
    // Inventory pages are bounded and must all belong to one complete snapshot.
    pub(super) async fn devices(
        &self,
        st: &crate::AppState,
        peer: Option<&config::Peer>,
    ) -> Result<Vec<RoutingDevice>, ClientError> {
        let mut cursor = None;
        let mut devices = Vec::new();
        let mut snapshot = None;
        for _ in 0..16 {
            let query = RoutingDevicesQuery::new(cursor);
            let reply = match peer {
                Some(peer) => self
                    .transport
                    .exchange(peer, RoutingPeerRequest::Devices(query))
                    .await
                    .map_err(remote)?,
                None => {
                    crate::local_call(st, RequestBody::Query(Query::RoutingDevices(query))).await?
                }
            };
            let Reply::RoutingDevices(page) = reply else {
                return Err(stale());
            };
            if !page.complete || snapshot.as_ref().is_some_and(|s| s != &page.snapshot) {
                return Err(stale());
            }
            snapshot = Some(page.snapshot);
            devices.extend(page.devices);
            if devices.len() > 256 {
                return Err(unavailable("routing peer inventory is too large"));
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(devices);
            }
        }
        Err(unavailable(
            "routing peer inventory paging did not complete",
        ))
    }

    async fn channels(
        &self,
        st: &crate::AppState,
        peer: Option<&config::Peer>,
        endpoint: &EndpointRef,
    ) -> Result<Vec<RoutingChannel>, ClientError> {
        let mut cursor = None;
        let mut channels = Vec::new();
        let mut snapshot = None;
        for _ in 0..16 {
            let query = RoutingChannelsQuery::new(endpoint.clone(), Direction::Tx, cursor);
            let reply = match peer {
                Some(peer) => self
                    .transport
                    .exchange(peer, RoutingPeerRequest::Channels(query))
                    .await
                    .map_err(remote)?,
                None => {
                    crate::local_call(st, RequestBody::Query(Query::RoutingChannels(query))).await?
                }
            };
            let Reply::RoutingChannels(page) = reply else {
                return Err(stale());
            };
            if !page.complete
                || snapshot.as_ref().is_some_and(|s| s != &page.snapshot)
                || page
                    .channels
                    .iter()
                    .any(|c| &c.endpoint != endpoint || c.direction != Direction::Tx)
            {
                return Err(stale());
            }
            snapshot = Some(page.snapshot);
            channels.extend(page.channels);
            if channels.len() > 4096 {
                return Err(unavailable("routing peer channel inventory is too large"));
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(channels);
            }
        }
        Err(unavailable("routing peer channel paging did not complete"))
    }

    fn unique<'a>(
        devices: &'a [RoutingDevice],
        wire: &str,
    ) -> Result<&'a RoutingDevice, ClientError> {
        let mut matches = devices
            .iter()
            .filter(|d| d.wire_id.as_deref() == Some(wire));
        let device = matches.next().ok_or_else(stale)?;
        if matches.next().is_some() {
            return Err(unavailable("native wire identity is ambiguous"));
        }
        Ok(device)
    }

    async fn target(
        &self,
        st: &crate::AppState,
        peer: &config::Peer,
        receiver: &EndpointRef,
    ) -> Result<RoutingDevice, ClientError> {
        let central = self.devices(st, None).await?;
        if &Self::unique(&central, &peer.wire_id)?.endpoint != receiver {
            return Err(stale());
        }
        let inventory = self.devices(st, Some(peer)).await?;
        let target = Self::unique(&inventory, &peer.wire_id)?;
        if target.local_device.is_none() {
            return Err(unavailable(
                "configured peer does not own the selected receiver",
            ));
        }
        Ok(target.clone())
    }

    /// Intercept only configured remote receiver reads and our opaque tokens.
    /// The ordinary local socket remains the authority for every other request.
    pub(crate) async fn dispatch(
        &self,
        st: &crate::AppState,
        body: &RequestBody,
    ) -> Option<Result<Reply, ClientError>> {
        match body {
            RequestBody::Query(Query::RoutingReceiver(query)) if !self.config.peer.is_empty() => {
                let devices = match self.devices(st, None).await {
                    Ok(d) => d,
                    Err(_) => return None,
                };
                let device = devices.iter().find(|d| d.endpoint == query.endpoint)?;
                if device.local_device.is_some() {
                    return None;
                }
                let peer = self
                    .config
                    .peer
                    .iter()
                    .find(|p| device.wire_id.as_deref() == Some(p.wire_id.as_str()))?;
                Some(
                    tokio::time::timeout(Duration::from_secs(15), self.receiver(st, peer, query))
                        .await
                        .unwrap_or(Err(ClientError::Command(CommandError::ReceiverReadTimeout))),
                )
            }
            RequestBody::Command(Command::RouteReceiver {
                expectation,
                change,
            }) if expectation.0.starts_with("peer-") => Some(
                match tokio::time::timeout(
                    Duration::from_secs(15),
                    self.route(st, &expectation.0, change),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => self
                        .records
                        .lock()
                        .map_err(|_| stale())
                        .and_then(|records| {
                            records
                                .get(&expectation.0)
                                .map(|record| reference(record, &expectation.0))
                                .ok_or_else(stale)
                        }),
                },
            ),
            _ => None,
        }
    }

    async fn receiver(
        &self,
        st: &crate::AppState,
        peer: &config::Peer,
        query: &RoutingReceiverQuery,
    ) -> Result<Reply, ClientError> {
        let target = self.target(st, peer, &query.endpoint).await?;
        let reply = self
            .transport
            .exchange(
                peer,
                RoutingPeerRequest::Receiver(RoutingReceiverQuery::new(
                    target.endpoint.clone(),
                    query.first,
                )),
            )
            .await
            .map_err(remote)?;
        let Reply::RoutingReceiver(mut page) = reply else {
            return Err(stale());
        };
        if page.endpoint != target.endpoint || page.first != query.first {
            return Err(stale());
        }
        let mut records = self.records.lock().map_err(|_| stale())?;
        records.retain(|_, r| {
            if r.used {
                r.created.elapsed() < Duration::from_secs(1800)
                    || matches!(
                        r.state,
                        ReceiverOperationState::Unknown { .. } | ReceiverOperationState::Pending
                    )
            } else {
                r.created.elapsed() < Duration::from_secs(30)
            }
        });
        let blocked = records
            .values()
            .any(|record| record.peer.id == peer.id && Self::busy(record));
        if blocked {
            page.operation_unavailable = Some("An unresolved receiver-host submission must be inspected or retired before another change.".into());
        }
        for channel in &mut page.channels {
            if let Some(target_token) = channel.expectation.take() {
                if blocked {
                    continue;
                }
                if records.len() >= 256 {
                    page.operation_unavailable = Some(
                        "Peer correlation table is full; inspect unresolved operations.".into(),
                    );
                    continue;
                }
                let token = format!("peer-{}", crate::auth::random_hex(16));
                records.insert(
                    token.clone(),
                    Record {
                        peer: peer.clone(),
                        receiver: query.endpoint.clone(),
                        target: target.endpoint.clone(),
                        target_token,
                        rx: channel.id,
                        created: Instant::now(),
                        used: false,
                        change: None,
                        state: ReceiverOperationState::Pending,
                        revision: 0,
                        retired: false,
                    },
                );
                channel.expectation = Some(ReceiverExpectation(token));
            }
        }
        page.endpoint = query.endpoint.clone();
        Ok(Reply::RoutingReceiver(page))
    }

    async fn remap(
        &self,
        st: &crate::AppState,
        record: &Record,
        source: &RoutingSourceRef,
    ) -> Result<RoutingSourceRef, ClientError> {
        let central = self.devices(st, None).await?;
        let selected = central
            .iter()
            .find(|d| d.endpoint == source.endpoint)
            .ok_or_else(stale)?;
        let wire = selected
            .wire_id
            .as_deref()
            .ok_or_else(|| unavailable("source has no unambiguous native wire identity"))?;
        Self::unique(&central, wire)?;
        let peer_devices = self.devices(st, Some(&record.peer)).await?;
        let target_source = Self::unique(&peer_devices, wire)?;
        let channels = self.channels(st, None, &selected.endpoint).await?;
        let target_channels = self
            .channels(st, Some(&record.peer), &target_source.endpoint)
            .await?;
        let one = |items: Vec<RoutingChannel>| -> Result<RoutingChannel, ClientError> {
            let mut matches = items.into_iter().filter(|c| c.id == source.channel);
            let channel = matches.next().ok_or_else(stale)?;
            if matches.next().is_some()
                || channel.sample_rate.is_none()
                || channel.encoding.is_none()
            {
                return Err(stale());
            }
            Ok(channel)
        };
        let channel = one(channels)?;
        let target_channel = one(target_channels)?;
        if channel.sample_rate != target_channel.sample_rate
            || channel.encoding != target_channel.encoding
        {
            return Err(unavailable(
                "source format differs in receiver-host inventory",
            ));
        }
        Ok(RoutingSourceRef::new(
            target_source.endpoint.clone(),
            source.channel,
        ))
    }

    async fn prepare(
        &self,
        st: &crate::AppState,
        record: &Record,
        change: &ReceiverChange,
    ) -> Result<ReceiverChange, ClientError> {
        let target = self.target(st, &record.peer, &record.receiver).await?;
        if target.endpoint != record.target {
            return Err(stale());
        }
        match change {
            ReceiverChange::Connect { source } => Ok(ReceiverChange::Connect {
                source: self.remap(st, record, source).await?,
            }),
            ReceiverChange::Replace { previous, source } => Ok(ReceiverChange::Replace {
                previous: previous.clone(),
                source: self.remap(st, record, source).await?,
            }),
            ReceiverChange::Disconnect { previous } => Ok(ReceiverChange::Disconnect {
                previous: previous.clone(),
            }),
            _ => Err(unavailable("unsupported receiver operation")),
        }
    }

    async fn route(
        &self,
        st: &crate::AppState,
        token: &str,
        change: &ReceiverChange,
    ) -> Result<Reply, ClientError> {
        let record = {
            let mut records = self.records.lock().map_err(|_| stale())?;
            let existing = records.get(token).ok_or_else(stale)?;
            if existing.used {
                if existing.change.as_ref() != Some(change) {
                    return Err(stale());
                }
                return Ok(reference(existing, token));
            }
            let peer = existing.peer.id.clone();
            if records
                .iter()
                .any(|(key, record)| key != token && record.peer.id == peer && Self::busy(record))
            {
                return Err(ClientError::Command(CommandError::RateLimited));
            }
            let record = records.get_mut(token).ok_or_else(stale)?;
            if record.created.elapsed() >= Duration::from_secs(30) {
                return Err(stale());
            }
            // Consume before any await. Duplicate submissions cannot send twice.
            record.used = true;
            record.revision = record.revision.checked_add(1).ok_or_else(stale)?;
            record.change = Some(change.clone());
            record.clone()
        };
        let mut attempt = Attempt {
            records: &self.records,
            token,
            completed: false,
            revision: record.revision,
            mutation_started: false,
        };
        let state = match self.prepare(st, &record, change).await {
            Err(error) => ReceiverOperationState::Refused {
                reason: crate::explain(&error),
            },
            Ok(change) => {
                let request = RoutingPeerRequest::Route {
                    expectation: record.target_token.clone(),
                    change,
                };
                attempt.mutation_started = true;
                match self.transport.exchange(&record.peer, request).await {
                    Ok(Reply::ReceiverOperation(result)) if result.operation == record.target_token && result.receiver == record.target && result.rx_channel == record.rx => result.state,
                    _ => ReceiverOperationState::Unknown { reason: "Receiver-host reply lost or uncorrelated. No resend was made. Inspect operation status or open recovery on the receiver host.".into() },
                }
            }
        };
        let mut records = self.records.lock().map_err(|_| stale())?;
        let row = records.get_mut(token).ok_or_else(stale)?;
        if row.revision == record.revision {
            row.revision = row.revision.checked_add(1).ok_or_else(stale)?;
            row.state = state;
        }
        attempt.completed = true;
        Ok(reference(row, token))
    }

    pub(crate) async fn retire(&self, token: &str) -> Result<Reply, ClientError> {
        let record = self
            .records
            .lock()
            .map_err(|_| stale())?
            .get(token)
            .cloned()
            .ok_or_else(stale)?;
        if !record.used {
            return Err(stale());
        }
        let response = self
            .transport
            .exchange(
                &record.peer,
                RoutingPeerRequest::Retire {
                    operation: record.target_token.clone(),
                },
            )
            .await
            .map_err(remote)?;
        let state = match response {
            Reply::Done => None,
            Reply::ReceiverOperation(result)
                if result.operation == record.target_token
                    && result.receiver == record.target
                    && result.rx_channel == record.rx =>
            {
                Some(result.state)
            }
            _ => return Err(stale()),
        };
        let mut records = self.records.lock().map_err(|_| stale())?;
        let row = records.get_mut(token).ok_or_else(stale)?;
        if row.revision == record.revision {
            row.revision = row.revision.checked_add(1).ok_or_else(stale)?;
            match state {
                Some(state) => {
                    row.retired = !matches!(
                        state,
                        ReceiverOperationState::Pending | ReceiverOperationState::Unknown { .. }
                    );
                    row.state = state;
                }
                None => {
                    row.retired = true;
                    row.state = ReceiverOperationState::Unknown { reason: "Original outcome remains unknown. The receiver host has retired this submission; it cannot execute later. Retirement sent no routing mutation.".into() };
                }
            }
        }
        Ok(reference(row, token))
    }

    pub(crate) async fn operation(
        &self,
        st: &crate::AppState,
        token: &str,
    ) -> Result<Reply, ClientError> {
        let record = self
            .records
            .lock()
            .map_err(|_| stale())?
            .get(token)
            .cloned()
            .ok_or_else(stale)?;
        if !record.used {
            return Err(stale());
        }
        // Status is read-only; an unreachable/restarted peer retains Unknown.
        let _ = st;
        let result = self
            .transport
            .exchange(
                &record.peer,
                RoutingPeerRequest::Operation {
                    operation: record.target_token.clone(),
                },
            )
            .await;
        if let Ok(Reply::ReceiverOperation(result)) = result {
            if result.operation == record.target_token
                && result.receiver == record.target
                && result.rx_channel == record.rx
            {
                if let Some(row) = self.records.lock().map_err(|_| stale())?.get_mut(token) {
                    if row.revision == record.revision && row.state != result.state {
                        row.revision = row.revision.checked_add(1).ok_or_else(stale)?;
                        row.state = result.state;
                    }
                    return Ok(reference(row, token));
                }
            }
        }
        Ok(reference(&record, token))
    }
}
