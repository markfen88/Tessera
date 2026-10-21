//! The one place `tesserad` needs `unsafe` (ADR 0002): `SO_PEERGROUPS`, which
//! `nix` does not wrap.
#![allow(unsafe_code)]

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

use nix::libc;

/// Largest group list accepted, in bytes (the kernel limit is 65536 groups).
const MAX_BYTES: usize = 1 << 20;

/// The supplementary groups of the process on the other end of a connected
/// Unix socket, as recorded by the kernel when it connected
/// (`SO_PEERGROUPS`, Linux 4.13). Not subject to NSS, so it never blocks.
///
/// # Errors
/// The socket option is not supported or the call failed.
pub fn peer_groups(fd: BorrowedFd<'_>) -> io::Result<Vec<u32>> {
    let mut entries = 256usize;
    loop {
        let mut buf = vec![0u32; entries];
        let mut len = libc::socklen_t::try_from(entries * 4).unwrap_or(libc::socklen_t::MAX);
        // SAFETY: `buf` is a live, writable allocation of `len` bytes; `len`
        // is a valid in/out pointer; `fd` is a valid open descriptor for the
        // duration of the call. The kernel writes at most `len` bytes and
        // updates `len` to the number written (or required, with ERANGE).
        let rc = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERGROUPS,
                buf.as_mut_ptr().cast(),
                &raw mut len,
            )
        };
        if rc == 0 {
            buf.truncate((len as usize) / 4);
            return Ok(buf);
        }
        let err = io::Error::last_os_error();
        let need = len as usize;
        if err.raw_os_error() == Some(libc::ERANGE) && need > entries * 4 && need <= MAX_BYTES {
            entries = need / 4 + 1;
            continue;
        }
        return Err(err);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn a_socketpair_reports_our_own_supplementary_groups() {
        let (a, _b) = UnixStream::pair().unwrap();
        let got = peer_groups(a.as_fd()).expect("SO_PEERGROUPS");
        let mine: Vec<u32> = nix::unistd::getgroups()
            .unwrap()
            .into_iter()
            .map(|g| g.as_raw())
            .collect();
        for g in &mine {
            assert!(got.contains(g), "group {g} missing from {got:?}");
        }
    }

    #[test]
    fn a_non_socket_is_an_error_not_a_panic() {
        let f = std::fs::File::open("/dev/null").unwrap();
        assert!(peer_groups(f.as_fd()).is_err());
    }
}
