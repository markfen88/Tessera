//! The access lock of another device, read before routing it
//! (`device-lock.md` §7a, §9.3 L14): board info (0x0061) for the lockable
//! bit, then, only for a lockable device, the lock state (0x1008), on its
//! device-info port. Devices answer to the info group 224.0.0.231:8702 (some,
//! tessera among them, also to the asker): both are heard, from the target's
//! address only. Content 0x00 of 0x1009 not zero is locked (`guess` for the
//! value, `likely` for the position). No answer in time is "unknown": the
//! routing view stays as before, and the device's own ARC server still refuses
//! a write while it is locked. One request each, no resend.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;

use super::Target;
use crate::native::wire;

/// The device-info port of every device seen (`device-lock.md` §2; CMC
/// 0x1001 could tell another).
pub(crate) const INFO_PORT: u16 = 8700;
/// Where devices send their info replies (`conmon.md` §1).
const INFO_GROUP: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(224, 0, 0, 231), 8702);
/// Board-info capability bit "supports device lock" (`device-lock.md` §3).
const LOCKABLE: u32 = 0x0800_0000;
/// How long one answer may take; real devices answer within a few ms.
const STEP: Duration = Duration::from_millis(400);

/// `Some(true)` when the device at `target` reports itself locked,
/// `Some(false)` when it reports unlocked or cannot be locked, `None` when it
/// did not answer in time (or before `deadline`).
pub(crate) async fn read(target: &Target, deadline: Instant) -> Option<bool> {
    read_at(target, INFO_PORT, deadline).await
}

#[cfg(target_os = "linux")]
pub(crate) async fn read_at(target: &Target, info_port: u16, deadline: Instant) -> Option<bool> {
    let info = crate::netif::lookup(&target.interface).ok()?;
    if info.addr != target.source {
        return None;
    }
    let unicast = unicast_socket(target).ok()?;
    // Not every host lets a second socket join the group: unicast answers
    // alone then.
    let group = group_socket(target).ok();
    let mut asker = [0u8; 8];
    asker[..6].copy_from_slice(&info.mac);
    let to = SocketAddrV4::new(*target.peer.ip(), info_port);
    let query = |request, answer| {
        ask(
            &unicast,
            group.as_ref(),
            to,
            asker,
            request,
            answer,
            deadline,
        )
    };
    let board = query(0x0061, 0x0060).await?;
    let capabilities = u32::from_be_bytes(board.get(32 + 0x14..32 + 0x18)?.try_into().ok()?);
    if capabilities & LOCKABLE == 0 {
        return Some(false);
    }
    let state = query(0x1008, 0x1009).await?;
    Some(wire::be16(&state, 32)? != 0)
}

#[cfg(not(target_os = "linux"))]
pub(crate) async fn read_at(_: &Target, _: u16, _: Instant) -> Option<bool> {
    None
}

/// Send info request `request` to `to` and wait for its answer `answer` from
/// the same address, on either socket.
async fn ask(
    unicast: &UdpSocket,
    group: Option<&UdpSocket>,
    to: SocketAddrV4,
    asker: [u8; 8],
    request: u16,
    answer: u16,
    deadline: Instant,
) -> Option<Vec<u8>> {
    let m = wire::info_message(
        0xFFFF,
        super::sequence(),
        asker,
        b"tessera ",
        wire::info_opcode(request),
        &[],
    );
    unicast.send_to(&m, to).await.ok()?;
    let until = (Instant::now() + STEP).min(deadline);
    let wait = async {
        let (mut a, mut b) = (vec![0u8; 2048], vec![0u8; 2048]);
        loop {
            let (bytes, from) = tokio::select! {
                r = unicast.recv_from(&mut a) => match r {
                    Ok((n, from)) => (&a[..n], from),
                    Err(_) => continue,
                },
                r = async {
                    match group {
                        Some(g) => g.recv_from(&mut b).await,
                        None => std::future::pending().await,
                    }
                } => match r {
                    Ok((n, from)) => (&b[..n], from),
                    Err(_) => continue,
                },
            };
            if from.ip() == SocketAddr::V4(to).ip() && wire::info_request(bytes) == Some(answer) {
                return bytes.to_vec();
            }
        }
    };
    tokio::time::timeout_at(until.into(), wait).await.ok()
}

#[cfg(target_os = "linux")]
fn unicast_socket(target: &Target) -> std::io::Result<UdpSocket> {
    let socket = std::net::UdpSocket::bind(SocketAddrV4::new(target.source, 0))?;
    nix::sys::socket::setsockopt(
        &socket,
        nix::sys::socket::sockopt::BindToDevice,
        &std::ffi::OsString::from(&target.interface),
    )?;
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket)
}

/// A socket on the info group, joined on the target's interface address;
/// shared with other listeners of the group (`SO_REUSEADDR`).
#[cfg(target_os = "linux")]
fn group_socket(target: &Target) -> std::io::Result<UdpSocket> {
    use nix::sys::socket::{
        AddressFamily, IpMembershipRequest, SockFlag, SockType, SockaddrIn, bind, setsockopt,
        socket, sockopt,
    };
    use std::os::fd::AsRawFd as _;
    let fd = socket(
        AddressFamily::Inet,
        SockType::Datagram,
        SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    let socket = std::net::UdpSocket::from(fd);
    setsockopt(&socket, sockopt::ReuseAddr, &true)?;
    setsockopt(
        &socket,
        sockopt::BindToDevice,
        &std::ffi::OsString::from(&target.interface),
    )?;
    bind(socket.as_raw_fd(), &SockaddrIn::from(INFO_GROUP))?;
    setsockopt(
        &socket,
        sockopt::IpAddMembership,
        &IpMembershipRequest::new(*INFO_GROUP.ip(), Some(target.source)),
    )?;
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use tessera_control_types::EndpointRef;

    fn target(peer_port: u16) -> Target {
        Target {
            endpoint: EndpointRef::new("remote", "g1"),
            label: "Remote".into(),
            interface: "lo".into(),
            source: Ipv4Addr::LOCALHOST,
            peer: SocketAddrV4::new(Ipv4Addr::LOCALHOST, peer_port),
            profile: tessera_control::arc::Legacy::from_advertised_version("2.7.41").unwrap(),
        }
    }

    /// A device on the loopback that answers 0x0061 with `capabilities` and
    /// 0x1008 with `locked`, unicast; the requests it saw.
    async fn device(capabilities: u32, locked: bool) -> (u16, tokio::task::JoinHandle<Vec<u16>>) {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let mut seen = Vec::new();
            let mut buf = vec![0u8; 2048];
            while let Ok(Ok((n, from))) =
                tokio::time::timeout(Duration::from_millis(500), socket.recv_from(&mut buf)).await
            {
                let Some(request) = wire::info_request(&buf[..n]) else {
                    continue;
                };
                seen.push(request);
                let (id, content) = match request {
                    0x0061 => {
                        let mut c = vec![0u8; 200];
                        c[0x14..0x18].copy_from_slice(&capabilities.to_be_bytes());
                        (0x0060, c)
                    }
                    0x1008 => {
                        // The 0x1009 forms of `device-lock.md` §9.3 L10.
                        let mut c = vec![0u8; 16];
                        c[1] = u8::from(locked);
                        c[7] = 8;
                        (0x1009, c)
                    }
                    _ => continue,
                };
                let m = wire::info_message(
                    0xFFFF,
                    1,
                    [0; 8],
                    b"device  ",
                    wire::info_opcode(id),
                    &content,
                );
                let _ = socket.send_to(&m, from).await;
            }
            seen
        });
        (port, task)
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(3)
    }

    #[tokio::test]
    async fn a_lockable_device_is_asked_for_its_lock_state() {
        for locked in [true, false] {
            let (port, seen) = device(0x0800_10DB, locked).await;
            assert_eq!(read_at(&target(4440), port, soon()).await, Some(locked));
            seen.abort();
        }
    }

    #[tokio::test]
    async fn a_device_without_the_lockable_bit_is_not_asked_and_counts_as_open() {
        let (port, seen) = device(0x0000_10DB, true).await;
        assert_eq!(read_at(&target(4440), port, soon()).await, Some(false));
        let seen = seen.await.unwrap();
        assert!(!seen.contains(&0x1008), "{seen:x?}");
    }

    #[tokio::test]
    async fn no_answer_is_unknown_within_the_deadline() {
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = silent.local_addr().unwrap().port();
        let started = Instant::now();
        assert_eq!(read_at(&target(4440), port, soon()).await, None);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
