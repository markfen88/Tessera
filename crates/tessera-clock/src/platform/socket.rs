//! UDP sockets for the PTP event and general ports with `SO_TIMESTAMPING`.
//!
//! `recv` uses a raw `recvmsg` with a caller-free, stack-allocated control
//! buffer, so the receive path never allocates.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

use std::string::String;

use socket2::{Domain, InterfaceIndexOrAddress, Protocol, SockAddr, Socket, Type};

use crate::provisional::{EVENT_PORT, GENERAL_PORT, MULTICAST_GROUP_V4};

/// 64-bit-time variant (Linux 5.1+). The original option is not used: on
/// 32-bit targets its `timespec` layout differs from the one parsed here.
const SO_TIMESTAMPING_NEW: libc::c_int = 65;
const SCM_TIMESTAMPING_NEW: libc::c_int = 65;
const IP_MULTICAST_ALL: libc::c_int = 49;
const IP_RECVERR: libc::c_int = 11;
const MSG_ERRQUEUE: libc::c_int = 0x2000;
const SO_EE_ORIGIN_TIMESTAMPING: u8 = 4;

const SOF_TX_HARDWARE: u32 = 1 << 0;
const SOF_TX_SOFTWARE: u32 = 1 << 1;
const SOF_RX_HARDWARE: u32 = 1 << 2;
const SOF_RX_SOFTWARE: u32 = 1 << 3;
const SOF_SOFTWARE: u32 = 1 << 4;
const SOF_RAW_HARDWARE: u32 = 1 << 6;
const SOF_OPT_ID: u32 = 1 << 7;

/// Socket parameters.
#[derive(Debug, Clone)]
pub struct SocketConfig {
    /// Interface name: the sockets are bound to this device and join the
    /// group by its index, so a port never sees another interface's PTP.
    pub iface: String,
    /// Interface address used as the egress interface of multicast sends.
    pub local_addr: Ipv4Addr,
    /// DSCP written into the IP TOS byte (`dscp << 2`).
    pub dscp: u8,
    /// Whether our own multicast sends are looped back (the port filters its
    /// own identity either way).
    pub multicast_loop: bool,
    /// Request hardware timestamps in addition to software ones.
    pub hardware: bool,
}

/// Which kernel timestamps a received datagram carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stamps {
    /// Software timestamp: `CLOCK_REALTIME` ns.
    pub software_ns: Option<u64>,
    /// Raw hardware timestamp: PHC ns.
    pub hardware_ns: Option<u64>,
}

/// What `recv` returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Received {
    /// A datagram of `len` bytes with its receive stamps.
    Data {
        /// Bytes placed in the buffer.
        len: usize,
        /// Kernel RX timestamps.
        stamps: Stamps,
    },
    /// A transmit timestamp from the error queue. The looped-back packet is
    /// in the buffer (`len` bytes, headers included); it identifies the message
    /// (the per-socket key `id` is only a fallback: a failed send may or may
    /// not consume a key depending on the kernel).
    TxStamp {
        /// Per-socket send counter value (`SOF_TIMESTAMPING_OPT_ID`).
        id: u32,
        /// Bytes of the looped-back packet in the buffer.
        len: usize,
        /// Kernel TX timestamps.
        stamps: Stamps,
    },
    /// An error-queue entry that is not a TX timestamp (ignored).
    Other,
}

/// A bound, joined, non-blocking PTP UDP socket.
#[derive(Debug)]
pub struct PtpSocket {
    sock: Socket,
    dest: SockAddr,
}

fn set_multicast_all_off(sock: &Socket) -> io::Result<()> {
    let off: libc::c_int = 0;
    // SAFETY: `off` outlives the call; the length is that of a `c_int`; the
    // fd is a valid socket owned by `sock`.
    let rc = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::IPPROTO_IP,
            IP_MULTICAST_ALL,
            (&raw const off).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn set_so_timestamping(sock: &Socket, flags: u32) -> io::Result<()> {
    let try_opt = |opt: libc::c_int| -> io::Result<()> {
        // SAFETY: `flags` outlives the call; the length is that of a `u32`;
        // the fd is a valid socket owned by `sock`.
        let rc = unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                opt,
                (&raw const flags).cast(),
                size_of::<u32>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    try_opt(SO_TIMESTAMPING_NEW)
}

/// `SO_TIMESTAMPING` flags: software always, hardware in addition on request.
fn timestamping_flags(hardware: bool) -> u32 {
    let mut flags = SOF_RX_SOFTWARE | SOF_TX_SOFTWARE | SOF_SOFTWARE | SOF_OPT_ID;
    if hardware {
        flags |= SOF_RX_HARDWARE | SOF_TX_HARDWARE | SOF_RAW_HARDWARE;
    }
    flags
}

impl PtpSocket {
    /// Switches hardware timestamps on or off on a live event socket (the
    /// socket option can be changed at any time; no rebind, no loss of
    /// queued data). Software stamps stay enabled either way.
    ///
    /// # Errors
    /// The `setsockopt` failed.
    pub fn set_hardware(&self, hardware: bool) -> io::Result<()> {
        set_so_timestamping(&self.sock, timestamping_flags(hardware))
    }

    /// Opens the event (319) or general (320) socket.
    ///
    /// # Errors
    /// Any socket, bind, multicast or option failure.
    pub fn open(cfg: &SocketConfig, event: bool) -> io::Result<Self> {
        let port = if event { EVENT_PORT } else { GENERAL_PORT };
        let group = Ipv4Addr::from(MULTICAST_GROUP_V4);
        let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        sock.set_reuse_address(true)?;
        // Bind to the group address so only this subdomain's traffic arrives.
        sock.bind(&SockAddr::from(SocketAddrV4::new(group, port)))?;
        let ifindex = nix::net::if_::if_nametoindex(cfg.iface.as_str()).map_err(io::Error::from)?;
        // Only deliver groups this socket joined (default is any joined
        // group on the host), and join by interface index.
        set_multicast_all_off(&sock)?;
        sock.join_multicast_v4_n(&group, &InterfaceIndexOrAddress::Index(ifindex))?;
        // Bind to the device so datagrams of other interfaces never match.
        // Needs CAP_NET_RAW; without it the per-interface group membership
        // above still isolates the sockets.
        match sock.bind_device(Some(cfg.iface.as_bytes())) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {}
            Err(e) => return Err(e),
        }
        sock.set_multicast_if_v4(&cfg.local_addr)?;
        sock.set_multicast_ttl_v4(1)?;
        sock.set_multicast_loop_v4(cfg.multicast_loop)?;
        sock.set_tos_v4(u32::from(cfg.dscp) << 2)?;
        sock.set_nonblocking(true)?;
        if event {
            set_so_timestamping(&sock, timestamping_flags(cfg.hardware))?;
        }
        Ok(PtpSocket {
            sock,
            dest: SockAddr::from(SocketAddrV4::new(group, port)),
        })
    }

    /// The descriptor (for polling and `SIOCSHWTSTAMP`).
    #[must_use]
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.sock.as_fd()
    }

    /// Sends `data` to the group on this socket's port.
    ///
    /// # Errors
    /// `WouldBlock` if the send buffer is full, or any send error.
    pub fn send(&self, data: &[u8]) -> io::Result<()> {
        self.sock.send_to(data, &self.dest).map(|_| ())
    }

    /// Receives one datagram (or one error-queue entry if `errqueue`).
    /// `Ok(None)` means nothing is pending. Does not allocate.
    ///
    /// # Errors
    /// Any receive error other than "would block" and "interrupted".
    pub fn recv(&self, buf: &mut [u8], errqueue: bool) -> io::Result<Option<Received>> {
        // 8-byte aligned control buffer: room for scm_timestamping (48 bytes)
        // and sock_extended_err + sockaddr_in with headers.
        let mut ctrl = [0u64; 32];
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        // SAFETY: an all-zero `msghdr` is a valid value (null pointers and
        // zero lengths); it is fully initialised below.
        let mut msg: libc::msghdr = unsafe { core::mem::zeroed() };
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = ctrl.as_mut_ptr().cast();
        msg.msg_controllen = size_of_val(&ctrl) as _;
        let flags = libc::MSG_DONTWAIT | if errqueue { MSG_ERRQUEUE } else { 0 };
        let n = loop {
            // SAFETY: `msg` points at `iov` (valid for `buf.len()` writable
            // bytes) and `ctrl` (valid for its size); all live across the call.
            let n = unsafe { libc::recvmsg(self.sock.as_raw_fd(), &raw mut msg, flags) };
            if n >= 0 {
                break n as usize;
            }
            let e = io::Error::last_os_error();
            match e.kind() {
                io::ErrorKind::WouldBlock => return Ok(None),
                io::ErrorKind::Interrupted => {}
                _ => return Err(e),
            }
        };
        // SAFETY: `msg` was just filled in by a successful `recvmsg`.
        let (stamps, ee_id) = unsafe { parse_cmsgs(&msg) };
        if errqueue {
            return Ok(Some(match ee_id {
                Some(id) => Received::TxStamp {
                    id,
                    len: n.min(buf.len()),
                    stamps,
                },
                None => Received::Other,
            }));
        }
        let len = if msg.msg_flags & libc::MSG_TRUNC != 0 {
            buf.len()
        } else {
            n.min(buf.len())
        };
        Ok(Some(Received::Data { len, stamps }))
    }
}

fn ts_ns(b: &[u8]) -> Option<u64> {
    let sec = i64::from_ne_bytes(b.get(0..8)?.try_into().ok()?);
    let nsec = i64::from_ne_bytes(b.get(8..16)?.try_into().ok()?);
    if sec == 0 && nsec == 0 {
        return None;
    }
    u64::try_from(sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(nsec).ok()?)
}

/// Walks the control messages of a received `msghdr`.
///
/// # Safety
/// `msg` must have been filled in by a successful `recvmsg` whose control
/// buffer is still alive and unchanged: `msg_control`/`msg_controllen` must
/// describe valid, initialised control data.
unsafe fn parse_cmsgs(msg: &libc::msghdr) -> (Stamps, Option<u32>) {
    let mut stamps = Stamps::default();
    let mut ee_id = None;
    // SAFETY: `msg` was filled in by a successful `recvmsg`, so
    // `msg_control`/`msg_controllen` describe valid, initialised control
    // data; the CMSG_* macros only inspect within that range.
    let mut hdr = unsafe { libc::CMSG_FIRSTHDR(msg) };
    while !hdr.is_null() {
        // SAFETY: `hdr` is non-null and returned by CMSG_FIRSTHDR/NXTHDR for
        // this `msg`, so it points at a complete `cmsghdr` followed by
        // `cmsg_len - CMSG_LEN(0)` bytes of data inside the control buffer.
        let (level, ty, data) = unsafe {
            let h = &*hdr;
            let hdr_len = libc::CMSG_LEN(0) as usize;
            let data_len = (h.cmsg_len as usize).saturating_sub(hdr_len);
            (
                h.cmsg_level,
                h.cmsg_type,
                core::slice::from_raw_parts(libc::CMSG_DATA(hdr), data_len),
            )
        };
        if level == libc::SOL_SOCKET && (ty == SCM_TIMESTAMPING_NEW) {
            // struct scm_timestamping { struct timespec ts[3]; } with 64-bit
            // fields: [0] software, [1] legacy, [2] raw hardware.
            stamps.software_ns = data.get(0..16).and_then(ts_ns);
            stamps.hardware_ns = data.get(32..48).and_then(ts_ns);
        } else if level == libc::SOL_IP && ty == IP_RECVERR {
            // struct sock_extended_err { u32 errno; u8 origin, type, code,
            // pad; u32 info; u32 data; }
            let origin = data.get(4).copied();
            let info = data
                .get(8..12)
                .and_then(|b| b.try_into().ok())
                .map(u32::from_ne_bytes);
            let id = data
                .get(12..16)
                .and_then(|b| b.try_into().ok())
                .map(u32::from_ne_bytes);
            // info 0 = SCM_TSTAMP_SND (software/hardware transmit stamp).
            if origin == Some(SO_EE_ORIGIN_TIMESTAMPING) && info == Some(0) {
                ee_id = id;
            }
        }
        // SAFETY: `hdr` is a valid header for `msg` (see above).
        hdr = unsafe { libc::CMSG_NXTHDR(msg, hdr) };
    }
    (stamps, ee_id)
}
