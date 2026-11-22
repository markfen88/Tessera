//! On-demand receiver reads. No subscription write can be built or sent here.
//! Targets come from the actor's inventory; network waits stay off the actor.
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tessera_control::arc;
use tessera_control_types::{
    CommandError, EndpointRef, ReceiverSelection, Reply, RoutingReceiverChannel,
    RoutingReceiverPage,
};

/// Two ordinary reads and one reserved administrator slot, at most one per uid.
#[derive(Debug, Default)]
pub(crate) struct ReadLimits(Mutex<std::collections::BTreeMap<u32, crate::peer::Access>>);

impl ReadLimits {
    pub(crate) fn acquire(
        self: &Arc<Self>,
        uid: u32,
        access: crate::peer::Access,
    ) -> Result<ReadPermit, CommandError> {
        let mut slots = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ordinary = slots.values().filter(|a| !a.can_mutate()).count();
        if slots.contains_key(&uid) || slots.len() >= 3 || (!access.can_mutate() && ordinary >= 2) {
            return Err(CommandError::RateLimited);
        }
        slots.insert(uid, access);
        Ok(ReadPermit {
            limits: self.clone(),
            uid,
        })
    }
}

pub(crate) struct ReadPermit {
    limits: Arc<ReadLimits>,
    uid: u32,
}
impl Drop for ReadPermit {
    fn drop(&mut self) {
        self.limits
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.uid);
    }
}

static SEQUENCE: AtomicU16 = AtomicU16::new(1);
fn next_sequence(counter: &AtomicU16) -> u16 {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let next = if current == u16::MAX { 1 } else { current + 1 };
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(value) => return value,
            Err(value) => current = value,
        }
    }
}

pub(crate) fn sequence() -> u16 {
    next_sequence(&SEQUENCE)
}

pub(crate) fn check_response(page: &RoutingReceiverPage, max: usize) -> Result<(), CommandError> {
    encoded_size(page, max).map(|_| ())
}

/// Make this host's completed page fit `max`: rows from the end move to the
/// next page (`next_first`, the page index of this host's contiguous rows)
/// until it fits. Too large only when not even one row fits.
pub(crate) fn fit_response(page: &mut RoutingReceiverPage, max: usize) -> Result<(), CommandError> {
    let Err(too_large) = encoded_size(page, max) else {
        return Ok(());
    };
    let rows = std::mem::take(&mut page.channels);
    // The first `n` rows, the rest from row `n` on the next page.
    let take = |page: &mut RoutingReceiverPage, n: usize| {
        page.channels = rows.get(..n).unwrap_or_default().to_vec();
        page.next_first = rows.get(n).map(|r| r.id);
        encoded_size(page, max).is_ok()
    };
    // The most rows that fit (fewer than all: all did not).
    let (mut lo, mut hi) = (0, rows.len().saturating_sub(1));
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if take(page, mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 || !take(page, lo) {
        return Err(too_large);
    }
    Ok(())
}

/// Construction is private: no unbounded receiver page can reach the actor.
#[derive(Debug, Clone)]
pub(crate) struct PreparedPage {
    page: RoutingReceiverPage,
    wire_size: usize,
}
impl PreparedPage {
    pub(crate) fn wire_size(&self) -> usize {
        self.wire_size
    }
    pub(crate) fn into_reply(self, limit: usize) -> Result<Reply, CommandError> {
        if self.wire_size > limit {
            return Err(too_large(self.wire_size, limit));
        }
        Ok(Reply::RoutingReceiver(self.page))
    }
    /// The receiver device's access lock as read ([`lock_state`]); its few
    /// bytes are within the reserve `page` keeps for the actor's fields.
    pub(crate) fn with_access_locked(mut self, locked: Option<bool>) -> Self {
        self.page.access_locked = locked;
        self
    }

    #[cfg(test)]
    pub(crate) fn checked(page: RoutingReceiverPage) -> Self {
        let wire_size = encoded_size(&page, usize::MAX).unwrap();
        Self { page, wire_size }
    }
}

fn too_large(size: usize, limit: usize) -> CommandError {
    CommandError::ResponseTooLarge {
        size: size as u64,
        limit: limit as u64,
        key: "control.max_frame_bytes".into(),
    }
}

/// Count the exact envelope without a JSON buffer, stopping at the first excess.
/// `size` on rejection is a lower bound; no rejected name is copied into a String.
fn encoded_size(page: &impl serde::Serialize, limit: usize) -> Result<usize, CommandError> {
    #[derive(serde::Serialize)]
    struct Envelope<T> {
        v: u32,
        id: u64,
        result: Success<T>,
    }
    #[derive(serde::Serialize)]
    struct Success<T> {
        ok: Receiver<T>,
    }
    #[derive(serde::Serialize)]
    struct Receiver<T> {
        routing_receiver: T,
    }
    struct Count {
        size: usize,
        limit: usize,
    }
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.size = self.size.saturating_add(bytes.len());
            if self.size > self.limit {
                return Err(std::io::Error::other("receiver response limit"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count { size: 0, limit };
    let result = serde_json::to_writer(
        &mut count,
        &Envelope {
            v: tessera_control_types::WIRE_VERSION,
            id: u64::MAX,
            result: Success {
                ok: Receiver {
                    routing_receiver: page,
                },
            },
        },
    );
    if count.size > limit {
        return Err(too_large(count.size, limit));
    }
    result.map_err(failed)?;
    Ok(count.size)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub endpoint: EndpointRef,
    pub label: String,
    pub interface: String,
    pub source: Ipv4Addr,
    pub peer: SocketAddrV4,
    pub profile: arc::Legacy,
}

pub(crate) fn unsupported(what: &str) -> CommandError {
    CommandError::Unsupported { what: what.into() }
}

fn failed(what: impl std::fmt::Display) -> CommandError {
    CommandError::ReceiverReadUnavailable {
        reason: what.to_string(),
    }
}

trait Exchange {
    async fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, CommandError>;
}

async fn page(
    target: &Target,
    first: u16,
    sequence: u16,
    max_frame: usize,
    exchange: &mut impl Exchange,
) -> Result<PreparedPage, CommandError> {
    let mut request = [0; 16];
    // One sequence on one ephemeral connected socket (a retry is another
    // socket and sequence, see [`READ_RETRY_AFTER`]); no version fallback.
    let request = target
        .profile
        .rx_list_request(sequence, first, 0, &mut request)
        .map_err(failed)?;
    let bytes = exchange.exchange(request).await?;
    let reply = target
        .profile
        .reply_page(sequence, 0x3000, &bytes)
        .map_err(failed)?;
    let page = reply.rx_channels().map_err(failed)?;
    let mut result = RoutingReceiverPage::new(
        target.endpoint.clone(),
        target.label.clone(),
        first,
        page.slots(),
    );
    if page.has_more() {
        let count = u16::try_from(page.len()).map_err(failed)?;
        if count == 0 {
            return Err(failed("continued page made no progress"));
        }
        result.next_first = Some(
            first
                .checked_add(count)
                .ok_or_else(|| failed("page index overflow"))?,
        );
    }
    // Only borrowed views and protocol-bounded row metadata exist until the
    // complete escaped JSON envelope fits the caller's actual frame budget.
    #[derive(serde::Serialize)]
    struct Source<'a> {
        tx_device: &'a str,
        tx_channel: &'a str,
    }
    #[derive(serde::Serialize)]
    struct Row<'a> {
        id: u16,
        label: Option<&'a str>,
        source: Option<Source<'a>>,
        raw_status: u32,
    }
    #[derive(serde::Serialize)]
    struct Page<'a> {
        endpoint: &'a EndpointRef,
        label: &'a str,
        first: u16,
        raw_slots: u8,
        channels: Vec<Row<'a>>,
        next_first: Option<u16>,
    }
    let borrowed = Page {
        endpoint: &result.endpoint,
        label: &result.label,
        first,
        raw_slots: result.raw_slots,
        next_first: result.next_first,
        channels: page
            .records()
            .map(|r| Row {
                id: r.id,
                label: r.label,
                source: r.source.map(|s| Source {
                    tx_device: s.device,
                    tx_channel: s.channel,
                }),
                raw_status: r.raw_status,
            })
            .collect(),
    };
    let wire_size = encoded_size(&borrowed, max_frame)?;
    // Reserve actor-issued tokens and a bounded eligibility explanation before
    // copying any datagram names. These fields cannot expand a rejected page.
    let reserve = page.len().saturating_mul(96).saturating_add(512);
    if wire_size.saturating_add(reserve) > max_frame {
        return Err(too_large(wire_size.saturating_add(reserve), max_frame));
    }
    result.channels = page
        .records()
        .map(|record| {
            RoutingReceiverChannel::new(
                record.id,
                record.label.map(str::to_owned),
                record
                    .source
                    .map(|s| ReceiverSelection::new(s.device, s.channel)),
                record.raw_status,
            )
        })
        .collect();
    Ok(PreparedPage {
        page: result,
        wire_size,
    })
}

struct Socket(tokio::net::UdpSocket);
impl Exchange for Socket {
    async fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, CommandError> {
        if self.0.send(request).await.map_err(failed)? != request.len() {
            return Err(failed("partial request send; no retry"));
        }
        let mut bytes = vec![0; 65535];
        let count = self.0.recv(&mut bytes).await.map_err(failed)?;
        bytes.truncate(count);
        Ok(bytes)
    }
}

fn on_link(source: Ipv4Addr, peer: Ipv4Addr, mask: Ipv4Addr) -> bool {
    let mask = u32::from(mask);
    let prefix = mask.leading_ones();
    if prefix == 0
        || prefix != mask.count_ones()
        || u32::from(source) & mask != u32::from(peer) & mask
    {
        return false;
    }
    let host = u32::from(peer) & !mask;
    prefix >= 31 || (host != 0 && host != !mask)
}

fn scope(target: &Target) -> Result<Ipv4Addr, CommandError> {
    nix::ifaddrs::getifaddrs()
        .map_err(failed)?
        .find_map(|ifa| {
            if ifa.interface_name != target.interface
                || ifa.address?.as_sockaddr_in()?.ip() != target.source
            {
                return None;
            }
            Some(ifa.netmask?.as_sockaddr_in()?.ip())
        })
        .filter(|mask| on_link(target.source, *target.peer.ip(), *mask))
        .ok_or_else(|| {
            unsupported("receiver address is not a unicast neighbour on the selected interface")
        })
}

pub(crate) struct OperationSocket {
    socket: Socket,
    target: Target,
    index: u32,
    mask: Ipv4Addr,
}

impl OperationSocket {
    pub(crate) async fn connect(target: &Target) -> Result<Self, CommandError> {
        let interface = crate::netif::lookup(&target.interface).map_err(failed)?;
        if interface.addr != target.source {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        let mask = scope(target)?;
        let socket =
            std::net::UdpSocket::bind(SocketAddrV4::new(target.source, 0)).map_err(failed)?;
        nix::sys::socket::setsockopt(
            &socket,
            nix::sys::socket::sockopt::BindToDevice,
            &std::ffi::OsString::from(&target.interface),
        )
        .map_err(failed)?;
        socket.set_nonblocking(true).map_err(failed)?;
        let socket = tokio::net::UdpSocket::from_std(socket).map_err(failed)?;
        socket.connect(target.peer).await.map_err(failed)?;
        Ok(Self {
            socket: Socket(socket),
            target: target.clone(),
            index: interface.index,
            mask,
        })
    }

    fn check(&self) -> Result<(), CommandError> {
        let now = crate::netif::lookup(&self.target.interface).map_err(failed)?;
        if now.index != self.index
            || now.addr != self.target.source
            || scope(&self.target)? != self.mask
        {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        Ok(())
    }
}

impl crate::routing_operation::Transport for OperationSocket {
    fn sequence(&mut self) -> u16 {
        sequence()
    }
    async fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, CommandError> {
        self.check()?;
        let reply = self.socket.exchange(request).await?;
        self.check()?;
        Ok(reply)
    }
}

/// A read that got no answer is asked once more, with a fresh socket and
/// sequence, this long after the first request (within the same deadline):
/// one lost datagram must not fail the read. A read changes nothing on the
/// receiver, so asking again is safe; the vendor controller also repeats a
/// whole query round once, 1.1-1.3 s later (`control-arc.md` §2.1).
pub(crate) const READ_RETRY_AFTER: std::time::Duration = std::time::Duration::from_millis(1200);

/// This host's own receiver, when the daemon serves it (the native device
/// role): the page is answered from the live state in process (no
/// advertisement or UDP round trip), byte for byte as the ARC listener
/// would, so expectations issued on it match what a write reads.
struct InProcess<'a>(&'a crate::engine_thread::EngineHandle);
impl Exchange for InProcess<'_> {
    async fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, CommandError> {
        // Boxed: the state read goes through the engine's query path,
        // which leads back here for a receiver query.
        Box::pin(crate::native::local_read(self.0, request))
            .await
            .ok_or_else(|| failed("this host's receiver state is unavailable"))
    }
}

/// Read a page of the receiver this daemon serves itself (see
/// [`crate::native::serves`]).
pub(crate) async fn read_local(
    engine: &crate::engine_thread::EngineHandle,
    target: &Target,
    first: u16,
    deadline: Instant,
    max_frame: usize,
) -> Result<PreparedPage, CommandError> {
    tokio::time::timeout_at(
        deadline.into(),
        page(
            target,
            first,
            next_sequence(&SEQUENCE),
            max_frame,
            &mut InProcess(engine),
        ),
    )
    .await
    .map_err(|_| CommandError::ReceiverReadTimeout)?
}

/// Run `attempt` until [`READ_RETRY_AFTER`]; when it got no answer and the
/// deadline is not reached, once more until the deadline (a late answer to
/// the first request goes to its closed socket).
async fn with_retry<F, Fut>(deadline: Instant, mut attempt: F) -> Result<PreparedPage, CommandError>
where
    F: FnMut(Instant) -> Fut,
    Fut: std::future::Future<Output = Result<PreparedPage, CommandError>>,
{
    let retry_at = Instant::now() + READ_RETRY_AFTER;
    match attempt(retry_at.min(deadline)).await {
        Err(CommandError::ReceiverReadTimeout) if Instant::now() < deadline => {
            tracing::debug!(
                "receiver read: no answer within {} ms, asking once more",
                READ_RETRY_AFTER.as_millis()
            );
            attempt(deadline).await
        }
        r => r,
    }
}

/// One request on its own socket, answered by `until`.
#[cfg(target_os = "linux")]
async fn attempt(
    target: &Target,
    first: u16,
    until: Instant,
    max_frame: usize,
) -> Result<PreparedPage, CommandError> {
    let socket = std::net::UdpSocket::bind(SocketAddrV4::new(target.source, 0)).map_err(failed)?;
    nix::sys::socket::setsockopt(
        &socket,
        nix::sys::socket::sockopt::BindToDevice,
        &std::ffi::OsString::from(&target.interface),
    )
    .map_err(failed)?;
    socket.set_nonblocking(true).map_err(failed)?;
    let socket = tokio::net::UdpSocket::from_std(socket).map_err(failed)?;
    tokio::time::timeout_at(until.into(), async {
        socket.connect(target.peer).await.map_err(failed)?;
        page(
            target,
            first,
            next_sequence(&SEQUENCE),
            max_frame,
            &mut Socket(socket),
        )
        .await
    })
    .await
    .map_err(|_| CommandError::ReceiverReadTimeout)?
}

#[cfg(target_os = "linux")]
pub(crate) async fn read(
    target: &Target,
    first: u16,
    deadline: Instant,
    max_frame: usize,
) -> Result<PreparedPage, CommandError> {
    let before = crate::netif::lookup(&target.interface).map_err(failed)?;
    if before.addr != target.source {
        return Err(CommandError::RoutingSnapshotExpired);
    }
    let before_scope = scope(target)?;
    let result = with_retry(deadline, |until| attempt(target, first, until, max_frame)).await?;
    if Instant::now() >= deadline {
        return Err(CommandError::ReceiverReadTimeout);
    }
    let after = crate::netif::lookup(&target.interface).map_err(failed)?;
    if after.index != before.index || after.addr != before.addr || scope(target)? != before_scope {
        return Err(CommandError::RoutingSnapshotExpired);
    }
    Ok(result)
}

#[cfg(not(target_os = "linux"))]
pub(crate) async fn read(
    _: &Target,
    _: u16,
    _: Instant,
    _: usize,
) -> Result<PreparedPage, CommandError> {
    Err(unsupported(
        "receiver reads require Linux interface binding",
    ))
}

pub(crate) mod lock_state;
