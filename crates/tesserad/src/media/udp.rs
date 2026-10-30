//! UDP I/O of the media engine: batched `recvmmsg` / `sendmmsg` over
//! message arrays that are allocated once (ADR 0004).
//!
//! This is where `tesserad`'s media `unsafe` lives: the two batched socket
//! calls, walking the control messages of a received datagram, and two
//! `setsockopt` calls (multicast send interface, group membership). Each
//! block says why it is sound. Sockets themselves are created with
//! `std::net::UdpSocket` (or `nix`, for a group listener that must set
//! `SO_REUSEADDR` before it binds) and configured with `nix`.
//!
//! * [`RxSock`] receives up to `batch` datagrams per call into one
//!   pre-allocated buffer, with the sender address and the kernel's receive
//!   timestamp (`SO_TIMESTAMPNS`, `CLOCK_REALTIME`) of each; the caller maps
//!   the timestamp onto the monotonic clock.
//! * [`TxSock`] collects outgoing datagrams (each with its own destination,
//!   so one socket serves any number of flows) and sends them with one call.
//!   Datagrams the kernel does not take are dropped and counted, never
//!   retried later (ADR 0004: never burst).
//!
//! The steady-state paths ([`RxSock::recv`], [`RxSock::datagram`],
//! [`TxSock::slot`], [`TxSock::queue`], [`TxSock::flush`]) do not allocate.
#![allow(unsafe_code)]

use std::io;
use std::mem::{size_of, zeroed};
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::Arc;

use nix::libc;
use nix::sys::socket::{getsockopt, setsockopt, sockopt};
use tessera_media::Counter;

/// Words of control-message space per received datagram (64 bytes: room for
/// one `SCM_TIMESTAMPNS`, which needs `CMSG_SPACE(16)`).
const CTRL_WORDS: usize = 8;

const SOCKADDR_IN_LEN: libc::socklen_t = size_of::<libc::sockaddr_in>() as libc::socklen_t;

/// A vector of `n` all-zero C structs.
///
/// Only used for `libc::iovec`, `libc::sockaddr_in` and `libc::mmsghdr`
/// (plain data, for which the all-zero bit pattern is a valid value: null
/// pointers, zero lengths and addresses).
fn zeroed_vec<T: Zeroable>(n: usize) -> Vec<T> {
    // SAFETY: `Zeroable` is implemented only for the three plain-data C
    // structs named above, for which the all-zero bit pattern is valid.
    (0..n).map(|_| unsafe { zeroed() }).collect()
}

/// Types for which the all-zero bit pattern is a valid value. Private and
/// implemented only for the C structs this module fills in.
trait Zeroable {}
impl Zeroable for libc::iovec {}
impl Zeroable for libc::sockaddr_in {}
impl Zeroable for libc::mmsghdr {}

/// One received datagram, borrowed from the batch.
#[derive(Debug, Clone, Copy)]
pub struct Datagram<'a> {
    /// The bytes received (cut at the buffer size if `truncated`).
    pub data: &'a [u8],
    /// The sender's IPv4 address and port (where receiver keepalives go).
    pub src: Option<SocketAddrV4>,
    /// Kernel receive time, `CLOCK_REALTIME` nanoseconds.
    pub stamp_realtime_ns: Option<u64>,
    /// The datagram did not fit the buffer.
    pub truncated: bool,
}

/// A bound, non-blocking UDP socket that receives in batches.
pub struct RxSock {
    sock: UdpSocket,
    cap: usize,
    bufs: Vec<u8>,
    // Self-referential message arrays: `msgs[i]` points into `iov[i]`,
    // `names[i]` and `ctrl`, and `iov[i]` points into `bufs`. All of these
    // are heap allocations owned by this struct that are never resized after
    // construction, so the pointers stay valid when the struct moves.
    iov: Vec<libc::iovec>,
    names: Vec<libc::sockaddr_in>,
    ctrl: Vec<u64>,
    msgs: Vec<libc::mmsghdr>,
    got: usize,
    stamps: bool,
}

// SAFETY: the raw pointers inside `iov` and `msgs` point only into
// allocations owned by this very struct, which move with it; nothing else
// aliases them. The socket is a plain file descriptor.
unsafe impl Send for RxSock {}

impl std::fmt::Debug for RxSock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RxSock")
            .field("batch", &self.msgs.len())
            .field("cap", &self.cap)
            .finish_non_exhaustive()
    }
}

impl RxSock {
    /// Bind `local`, with room for `batch` datagrams of up to `cap` bytes per
    /// call and a kernel receive buffer of about `rcvbuf` bytes.
    ///
    /// # Errors
    /// Socket creation, bind or configuration failures.
    pub fn bind(
        local: SocketAddrV4,
        iface: &str,
        rcvbuf: usize,
        batch: usize,
        cap: usize,
    ) -> io::Result<Self> {
        let sock = UdpSocket::bind(local)?;
        bind_to_device(&sock, iface)?;
        Self::from_socket(sock, rcvbuf, batch, cap)
    }

    /// Listen on a multicast group (`flows-media.md` §5): bind the group
    /// address and port with `SO_REUSEADDR`, so that every listener of the
    /// group gets its own socket and each receives every datagram, restrict
    /// the socket to `iface` and join the group there (`ifindex`). The
    /// membership ends when the last descriptor of the socket is closed.
    ///
    /// # Errors
    /// Socket creation, bind, join or configuration failures.
    pub fn join(
        group: SocketAddrV4,
        iface: &str,
        ifindex: u32,
        rcvbuf: usize,
        batch: usize,
        cap: usize,
    ) -> io::Result<Self> {
        use nix::sys::socket::{AddressFamily, SockFlag, SockType, SockaddrIn, bind, socket};
        let fd = socket(
            AddressFamily::Inet,
            SockType::Datagram,
            SockFlag::SOCK_CLOEXEC,
            None,
        )?;
        let sock = UdpSocket::from(fd);
        setsockopt(&sock, sockopt::ReuseAddr, &true)?;
        bind_to_device(&sock, iface)?;
        bind(sock.as_raw_fd(), &SockaddrIn::from(group))?;
        join_group(&sock, *group.ip(), ifindex)?;
        Self::from_socket(sock, rcvbuf, batch, cap)
    }

    /// Duplicate the underlying socket (the same kernel socket, a second
    /// descriptor): a replacement flow on the same port takes it over without
    /// the port ever being unbound.
    ///
    /// # Errors
    /// The descriptor cannot be duplicated.
    pub fn try_clone_socket(&self) -> io::Result<UdpSocket> {
        self.sock.try_clone()
    }

    /// Wrap an already bound (and interface-restricted) socket.
    ///
    /// # Errors
    /// Socket configuration failures.
    pub fn from_socket(
        sock: UdpSocket,
        rcvbuf: usize,
        batch: usize,
        cap: usize,
    ) -> io::Result<Self> {
        sock.set_nonblocking(true)?;
        // Best effort: the kernel caps the size at `net.core.rmem_max`
        // unless the process may force it.
        if setsockopt(&sock, sockopt::RcvBufForce, &rcvbuf).is_err() {
            let _ = setsockopt(&sock, sockopt::RcvBuf, &rcvbuf);
        }
        let stamps = setsockopt(&sock, sockopt::ReceiveTimestampns, &true).is_ok();
        let batch = batch.max(1);
        let cap = cap.max(1);
        let mut s = Self {
            sock,
            cap,
            bufs: vec![0; batch * cap],
            iov: zeroed_vec(batch),
            names: zeroed_vec(batch),
            ctrl: vec![0; batch * CTRL_WORDS],
            msgs: zeroed_vec(batch),
            got: 0,
            stamps,
        };
        s.wire();
        Ok(s)
    }

    /// Points every message at its buffers and resets the fields the kernel
    /// overwrites.
    fn wire(&mut self) {
        let cap = self.cap;
        for (v, chunk) in self.iov.iter_mut().zip(self.bufs.chunks_exact_mut(cap)) {
            v.iov_base = chunk.as_mut_ptr().cast();
            v.iov_len = cap;
        }
        for (i, m) in self.msgs.iter_mut().enumerate() {
            // Pointers into this struct's own heap allocations; taking them is
            // safe, using them is the (unsafe) `recvmmsg` call.
            if let Some(n) = self.names.get_mut(i) {
                m.msg_hdr.msg_name = std::ptr::from_mut(n).cast();
            }
            if let Some(v) = self.iov.get_mut(i) {
                m.msg_hdr.msg_iov = std::ptr::from_mut(v);
            }
            if let Some(c) = self.ctrl.get_mut(i * CTRL_WORDS..(i + 1) * CTRL_WORDS) {
                m.msg_hdr.msg_control = c.as_mut_ptr().cast();
            }
            m.msg_hdr.msg_iovlen = 1;
            m.msg_hdr.msg_namelen = SOCKADDR_IN_LEN;
            m.msg_hdr.msg_controllen = (CTRL_WORDS * 8) as _;
            m.msg_hdr.msg_flags = 0;
            m.msg_len = 0;
        }
    }

    /// Datagrams per call.
    #[must_use]
    pub fn batch(&self) -> usize {
        self.msgs.len()
    }

    /// The kernel's receive buffer for this socket in bytes (what
    /// `SO_RCVBUF` reads back: twice the value it accepted).
    #[must_use]
    pub fn effective_rcvbuf(&self) -> usize {
        getsockopt(&self.sock, sockopt::RcvBuf).unwrap_or(0)
    }

    /// Whether kernel timestamps are available on this socket.
    #[must_use]
    pub fn has_kernel_stamps(&self) -> bool {
        self.stamps
    }

    /// The bound local port.
    ///
    /// # Errors
    /// The socket address cannot be read.
    pub fn local_port(&self) -> io::Result<u16> {
        Ok(self.sock.local_addr()?.port())
    }

    /// Receives whatever is queued (non-blocking). Returns the number of
    /// datagrams now readable through [`datagram`](Self::datagram); `0` if the
    /// queue is empty.
    ///
    /// # Errors
    /// A socket error other than "nothing to read" or an interrupted call.
    pub fn recv(&mut self) -> io::Result<usize> {
        for m in self.msgs.iter_mut().take(self.got) {
            m.msg_hdr.msg_namelen = SOCKADDR_IN_LEN;
            m.msg_hdr.msg_controllen = (CTRL_WORDS * 8) as _;
            m.msg_hdr.msg_flags = 0;
            m.msg_len = 0;
        }
        self.got = 0;
        // SAFETY: the descriptor is open (owned by `self.sock`); `msgs` holds
        // `batch` initialised `mmsghdr`s whose pointers refer to buffers of
        // this struct (see `wire`) with the sizes recorded in the headers;
        // `&mut self` makes the access exclusive; a null timeout means none.
        let rc = unsafe {
            libc::recvmmsg(
                self.sock.as_raw_fd(),
                self.msgs.as_mut_ptr(),
                self.msgs.len() as libc::c_uint,
                libc::MSG_DONTWAIT,
                std::ptr::null_mut(),
            )
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            return match e.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(0),
                _ => Err(e),
            };
        }
        self.got = usize::try_from(rc).unwrap_or(0);
        Ok(self.got)
    }

    /// Sends one small datagram (a receiver keepalive) from this very socket,
    /// so it leaves from the flow's bound address and port, to `dest`.
    /// Non-blocking (the socket is): a full buffer is an error, never a wait
    /// or a retry. Does not allocate.
    ///
    /// # Errors
    /// The kernel refused the datagram or took only part of it.
    pub fn send_to(&self, payload: &[u8], dest: SocketAddrV4) -> io::Result<()> {
        let n = self.sock.send_to(payload, dest)?;
        if n == payload.len() {
            Ok(())
        } else {
            Err(io::ErrorKind::WriteZero.into())
        }
    }

    /// Datagram `i` of the last [`recv`](Self::recv).
    #[must_use]
    pub fn datagram(&self, i: usize) -> Option<Datagram<'_>> {
        if i >= self.got {
            return None;
        }
        let m = self.msgs.get(i)?;
        let len = (m.msg_len as usize).min(self.cap);
        let data = self.bufs.get(i * self.cap..i * self.cap + len)?;
        let src = (m.msg_hdr.msg_namelen as usize >= size_of::<libc::sockaddr_in>())
            .then(|| self.names.get(i))
            .flatten()
            .filter(|n| i32::from(n.sin_family) == libc::AF_INET)
            .map(|n| {
                SocketAddrV4::new(
                    Ipv4Addr::from(u32::from_be(n.sin_addr.s_addr)),
                    u16::from_be(n.sin_port),
                )
            });
        Some(Datagram {
            data,
            src,
            stamp_realtime_ns: if self.stamps {
                parse_stamp(&m.msg_hdr)
            } else {
                None
            },
            truncated: m.msg_hdr.msg_flags & libc::MSG_TRUNC != 0,
        })
    }
}

/// The `SCM_TIMESTAMPNS` control message of a received datagram, as
/// `CLOCK_REALTIME` nanoseconds.
fn parse_stamp(hdr: &libc::msghdr) -> Option<u64> {
    // SAFETY: `hdr` is a message header the kernel filled: its control
    // pointer and length describe our own control buffer, and the CMSG_*
    // helpers stay inside `msg_controllen` bytes. The payload read is
    // unaligned-safe and its size is checked against `cmsg_len` first.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(hdr);
        while !c.is_null() {
            let ch = &*c;
            if ch.cmsg_level == libc::SOL_SOCKET
                && ch.cmsg_type == libc::SCM_TIMESTAMPNS
                && ch.cmsg_len as usize
                    >= libc::CMSG_LEN(size_of::<libc::timespec>() as u32) as usize
            {
                let ts = std::ptr::read_unaligned(libc::CMSG_DATA(c).cast::<libc::timespec>());
                let secs = u64::try_from(ts.tv_sec).ok()?;
                let nanos = u64::try_from(ts.tv_nsec).ok()?;
                return secs.checked_mul(1_000_000_000)?.checked_add(nanos);
            }
            c = libc::CMSG_NXTHDR(hdr, c);
        }
    }
    None
}

/// What one [`TxSock::flush`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flush {
    /// Datagrams the kernel took.
    pub sent: usize,
    /// Datagrams that were not sent (dropped, never retried).
    pub failed: usize,
    /// `errno` of the failure, if any.
    pub errno: Option<i32>,
}

/// A UDP socket that sends in batches, each datagram to its own destination.
pub struct TxSock {
    sock: UdpSocket,
    cap: usize,
    bufs: Vec<u8>,
    iov: Vec<libc::iovec>,
    names: Vec<libc::sockaddr_in>,
    msgs: Vec<libc::mmsghdr>,
    /// Per queued datagram, the counter of the flow that owns it (bumped when
    /// the kernel refuses that datagram).
    owners: Vec<Option<Arc<Counter>>>,
}

// SAFETY: as for `RxSock`: the raw pointers refer only to allocations owned
// by this struct.
unsafe impl Send for TxSock {}

impl std::fmt::Debug for TxSock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TxSock")
            .field("batch", &self.msgs.len())
            .field("cap", &self.cap)
            .finish_non_exhaustive()
    }
}

impl TxSock {
    /// Read up to `max` pending datagrams without blocking and report each
    /// sender: receivers send their keepalives to the source port of the
    /// media (`control-flows.md` §7.4). Contents are ignored.
    pub fn drain(&self, max: usize, mut seen: impl FnMut(SocketAddrV4)) {
        let mut buf = [0u8; 64];
        for _ in 0..max {
            match self.sock.recv_from(&mut buf) {
                Ok((_, std::net::SocketAddr::V4(from))) => seen(from),
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }

    /// A socket bound to `local` (any port) that marks its packets with
    /// `dscp` and sends multicast out of the interface that owns `local`.
    /// `batch` datagrams of up to `cap` bytes can be queued per flush.
    ///
    /// # Errors
    /// Socket creation, bind or configuration failures.
    pub fn open(
        local: Ipv4Addr,
        iface: &str,
        dscp: u8,
        sndbuf: usize,
        batch: usize,
        cap: usize,
    ) -> io::Result<Self> {
        let sock = UdpSocket::bind(SocketAddrV4::new(local, 0))?;
        bind_to_device(&sock, iface)?;
        sock.set_nonblocking(true)?;
        setsockopt(&sock, sockopt::Ipv4Tos, &(i32::from(dscp & 63) << 2))
            .map_err(io::Error::from)?;
        if setsockopt(&sock, sockopt::SndBufForce, &sndbuf).is_err() {
            let _ = setsockopt(&sock, sockopt::SndBuf, &sndbuf);
        }
        sock.set_multicast_ttl_v4(1)?;
        set_multicast_if(&sock, local)?;
        let batch = batch.max(1);
        let cap = cap.max(1);
        let mut s = Self {
            sock,
            cap,
            bufs: vec![0; batch * cap],
            iov: zeroed_vec(batch),
            names: zeroed_vec(batch),
            msgs: zeroed_vec(batch),
            owners: vec![None; batch],
        };
        s.wire();
        Ok(s)
    }

    fn wire(&mut self) {
        let cap = self.cap;
        for (v, chunk) in self.iov.iter_mut().zip(self.bufs.chunks_exact_mut(cap)) {
            v.iov_base = chunk.as_mut_ptr().cast();
            v.iov_len = 0;
        }
        for (i, m) in self.msgs.iter_mut().enumerate() {
            // Pointers into this struct's own heap allocations; taking them is
            // safe, using them is the (unsafe) `sendmmsg` call.
            if let Some(n) = self.names.get_mut(i) {
                m.msg_hdr.msg_name = std::ptr::from_mut(n).cast();
            }
            if let Some(v) = self.iov.get_mut(i) {
                m.msg_hdr.msg_iov = std::ptr::from_mut(v);
            }
            m.msg_hdr.msg_iovlen = 1;
            m.msg_hdr.msg_namelen = SOCKADDR_IN_LEN;
            m.msg_hdr.msg_control = std::ptr::null_mut();
            m.msg_hdr.msg_controllen = 0;
        }
    }

    /// Datagrams per flush.
    #[must_use]
    pub fn batch(&self) -> usize {
        self.msgs.len()
    }

    /// The kernel's send buffer for this socket in bytes (what `SO_SNDBUF`
    /// reads back: twice the value it accepted).
    #[must_use]
    pub fn effective_sndbuf(&self) -> usize {
        getsockopt(&self.sock, sockopt::SndBuf).unwrap_or(0)
    }

    /// Largest datagram.
    #[must_use]
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// The buffer of queue slot `i` (`cap` bytes).
    pub fn slot(&mut self, i: usize) -> Option<&mut [u8]> {
        self.bufs.get_mut(i * self.cap..(i + 1) * self.cap)
    }

    /// Mark slot `i` as holding a datagram of `len` bytes for `dest`; `owner`
    /// counts it if the kernel refuses it.
    pub fn queue(&mut self, i: usize, len: usize, dest: SocketAddrV4, owner: &Arc<Counter>) {
        if let Some(o) = self.owners.get_mut(i) {
            *o = Some(Arc::clone(owner));
        }
        if let (Some(v), Some(n)) = (self.iov.get_mut(i), self.names.get_mut(i)) {
            v.iov_len = len.min(self.cap);
            n.sin_family = libc::AF_INET as libc::sa_family_t;
            n.sin_port = dest.port().to_be();
            n.sin_addr = libc::in_addr {
                s_addr: u32::from(*dest.ip()).to_be(),
            };
            n.sin_zero = [0; 8];
        }
    }

    /// Sends slots `0..n` (non-blocking).
    ///
    /// A datagram the kernel refuses for its own reasons (no route, an
    /// invalid destination) is dropped and counted on its owner; **the rest of
    /// the batch is still sent**. If the socket buffer is full the rest of the
    /// batch is dropped too (nothing is ever retried later, ADR 0004).
    pub fn flush(&mut self, n: usize) -> Flush {
        let n = n.min(self.msgs.len());
        let mut out = Flush::default();
        let mut start = 0;
        while start < n {
            let Some(tail) = self.msgs.get_mut(start..n) else {
                break;
            };
            // SAFETY: the descriptor is open; `tail` holds initialised
            // headers whose pointers refer to this struct's own buffers (see
            // `wire`), with lengths set by `queue`; `&mut self` makes the
            // access exclusive.
            let rc = unsafe {
                libc::sendmmsg(
                    self.sock.as_raw_fd(),
                    tail.as_mut_ptr(),
                    tail.len() as libc::c_uint,
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            if rc > 0 {
                let k = usize::try_from(rc).unwrap_or(1).min(n - start);
                out.sent += k;
                start += k;
                continue;
            }
            let errno = io::Error::last_os_error().raw_os_error();
            out.errno = errno;
            let full = matches!(errno, Some(libc::EAGAIN | libc::ENOBUFS));
            let upto = if full { n } else { start + 1 };
            for slot in start..upto {
                if let Some(Some(c)) = self.owners.get(slot) {
                    c.inc();
                }
            }
            out.failed += upto - start;
            start = upto;
        }
        for o in self.owners.iter_mut().take(n) {
            *o = None;
        }
        out
    }
}

/// Restrict the socket to one interface (`SO_BINDTODEVICE`): datagrams that
/// arrive on another interface are never delivered to it, and it sends only
/// out of this one. Unprivileged since Linux 5.7 for a socket that is not yet
/// bound to a device; refusing is an error, so a flow never silently listens
/// on every interface.
fn bind_to_device(sock: &UdpSocket, iface: &str) -> io::Result<()> {
    setsockopt(sock, sockopt::BindToDevice, &std::ffi::OsString::from(iface)).map_err(|e| {
        io::Error::new(
            io::Error::from(e).kind(),
            format!(
                "cannot bind to interface `{iface}`: {e} (SO_BINDTODEVICE needs Linux 5.7 or CAP_NET_RAW)"
            ),
        )
    })
}

/// Join `group` on the interface with index `ifindex` (`IP_ADD_MEMBERSHIP`
/// with an `ip_mreqn`: the interface by index, not by an address it may
/// lose).
fn join_group(sock: &UdpSocket, group: Ipv4Addr, ifindex: u32) -> io::Result<()> {
    let req = libc::ip_mreqn {
        imr_multiaddr: libc::in_addr {
            s_addr: u32::from(group).to_be(),
        },
        imr_address: libc::in_addr { s_addr: 0 },
        imr_ifindex: i32::try_from(ifindex)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interface index"))?,
    };
    // SAFETY: `req` outlives the call and its size is passed; the descriptor
    // is open.
    let rc = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_ADD_MEMBERSHIP,
            (&raw const req).cast(),
            size_of::<libc::ip_mreqn>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn set_multicast_if(sock: &UdpSocket, local: Ipv4Addr) -> io::Result<()> {
    let addr = libc::in_addr {
        s_addr: u32::from(local).to_be(),
    };
    // SAFETY: `addr` outlives the call and its size is passed; the descriptor
    // is open.
    let rc = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_IF,
            (&raw const addr).cast(),
            size_of::<libc::in_addr>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn realtime_ns() -> i128 {
        let r = nix::time::clock_gettime(nix::time::ClockId::CLOCK_REALTIME).unwrap();
        i128::from(r.tv_sec()) * 1_000_000_000 + i128::from(r.tv_nsec())
    }

    fn wait_for(rx: &mut RxSock, want: usize) -> usize {
        let t = Instant::now();
        loop {
            let n = rx.recv().unwrap();
            if n >= want || t.elapsed() > Duration::from_secs(2) {
                return n;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_batch_goes_out_with_one_call_and_comes_in_with_one() {
        let mut rx = RxSock::bind(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            "lo",
            1 << 20,
            8,
            256,
        )
        .unwrap();
        let port = rx.local_port().unwrap();
        let ctr = Arc::new(Counter::new());
        let mut tx = TxSock::open(Ipv4Addr::LOCALHOST, "lo", 46, 1 << 20, 8, 256).unwrap();
        let dest = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        for i in 0..5u8 {
            let s = tx.slot(usize::from(i)).unwrap();
            s[..3].copy_from_slice(&[i, i + 1, i + 2]);
            tx.queue(usize::from(i), 3, dest, &ctr);
        }
        let f = tx.flush(5);
        assert_eq!(
            f,
            Flush {
                sent: 5,
                failed: 0,
                errno: None
            }
        );
        let before = realtime_ns();
        assert_eq!(wait_for(&mut rx, 5), 5);
        let after = realtime_ns();
        for i in 0..5usize {
            let d = rx.datagram(i).unwrap();
            assert_eq!(d.data, &[i as u8, i as u8 + 1, i as u8 + 2]);
            assert_eq!(d.src.map(|a| *a.ip()), Some(Ipv4Addr::LOCALHOST));
            assert!(!d.truncated);
            // The kernel stamp lies between "before the send" and "now".
            let stamp = i128::from(d.stamp_realtime_ns.expect("kernel timestamp"));
            assert!(
                stamp >= before - 1_000_000 && stamp <= after,
                "stamp {stamp}"
            );
        }
        assert!(rx.datagram(5).is_none());
        assert_eq!(rx.recv().unwrap(), 0, "nothing more queued");
    }

    #[test]
    fn every_listener_of_a_multicast_group_receives_every_datagram() {
        let lo = nix::net::if_::if_nametoindex("lo").unwrap();
        let group = Ipv4Addr::new(239, 255, 77, 1);
        let mut a = RxSock::join(SocketAddrV4::new(group, 0), "lo", lo, 1 << 16, 8, 64).unwrap();
        let port = a.local_port().unwrap();
        // A second listener binds the same group and port.
        let at = SocketAddrV4::new(group, port);
        let mut b = RxSock::join(at, "lo", lo, 1 << 16, 8, 64).unwrap();
        // A unicast socket on the interface address does not see the group.
        let mut other = RxSock::bind(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
            "lo",
            1 << 16,
            8,
            64,
        )
        .unwrap();
        let ctr = Arc::new(Counter::new());
        let mut tx = TxSock::open(Ipv4Addr::LOCALHOST, "lo", 46, 1 << 16, 8, 64).unwrap();
        tx.slot(0).unwrap()[..2].copy_from_slice(&[7, 8]);
        tx.queue(0, 2, at, &ctr);
        assert_eq!(tx.flush(1).sent, 1);
        for rx in [&mut a, &mut b] {
            assert!(wait_for(rx, 1) >= 1, "each listener receives the datagram");
            let d = rx.datagram(0).unwrap();
            assert_eq!(d.data, &[7, 8]);
            assert_eq!(d.src.map(|s| *s.ip()), Some(Ipv4Addr::LOCALHOST));
        }
        assert_eq!(other.recv().unwrap(), 0);
        // Leaving (closing) one listener does not end the other's membership.
        drop(a);
        tx.slot(0).unwrap()[..2].copy_from_slice(&[9, 9]);
        tx.queue(0, 2, at, &ctr);
        assert_eq!(tx.flush(1).sent, 1);
        let t = Instant::now();
        let got = loop {
            let n = b.recv().unwrap();
            if (0..n).any(|i| b.datagram(i).is_some_and(|d| d.data == [9, 9])) {
                break true;
            }
            if t.elapsed() > Duration::from_secs(2) {
                break false;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(got, "the remaining listener still receives the group");
    }

    #[test]
    fn the_sender_port_is_reported_and_a_reply_leaves_from_the_bound_port() {
        let mut rx = RxSock::bind(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            "lo",
            1 << 16,
            4,
            64,
        )
        .unwrap();
        let port = rx.local_port().unwrap();
        let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let peer_addr = match peer.local_addr().unwrap() {
            std::net::SocketAddr::V4(a) => a,
            std::net::SocketAddr::V6(_) => unreachable!(),
        };
        peer.send_to(b"media", (Ipv4Addr::LOCALHOST, port)).unwrap();
        assert_eq!(wait_for(&mut rx, 1), 1);
        let src = rx.datagram(0).unwrap().src;
        assert_eq!(src, Some(peer_addr), "address and port of the sender");
        rx.send_to(&[0x13, 0x37], peer_addr).unwrap();
        let mut buf = [0u8; 16];
        let (n, from) = peer.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], &[0x13, 0x37]);
        assert_eq!(
            from,
            std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            "sent from the receive socket itself"
        );
    }

    #[test]
    fn oversize_datagrams_are_flagged_truncated() {
        let mut rx = RxSock::bind(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            "lo",
            1 << 16,
            2,
            8,
        )
        .unwrap();
        let port = rx.local_port().unwrap();
        let mut tx = TxSock::open(Ipv4Addr::LOCALHOST, "lo", 0, 1 << 16, 2, 64).unwrap();
        tx.slot(0).unwrap()[..20].fill(7);
        tx.queue(
            0,
            20,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
            &Arc::default(),
        );
        assert_eq!(tx.flush(1).sent, 1);
        assert_eq!(wait_for(&mut rx, 1), 1);
        let d = rx.datagram(0).unwrap();
        assert!(d.truncated);
        assert_eq!(d.data.len(), 8);
    }

    #[test]
    fn sending_where_nothing_listens_is_not_an_error_and_a_bad_route_is_counted() {
        let mut tx = TxSock::open(Ipv4Addr::LOCALHOST, "lo", 0, 1 << 16, 4, 64).unwrap();
        tx.queue(
            0,
            1,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9),
            &Arc::default(),
        );
        assert_eq!(tx.flush(1).sent, 1, "UDP does not know nobody is listening");
        // A destination the socket cannot reach fails as a whole.
        tx.queue(
            0,
            1,
            SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 1), 9),
            &Arc::default(),
        );
        let f = tx.flush(1);
        assert_eq!(f.sent + f.failed, 1);
        assert_eq!(tx.flush(0), Flush::default());
    }

    #[test]
    fn a_refused_datagram_is_skipped_and_the_rest_of_the_batch_still_goes() {
        let mut rx = RxSock::bind(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            "lo",
            1 << 20,
            8,
            64,
        )
        .unwrap();
        let port = rx.local_port().unwrap();
        let good = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        // Port 0 is refused by the kernel for this datagram alone.
        let bad = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        let (c_good, c_bad) = (Arc::new(Counter::new()), Arc::new(Counter::new()));
        let mut tx = TxSock::open(Ipv4Addr::LOCALHOST, "lo", 0, 1 << 20, 8, 64).unwrap();
        for (i, (dest, owner)) in [
            (good, &c_good),
            (bad, &c_bad),
            (good, &c_good),
            (bad, &c_bad),
            (good, &c_good),
        ]
        .into_iter()
        .enumerate()
        {
            tx.slot(i).unwrap()[0] = i as u8;
            tx.queue(i, 1, dest, owner);
        }
        let f = tx.flush(5);
        assert_eq!((f.sent, f.failed), (3, 2), "{f:?}");
        assert_eq!(
            (c_good.get(), c_bad.get()),
            (0, 2),
            "only the refused ones are counted, on their owner"
        );
        assert_eq!(wait_for(&mut rx, 3), 3);
        let got: Vec<u8> = (0..3).map(|i| rx.datagram(i).unwrap().data[0]).collect();
        assert_eq!(
            got,
            [0, 2, 4],
            "the datagrams after a refused one were sent"
        );
        // The next flush starts clean.
        tx.queue(0, 1, good, &c_good);
        assert_eq!(tx.flush(1).sent, 1);
    }

    #[test]
    fn slots_are_bounded() {
        let mut tx = TxSock::open(Ipv4Addr::LOCALHOST, "lo", 0, 1 << 16, 3, 16).unwrap();
        assert_eq!(tx.slot(0).unwrap().len(), 16);
        assert!(tx.slot(3).is_none());
        assert_eq!(tx.batch(), 3);
    }
}
