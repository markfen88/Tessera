//! Bounded Linux rtnetlink observations, not a TX permission or atomic revision.
//! The socket subscribes before dumping, remains open, and invalidates on any
//! notification or read failure. No production caller or state-model adapter
//! exists. Source/interface pinning remains the media socket's separate duty.
use std::fs::File;
use std::io::{self, IoSliceMut};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};

use nix::libc;
use nix::sys::socket::{
    AddressFamily, MsgFlags, NetlinkAddr, SockFlag, SockProtocol, SockType, bind, getsockname,
    recvmsg, sendto, socket,
};

mod projection;
mod wire;
use projection::NetworkView;

const MAX_DATAGRAM: usize = 65_536;
// LINK, IPV4_IFADDR, IPV4_ROUTE and IPV4_RULE (Linux rtnetlink UAPI).
const GROUPS: u32 = 1 | 0x10 | 0x40 | 0x80;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub bytes: usize,
    pub records: usize,
    pub duration: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            bytes: 8 << 20,
            records: 4096,
            duration: Duration::from_secs(3),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NamespaceKey {
    pub device: u64,
    pub inode: u64,
}

#[derive(Debug)]
pub(crate) struct ObservedNetwork {
    pub namespace: NamespaceKey,
    pub view: NetworkView,
    // The complete bounded record bodies retain unknown fields and attributes.
    // No lease/cache timestamp or interface-counter version is invented here.
    pub records: Vec<wire::Record>,
}

#[derive(Debug)]
pub(crate) struct Capture {
    socket: OwnedFd,
    _namespace: File,
    observation: ObservedNetwork,
    valid: bool,
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn namespace_key(namespace: &File) -> io::Result<NamespaceKey> {
    let meta = namespace.metadata()?;
    Ok(NamespaceKey {
        device: meta.dev(),
        inode: meta.ino(),
    })
}

impl Capture {
    /// Finite read-only GETLINK/GETADDR/GETROUTE/GETRULE dumps. There is no set,
    /// delete, route lookup mutation, retry or background thread in this path.
    pub(crate) fn collect(limits: Limits) -> io::Result<Self> {
        if limits.bytes == 0 || limits.records == 0 || limits.duration.is_zero() {
            return Err(invalid("empty network observation budget"));
        }
        let deadline = Instant::now()
            .checked_add(limits.duration)
            .ok_or_else(|| invalid("network deadline overflow"))?;
        // thread-self matters if the caller's network namespace differs from
        // the process leader. Retain the namespace inode while the socket lives.
        let namespace = File::open("/proc/thread-self/ns/net")?;
        let namespace_id = namespace_key(&namespace)?;
        let socket = socket(
            AddressFamily::Netlink,
            SockType::Raw,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
            SockProtocol::NetlinkRoute,
        )
        .map_err(io::Error::from)?;
        bind(socket.as_raw_fd(), &NetlinkAddr::new(0, GROUPS)).map_err(io::Error::from)?;
        let port = getsockname::<NetlinkAddr>(socket.as_raw_fd())
            .map_err(io::Error::from)?
            .pid();
        if namespace_key(&File::open("/proc/thread-self/ns/net")?)? != namespace_id {
            return Err(invalid("network namespace changed during socket creation"));
        }
        let mut records = Vec::new();
        let mut remaining = limits.bytes;
        let mut buffer = vec![0; MAX_DATAGRAM];
        for (seq, kind, size) in [(1, 18, 16), (2, 22, 8), (3, 26, 12), (4, 34, 12)] {
            let request = wire::request(seq, port, kind, size);
            let sent = sendto(
                socket.as_raw_fd(),
                &request,
                &NetlinkAddr::new(0, 0),
                MsgFlags::MSG_DONTWAIT,
            )
            .map_err(io::Error::from)?;
            if sent != request.len() {
                return Err(invalid("partial netlink dump request"));
            }
            let mut dump = wire::Dump::new(seq, port, kind - 2);
            while !dump.done() {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "network observation deadline",
                    ));
                }
                match receive(&socket, &mut buffer) {
                    Ok((bytes, group)) => {
                        remaining = remaining
                            .checked_sub(bytes)
                            .ok_or_else(|| invalid("network byte budget exceeded"))?;
                        if group != 0 {
                            return Err(invalid("network notification during dump"));
                        }
                        let batch = dump.accept(&buffer[..bytes])?;
                        if records
                            .len()
                            .checked_add(batch.len())
                            .is_none_or(|n| n > limits.records)
                        {
                            return Err(invalid("network record budget exceeded"));
                        }
                        records.extend(batch);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        wait_readable(&socket, deadline)?
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        // These are observations across dump operations, not proof of a native
        // revision. Any queued notification or unaccounted message invalidates.
        match receive(&socket, &mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
            Err(error) => return Err(error),
            Ok(_) => return Err(invalid("network changed at end of capture")),
        }
        let view = NetworkView::decode(&records)?;
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "network projection deadline",
            ));
        }
        Ok(Self {
            socket,
            _namespace: namespace,
            observation: ObservedNetwork {
                namespace: namespace_id,
                view,
                records,
            },
            valid: true,
        })
    }

    pub(crate) fn observation(&self) -> &ObservedNetwork {
        &self.observation
    }

    /// A finite last observation only. It cannot make route changes and a later
    /// authority/socket write atomic. Failure is sticky, never a refresh/rearm.
    pub(crate) fn still_unchanged(&mut self) -> io::Result<bool> {
        if !self.valid {
            return Ok(false);
        }
        let mut byte = [0; 1];
        match receive(&self.socket, &mut byte) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
            result => {
                self.valid = false;
                match result {
                    Ok(_) => Ok(false),
                    Err(error) => Err(error),
                }
            }
        }
    }
}

fn receive(socket: &OwnedFd, buffer: &mut [u8]) -> io::Result<(usize, u32)> {
    let mut vectors = [IoSliceMut::new(buffer)];
    let message = recvmsg::<NetlinkAddr>(
        socket.as_raw_fd(),
        &mut vectors,
        None,
        MsgFlags::MSG_DONTWAIT,
    )
    .map_err(io::Error::from)?;
    let sender = message
        .address
        .ok_or_else(|| invalid("missing netlink sender"))?;
    if sender.pid() != 0 {
        return Err(invalid("netlink sender is not the kernel"));
    }
    if message
        .flags
        .intersects(MsgFlags::MSG_TRUNC | MsgFlags::MSG_CTRUNC)
    {
        return Err(invalid("truncated netlink observation"));
    }
    if message.bytes == 0 {
        return Err(invalid("empty netlink datagram"));
    }
    Ok((message.bytes, sender.groups()))
}
fn wait_readable(socket: &OwnedFd, deadline: Instant) -> io::Result<()> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "network dump wait"));
        }
        let millis = i32::try_from(left.as_millis().saturating_add(1)).unwrap_or(i32::MAX);
        let mut descriptor = libc::pollfd {
            fd: socket.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: a live stack pollfd and a retained, nonblocking socket. The
        // wait is bounded by the one monotonic end-to-end collection deadline.
        let result = unsafe { libc::poll(&raw mut descriptor, 1, millis) };
        if result > 0 {
            if descriptor.revents != libc::POLLIN {
                return Err(invalid("netlink poll error/overrun"));
            }
            return Ok(());
        }
        if result == 0 {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "network dump wait"));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
