//! Socket-bound connector incarnation. Kernel credentials identify the process
//! that connected, not every later byte writer of an inherited/transferred fd.
//! A live process handle is a finite observation, not an atomic write barrier.
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::MetadataExt;

use nix::libc;
use nix::sys::socket::{
    getsockopt,
    sockopt::{PeerCredentials, PeerPidfd},
};

use crate::peer::PeerCred;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessKey {
    device: u64,
    inode: u64,
}

/// Holds both handles so neither socket nor process key can be recycled while
/// evidence is retained. It has no public wire representation and grants no
/// permission. It deliberately supplies no numeric-PID fallback.
#[derive(Debug)]
pub(crate) struct ConnectedProcess {
    socket: OwnedFd,
    process: OwnedFd,
    key: ProcessKey,
    credentials: PeerCred,
}

/// A process-only diagnostic retention. It owns no socket and carries no role,
/// parentage, unit, writer or launch authority.
#[derive(Debug)]
pub(crate) struct RetainedProcess {
    process: OwnedFd,
    key: ProcessKey,
}
impl RetainedProcess {
    pub(crate) fn key(&self) -> ProcessKey {
        self.key
    }
    pub(crate) fn still_live(&self) -> io::Result<()> {
        live(self.process.as_fd())
    }
}

fn live(fd: BorrowedFd<'_>) -> io::Result<()> {
    let mut process = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: this pollfd's borrowed descriptor stays live for the call.
    match unsafe { libc::poll(&raw mut process, 1, 0) } {
        -1 => Err(io::Error::last_os_error()),
        0 if process.revents == 0 => Ok(()),
        _ => Err(invalid(
            "retained process exited or its lifetime is unavailable",
        )),
    }
}

fn key(fd: BorrowedFd<'_>) -> io::Result<ProcessKey> {
    let metadata = File::from(fd.try_clone_to_owned()?).metadata()?;
    Ok(ProcessKey {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}
fn process_key(fd: BorrowedFd<'_>) -> io::Result<ProcessKey> {
    // SO_PEERPIDFD predates pidfs: older anonymous pidfd inodes are shared
    // across processes. Require pidfs before exposing an incarnation key.
    // Linux UAPI linux/magic.h names 0x50494446 ("PIDF") PID_FS_MAGIC.
    if nix::sys::statfs::fstatfs(fd)
        .map_err(io::Error::from)?
        .filesystem_type()
        .0
        != 0x5049_4446
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "pidfs process identity is unavailable",
        ));
    }
    key(fd)
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn canonical(mut credentials: PeerCred) -> PeerCred {
    credentials.groups.sort_unstable();
    credentials.groups.dedup();
    credentials
}

impl ConnectedProcess {
    pub(crate) fn pid(&self) -> Option<i32> {
        self.credentials.pid
    }
    #[cfg(feature = "routing-shadow")]
    pub(crate) fn retain_process(&self) -> io::Result<RetainedProcess> {
        self.still_live()?;
        let result = RetainedProcess {
            process: self.process.try_clone()?,
            key: self.key,
        };
        result.still_live()?;
        Ok(result)
    }
    pub(crate) fn capture(socket: BorrowedFd<'_>) -> io::Result<Self> {
        // Obtain the socket-bound pidfd first. Opening a pidfd from SO_PEERCRED's
        // numeric PID later can bind a reused PID to the wrong incarnation.
        let process = getsockopt(&socket, PeerPidfd).map_err(io::Error::from)?;
        let credentials = getsockopt(&socket, PeerCredentials).map_err(io::Error::from)?;
        if credentials.pid() <= 0 {
            return Err(invalid("connector PID is unavailable"));
        }
        let credentials = canonical(PeerCred {
            uid: credentials.uid(),
            gid: credentials.gid(),
            pid: Some(credentials.pid()),
            groups: super::peer_groups(socket)?,
        });
        let result = Self {
            key: process_key(process.as_fd())?,
            process,
            socket: socket.try_clone_to_owned()?,
            credentials,
        };
        result.still_live()?;
        Ok(result)
    }
    pub(crate) fn key(&self) -> ProcessKey {
        self.key
    }
    pub(crate) fn matches_credentials(&self, credentials: &PeerCred) -> io::Result<()> {
        if self.credentials != canonical(credentials.clone()) {
            return Err(invalid(
                "connector credentials do not match this connection",
            ));
        }
        self.still_live()
    }
    pub(crate) fn matches_socket(&self, socket: BorrowedFd<'_>) -> io::Result<()> {
        if key(self.socket.as_fd())? != key(socket)? {
            return Err(invalid("connector evidence belongs to another socket"));
        }
        self.still_live()
    }
    pub(crate) fn still_live(&self) -> io::Result<()> {
        live(self.process.as_fd())
    }
}

// Concrete process observations. Authority additionally needs manager-verified
// launch registration; this collector alone grants no rights.
pub(crate) mod launch;
