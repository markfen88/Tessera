//! Root-private write-ahead journal and atomic, durable configuration writes.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};
use serde::{Serialize, de::DeserializeOwned};

use crate::Result;

/// A directory whose owner is the helper's effective UID (root in production).
#[derive(Debug, Clone)]
pub struct Store(pub PathBuf);

impl Store {
    /// Verify a private directory; an existing symlink is never traversed.
    pub fn prepare(&self) -> Result<()> {
        match fs::DirBuilder::new().mode(0o700).create(&self.0) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.to_string()),
        }
        let m = fs::symlink_metadata(&self.0).map_err(|e| e.to_string())?;
        if !m.is_dir() || m.uid() != nix::unistd::geteuid().as_raw() || m.mode() & 0o777 != 0o700 {
            return Err("unsafe network journal directory".into());
        }
        Ok(())
    }

    /// Only mutations lock; status and pending remain readable during apply.
    pub fn lock(&self, wait: bool) -> Result<Flock<File>> {
        self.prepare()?;
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(self.0.join("lock"))
            .map_err(|e| e.to_string())?;
        let m = f.metadata().map_err(|e| e.to_string())?;
        if !m.is_file() || m.uid() != nix::unistd::geteuid().as_raw() || m.mode() & 0o077 != 0 {
            return Err("unsafe network journal lock".into());
        }
        Flock::lock(
            f,
            if wait {
                FlockArg::LockExclusive
            } else {
                FlockArg::LockExclusiveNonblock
            },
        )
        .map_err(|(_, e)| format!("Another network operation is running: {e}"))
    }

    /// Read a private state file without following links, with a size cap.
    pub fn read<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        self.prepare()?;
        let path = self.0.join(name);
        let Some(text) = read_regular(&path, 2 * 1024 * 1024)? else {
            return Ok(None);
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("Invalid network state: {e}"))
    }

    /// Atomic durable replacement; the directory remains root-private.
    pub fn write(&self, name: &str, value: &impl Serialize) -> Result<()> {
        self.prepare()?;
        atomic(
            &self.0.join(name),
            &serde_json::to_string_pretty(value).map_err(|e| e.to_string())?,
            0o600,
        )
    }

    /// Durable deletion, after successful recovery or a committed transaction.
    pub fn remove(&self, name: &str) -> Result<()> {
        match fs::remove_file(self.0.join(name)) {
            Ok(()) => File::open(&self.0)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Read only an owner-controlled regular file. Configuration is capped too.
pub fn read_regular(path: &Path, max: u64) -> Result<Option<String>> {
    let f = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let m = f.metadata().map_err(|e| e.to_string())?;
    if !m.is_file()
        || m.uid() != nix::unistd::geteuid().as_raw()
        || m.mode() & 0o022 != 0
        || m.len() > max
    {
        return Err(format!(
            "unsafe or oversized configuration file: {}",
            path.display()
        ));
    }
    let mut text = String::new();
    f.take(max + 1)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if text.len() as u64 > max {
        return Err("configuration file too large".into());
    }
    Ok(Some(text))
}

/// Refuse writable or linked ancestors of a privileged configuration target.
pub fn safe_parent(path: &Path) -> Result<()> {
    for parent in path.parent().ok_or("missing parent")?.ancestors() {
        let m = fs::symlink_metadata(parent).map_err(|e| e.to_string())?;
        if !m.is_dir()
            || (m.uid() != 0 && m.uid() != nix::unistd::geteuid().as_raw())
            || (m.mode() & 0o022 != 0 && !(m.uid() == 0 && m.mode() & 0o1000 != 0))
        {
            return Err(format!("unsafe configuration parent: {}", parent.display()));
        }
    }
    Ok(())
}

/// File + rename + parent fsync; no world-readable intermediate configuration.
pub fn atomic(path: &Path, text: &str, mode: u32) -> Result<()> {
    safe_parent(path)?;
    let _ = read_regular(path, 2 * 1024 * 1024)?;
    let suffix = token()?;
    let temporary = path.with_extension(format!("tmp-{suffix}"));
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        f.write_all(text.as_bytes())
            .and_then(|()| f.sync_all())
            .map_err(|e| e.to_string())?;
        fs::rename(&temporary, path).map_err(|e| e.to_string())?;
        File::open(path.parent().ok_or("missing parent")?)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

/// Opaque 256-bit transaction and temporary-file identity.
pub fn token() -> Result<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Never place unchecked strings in systemd unit names or helper arguments.
pub fn valid_token(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
