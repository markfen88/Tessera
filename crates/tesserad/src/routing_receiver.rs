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
    // One sequence on one ephemeral connected socket. No resend or fallback.
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
    let socket = std::net::UdpSocket::bind(SocketAddrV4::new(target.source, 0)).map_err(failed)?;
    nix::sys::socket::setsockopt(
        &socket,
        nix::sys::socket::sockopt::BindToDevice,
        &std::ffi::OsString::from(&target.interface),
    )
    .map_err(failed)?;
    socket.set_nonblocking(true).map_err(failed)?;
    let socket = tokio::net::UdpSocket::from_std(socket).map_err(failed)?;
    let result = tokio::time::timeout_at(deadline.into(), async {
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
    .map_err(|_| CommandError::ReceiverReadTimeout)??;
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
