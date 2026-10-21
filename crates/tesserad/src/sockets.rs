//! Creating the listening sockets with the right owner and mode, and making
//! sure only one daemon owns them (ADR 0010).
//!
//! * [`InstanceLock`]: an exclusive `flock` on `<runtime dir>/tesserad.lock`
//!   held for the life of the process. Whoever holds it is *the* daemon; only
//!   the holder may unlink or replace sockets in that directory, and nothing
//!   else (state files included) is touched before the lock is taken.
//! * [`bind`]: a stale socket (a file nobody listens on: `connect` fails with
//!   `ECONNREFUSED`) is replaced; a live one, a regular file, or any other
//!   connect error leaves the path alone and fails.
//! * [`Bound::remove_if_ours`]: at exit only a socket file that is still the
//!   inode we created is unlinked.

use std::fmt;
use std::fs::File;
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};
use nix::unistd::{Gid, chown};

/// Why a socket could not be created.
#[derive(Debug)]
pub enum SocketError {
    /// Another daemon runs here (it holds the lock or answers on the socket).
    InUse(PathBuf),
    /// Something that is not a socket is in the way; it is left alone.
    NotASocket(PathBuf),
    /// The path exists but its state is unclear (not refusing connections);
    /// it is left alone.
    Unclear(PathBuf, io::Error),
    /// The group could not be applied and was requested explicitly.
    Group(PathBuf, io::Error),
    /// An I/O call failed.
    Io(&'static str, PathBuf, io::Error),
}

impl fmt::Display for SocketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InUse(p) => write!(
                f,
                "{} is in use by a running daemon (refusing to start a second one)",
                p.display()
            ),
            Self::NotASocket(p) => write!(f, "{} exists and is not a socket", p.display()),
            Self::Unclear(p, e) => write!(
                f,
                "{} exists and is not a stale socket (connecting says: {e}); not touching it",
                p.display()
            ),
            Self::Group(p, e) => write!(f, "cannot set the group of {}: {e}", p.display()),
            Self::Io(what, p, e) => write!(f, "cannot {what} {}: {e}", p.display()),
        }
    }
}

impl std::error::Error for SocketError {}

/// How the socket's group is set.
#[derive(Debug, Clone, Copy)]
pub struct GroupSpec {
    /// The group, if it exists.
    pub gid: Option<Gid>,
    /// Set explicitly by the operator: failure to apply it is fatal instead of
    /// a warning.
    pub explicit: bool,
}

/// The exclusive instance lock. Dropping it (or exiting) releases it.
#[derive(Debug)]
pub struct InstanceLock {
    _lock: Flock<File>,
}

impl InstanceLock {
    /// Take `<dir>/tesserad.lock`, creating `dir` if needed.
    ///
    /// # Errors
    /// [`SocketError::InUse`] if another process holds it.
    pub fn acquire(dir: &Path) -> Result<Self, SocketError> {
        std::fs::create_dir_all(dir)
            .map_err(|e| SocketError::Io("create the runtime directory", dir.to_owned(), e))?;
        let path = dir.join("tesserad.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| SocketError::Io("open", path.clone(), e))?;
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => Ok(Self { _lock: lock }),
            Err((_, nix::errno::Errno::EWOULDBLOCK)) => Err(SocketError::InUse(path)),
            Err((_, e)) => Err(SocketError::Io("lock", path, e.into())),
        }
    }
}

/// Identity of a socket file we created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketId {
    dev: u64,
    ino: u64,
}

/// A listening socket and the identity of its file.
#[derive(Debug)]
pub struct Bound {
    /// The listener (non-blocking).
    pub listener: UnixListener,
    /// The file, for cleanup.
    pub file: BoundFile,
}

impl Bound {
    /// Split into the listener and the file handle.
    #[must_use]
    pub fn into_parts(self) -> (UnixListener, BoundFile) {
        (self.listener, self.file)
    }
}

/// The socket file we created.
#[derive(Debug, Clone)]
pub struct BoundFile {
    path: PathBuf,
    id: SocketId,
}

impl BoundFile {
    /// Unlink the socket file, but only if it is still the one we created
    /// (another daemon may have replaced it, for example after a crash
    /// recovery by an operator). Returns whether it was removed.
    pub fn remove_if_ours(&self) -> bool {
        match std::fs::symlink_metadata(&self.path) {
            Ok(md)
                if md.file_type().is_socket()
                    && (md.dev(), md.ino()) == (self.id.dev, self.id.ino) =>
            {
                std::fs::remove_file(&self.path).is_ok()
            }
            _ => false,
        }
    }
}

/// Remove a leftover socket file, but only if nobody is listening on it:
/// `connect` must fail with exactly `ECONNREFUSED`. Call with the instance
/// lock held.
fn clear_stale(path: &Path) -> Result<(), SocketError> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(SocketError::Io("inspect", path.to_owned(), e)),
    };
    if !md.file_type().is_socket() {
        return Err(SocketError::NotASocket(path.to_owned()));
    }
    match UnixStream::connect(path) {
        Ok(_) => Err(SocketError::InUse(path.to_owned())),
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => std::fs::remove_file(path)
            .map_err(|e| SocketError::Io("remove the stale socket", path.to_owned(), e)),
        Err(e) => Err(SocketError::Unclear(path.to_owned(), e)),
    }
}

/// Bind a non-blocking Unix stream listener at `path` with permission bits
/// `mode` and group `group`.
///
/// The socket is created inside a private (0700) scratch directory next to
/// `path`, given its mode and group there, and only then renamed into place.
/// It therefore never exists at its public path with wider permissions than
/// requested, without touching the process-wide umask. The caller holds the
/// [`InstanceLock`].
///
/// # Errors
/// [`SocketError`].
pub fn bind(path: &Path, mode: u32, group: GroupSpec) -> Result<Bound, SocketError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| SocketError::Io("create the directory for", path.to_owned(), e))?;
    clear_stale(path)?;
    let scratch = parent.join(format!(".tessera-bind-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&scratch)
        .map_err(|e| SocketError::Io("create a scratch directory in", parent.to_owned(), e))?;
    let result = bind_in(&scratch.join("s"), path, mode, group);
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

fn bind_in(tmp: &Path, path: &Path, mode: u32, group: GroupSpec) -> Result<Bound, SocketError> {
    let listener =
        UnixListener::bind(tmp).map_err(|e| SocketError::Io("bind", path.to_owned(), e))?;
    if let Some(gid) = group.gid {
        match chown(tmp, None, Some(gid)) {
            Ok(()) => {}
            Err(e) if group.explicit => return Err(SocketError::Group(path.to_owned(), e.into())),
            Err(e) => tracing::warn!(
                "cannot give {} to group {}: {e}; leaving the owning group",
                path.display(),
                gid
            ),
        }
    }
    std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(mode))
        .map_err(|e| SocketError::Io("set the mode of", path.to_owned(), e))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| SocketError::Io("configure", path.to_owned(), e))?;
    let md = std::fs::symlink_metadata(tmp)
        .map_err(|e| SocketError::Io("inspect", path.to_owned(), e))?;
    let id = SocketId {
        dev: md.dev(),
        ino: md.ino(),
    };
    std::fs::rename(tmp, path)
        .map_err(|e| SocketError::Io("move the socket into place at", path.to_owned(), e))?;
    Ok(Bound {
        listener,
        file: BoundFile {
            path: path.to_owned(),
            id,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Removed when dropped, also when the test fails.
    struct Tmp(PathBuf);

    impl std::ops::Deref for Tmp {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl AsRef<Path> for Tmp {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(name: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("tesserad-sock-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    const NO_GROUP: GroupSpec = GroupSpec {
        gid: None,
        explicit: false,
    };

    #[test]
    fn no_scratch_directory_is_left_behind() {
        let d = tmp("scratch");
        let p = d.join("s.sock");
        let _l = bind(&p, 0o660, NO_GROUP).unwrap();
        let names: Vec<String> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["s.sock"]);
    }

    #[test]
    fn socket_gets_the_requested_mode() {
        let d = tmp("mode");
        let p = d.join("s.sock");
        let _l = bind(&p, 0o660, NO_GROUP).unwrap();
        let m = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(m, 0o660);
    }

    #[test]
    fn stale_socket_is_replaced_and_live_one_is_refused() {
        let d = tmp("stale");
        let p = d.join("s.sock");
        let l = bind(&p, 0o660, NO_GROUP).unwrap();
        assert!(matches!(
            bind(&p, 0o660, NO_GROUP),
            Err(SocketError::InUse(_))
        ));
        // Leave the file behind as a crash would, then rebind.
        drop(l);
        assert!(p.exists());
        let _l2 = bind(&p, 0o660, NO_GROUP).unwrap();
    }

    #[test]
    fn regular_files_are_never_deleted() {
        let d = tmp("file");
        let p = d.join("s.sock");
        std::fs::write(&p, b"precious").unwrap();
        assert!(matches!(
            bind(&p, 0o660, NO_GROUP),
            Err(SocketError::NotASocket(_))
        ));
        assert_eq!(std::fs::read(&p).unwrap(), b"precious");
    }

    #[test]
    fn a_socket_that_does_not_refuse_is_not_unlinked() {
        // A socket whose backlog is full or that is not a stream socket fails
        // `connect` with something other than ECONNREFUSED; it must stay.
        let d = tmp("unclear");
        let p = d.join("s.sock");
        let dg = std::os::unix::net::UnixDatagram::bind(&p).unwrap();
        let r = bind(&p, 0o660, NO_GROUP);
        assert!(matches!(r, Err(SocketError::Unclear(..))), "{r:?}");
        assert!(p.exists());
        drop(dg);
    }

    #[test]
    fn only_the_holder_of_the_lock_may_start() {
        let d = tmp("lock");
        let first = InstanceLock::acquire(&d).unwrap();
        assert!(matches!(
            InstanceLock::acquire(&d),
            Err(SocketError::InUse(_))
        ));
        drop(first);
        assert!(InstanceLock::acquire(&d).is_ok());
    }

    #[test]
    fn removal_at_exit_only_touches_our_own_inode() {
        let d = tmp("ours");
        let p = d.join("s.sock");
        let mine = bind(&p, 0o660, NO_GROUP).unwrap().file;
        // Someone replaces the file behind our back.
        std::fs::remove_file(&p).unwrap();
        let other = UnixListener::bind(&p).unwrap();
        assert!(!mine.remove_if_ours(), "must not remove a foreign socket");
        assert!(p.exists());
        drop(other);
        // Our own file goes.
        let _ = std::fs::remove_file(&p);
        let mine = bind(&p, 0o660, NO_GROUP).unwrap().file;
        assert!(mine.remove_if_ours());
        assert!(!p.exists());
    }
}
