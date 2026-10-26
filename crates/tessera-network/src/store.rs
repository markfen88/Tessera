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

/// Distinguish a busy mutation from an unclassified filesystem failure.
#[derive(Debug)]
pub enum LockError {
    /// Another helper job owns the mutation lock.
    Busy(String),
    /// Opening or validating the private lock failed.
    Other(String),
}

impl From<String> for LockError {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

impl Store {
    /// Boot-only upgrade of the old helper's empty root:tessera-web lock.
    /// Never repair journal records or replace a lock held by another job.
    pub fn migrate_legacy_lock(&self) -> Result<()> {
        if !nix::unistd::geteuid().is_root() {
            return Ok(());
        }
        let group = nix::unistd::Group::from_name("tessera-web")
            .map_err(|e| e.to_string())?
            .map(|group| group.gid);
        self.migrate_lock(group)
    }

    fn migrate_lock(&self, legacy_group: Option<nix::unistd::Gid>) -> Result<()> {
        self.prepare()?;
        let path = self.0.join("lock");
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if private_record(&metadata) {
            return Ok(());
        }
        if !metadata.is_file()
            || metadata.uid() != 0
            || Some(nix::unistd::Gid::from_raw(metadata.gid())) != legacy_group
            || metadata.mode() & 0o7777 != 0o600
            || metadata.nlink() != 1
            || metadata.len() != 0
        {
            return Err("unsafe network journal lock".into());
        }
        let old = Flock::lock(file, FlockArg::LockExclusiveNonblock)
            .map_err(|(_, e)| format!("legacy network journal lock is busy: {e}"))?;
        let temporary = self.0.join(format!(".lock-{}", token()?));
        let result = (|| {
            let new = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(nix::libc::O_NOFOLLOW)
                .open(&temporary)
                .map_err(|e| e.to_string())?;
            nix::unistd::fchown(&new, None, Some(private_group())).map_err(|e| e.to_string())?;
            let new = Flock::lock(new, FlockArg::LockExclusiveNonblock)
                .map_err(|(_, e)| e.to_string())?;
            new.sync_all().map_err(|e| e.to_string())?;
            let current = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            let locked = old.metadata().map_err(|e| e.to_string())?;
            if current.dev() != metadata.dev()
                || current.ino() != metadata.ino()
                || locked.uid() != metadata.uid()
                || locked.gid() != metadata.gid()
                || locked.mode() != metadata.mode()
                || locked.nlink() != 1
                || locked.len() != 0
            {
                return Err("legacy network journal lock changed during migration".into());
            }
            // Hold both inodes through the durable rename. The old group only
            // had ownership, never read/write permission on this empty file.
            fs::rename(&temporary, &path).map_err(|e| e.to_string())?;
            File::open(&self.0)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    /// Verify a private directory; an existing symlink is never traversed.
    pub fn prepare(&self) -> Result<()> {
        safe_parent(&self.0)?;
        match fs::DirBuilder::new().mode(0o700).create(&self.0) {
            Ok(()) => {
                nix::unistd::chown(&self.0, None, Some(private_group()))
                    .map_err(|e| e.to_string())?;
                // Persist newly created exchange/archive directory names before
                // a durable record is moved into them from another directory.
                File::open(self.0.parent().ok_or("missing journal parent")?)
                    .and_then(|f| f.sync_all())
                    .map_err(|e| e.to_string())?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.to_string()),
        }
        let m = fs::symlink_metadata(&self.0).map_err(|e| e.to_string())?;
        if !m.is_dir()
            || m.uid() != nix::unistd::geteuid().as_raw()
            || m.gid() != private_group().as_raw()
            || m.mode() & 0o7777 != 0o700
        {
            return Err("unsafe network journal directory".into());
        }
        Ok(())
    }

    /// Only mutations lock; status and pending remain readable during apply.
    pub fn lock(&self, wait: bool) -> Result<Flock<File>> {
        self.lock_typed(wait).map_err(|error| match error {
            LockError::Busy(message) | LockError::Other(message) => message,
        })
    }

    /// Acquire the same mutation lock while preserving the contention kind.
    pub fn lock_typed(&self, wait: bool) -> std::result::Result<Flock<File>, LockError> {
        self.prepare()?;
        let path = self.0.join("lock");
        let f = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(f) => {
                nix::unistd::fchown(&f, None, Some(private_group())).map_err(|e| e.to_string())?;
                f
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
                .open(&path)
                .map_err(|e| e.to_string())?,
            Err(e) => return Err(LockError::Other(e.to_string())),
        };
        let m = f.metadata().map_err(|e| e.to_string())?;
        if !private_record(&m) {
            return Err(LockError::Other("unsafe network journal lock".into()));
        }
        Flock::lock(
            f,
            if wait {
                FlockArg::LockExclusive
            } else {
                FlockArg::LockExclusiveNonblock
            },
        )
        .map_err(|(_, e)| {
            let message = format!("Another network operation is running: {e}");
            if e == nix::errno::Errno::EAGAIN {
                LockError::Busy(message)
            } else {
                LockError::Other(message)
            }
        })
    }

    /// Lock an existing peer without creating anything during handoff review.
    pub fn lock_existing(&self) -> std::result::Result<Flock<File>, LockError> {
        if self.text_existing("lock")?.is_none() {
            return Err(LockError::Other("missing peer mutation lock".into()));
        }
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(self.0.join("lock"))
            .map_err(|e| e.to_string())?;
        if !private_record(&f.metadata().map_err(|e| e.to_string())?) {
            return Err(LockError::Other("unsafe peer mutation lock".into()));
        }
        Flock::lock(f, FlockArg::LockExclusiveNonblock).map_err(|(_, e)| {
            if e == nix::errno::Errno::EAGAIN {
                LockError::Busy(e.to_string())
            } else {
                LockError::Other(e.to_string())
            }
        })
    }

    /// Read a private state file without following links, with a size cap.
    pub fn read<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        self.prepare()?;
        state_name(name)?;
        let path = self.0.join(name);
        let Some(text) = read_checked(&path, 2 * 1024 * 1024, true)? else {
            return Ok(None);
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("Invalid network state: {e}"))
    }

    /// Read an existing exchange without creating directories during import.
    pub fn read_existing<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        self.text_existing(name)?
            .map(|text| serde_json::from_str(&text).map_err(|e| e.to_string()))
            .transpose()
    }

    pub fn text_existing(&self, name: &str) -> Result<Option<String>> {
        state_name(name)?;
        match fs::symlink_metadata(&self.0) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.to_string()),
            Ok(m) => {
                safe_parent(&self.0)?;
                if !m.is_dir()
                    || m.uid() != nix::unistd::geteuid().as_raw()
                    || m.gid() != private_group().as_raw()
                    || m.mode() & 0o7777 != 0o700
                {
                    return Err("unsafe network exchange directory".into());
                }
            }
        }
        read_checked(&self.0.join(name), 2 * 1024 * 1024, true)
    }

    /// Move a private record into a prepared archive, retaining its bytes/mode.
    pub fn archive(&self, name: &str, archive: &Store) -> Result<()> {
        if self.text_existing(name)?.is_none() {
            return Ok(());
        }
        archive.prepare()?;
        if archive.text_existing(name)?.is_some() {
            return Err("archive destination already exists".into());
        }
        fs::rename(self.0.join(name), archive.0.join(name)).map_err(|e| e.to_string())?;
        for directory in [&self.0, &archive.0] {
            File::open(directory)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Atomic durable replacement; the directory remains root-private.
    pub fn write(&self, name: &str, value: &impl Serialize) -> Result<()> {
        self.prepare()?;
        state_name(name)?;
        let _ = read_checked(&self.0.join(name), 2 * 1024 * 1024, true)?;
        atomic(
            &self.0.join(name),
            &serde_json::to_string_pretty(value).map_err(|e| e.to_string())?,
            0o600,
        )
    }

    /// Durable deletion, after successful recovery or a committed transaction.
    pub fn remove(&self, name: &str) -> Result<()> {
        self.prepare()?;
        state_name(name)?;
        let _ = read_checked(&self.0.join(name), 2 * 1024 * 1024, true)?;
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
    read_checked(path, max, false)
}

fn private_group() -> nix::unistd::Gid {
    if nix::unistd::geteuid().is_root() {
        nix::unistd::Gid::from_raw(0)
    } else {
        nix::unistd::getegid()
    }
}

fn private_record(m: &fs::Metadata) -> bool {
    m.is_file()
        && m.uid() == nix::unistd::geteuid().as_raw()
        && m.gid() == private_group().as_raw()
        && m.mode() & 0o7777 == 0o600
        && m.nlink() == 1
}

fn state_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err("invalid network state filename".into());
    }
    Ok(())
}

fn read_checked(path: &Path, max: u64, private: bool) -> Result<Option<String>> {
    let f = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    safe_parent(path)?;
    let m = f.metadata().map_err(|e| e.to_string())?;
    if !m.is_file()
        || m.uid() != nix::unistd::geteuid().as_raw()
        || m.mode() & 0o022 != 0
        || m.len() > max
        || (private && !private_record(&m))
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
        nix::unistd::fchown(&f, None, Some(private_group())).map_err(|e| e.to_string())?;
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

/// Fixed work for all valid transaction tokens (NE-7.1).
pub fn token_equal(left: &str, right: &str) -> bool {
    if !valid_token(left) || !valid_token(right) {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    #[ignore = "requires root to recreate the exact legacy upgrade ownership"]
    fn legacy_lock_upgrade_is_exact_atomic_and_refuses_busy_or_unsafe_files() {
        assert!(nix::unistd::geteuid().is_root());
        let store = Store(std::env::temp_dir().join(format!("tessera-lock-{}", token().unwrap())));
        store.prepare().unwrap();
        let path = store.0.join("lock");
        let group = nix::unistd::Gid::from_raw(65534);
        let seed = || {
            fs::write(&path, "").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            nix::unistd::chown(&path, Some(nix::unistd::Uid::from_raw(0)), Some(group)).unwrap();
        };
        for bad in ["content", "mode", "owner", "group", "hardlink", "symlink"] {
            seed();
            let alias = store.0.join("alias");
            match bad {
                "content" => fs::write(&path, "not an empty lock").unwrap(),
                "mode" => fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap(),
                "owner" => nix::unistd::chown(&path, Some(nix::unistd::Uid::from_raw(65534)), None)
                    .unwrap(),
                "group" => {
                    nix::unistd::chown(&path, None, Some(nix::unistd::Gid::from_raw(1))).unwrap()
                }
                "hardlink" => fs::hard_link(&path, &alias).unwrap(),
                "symlink" => {
                    fs::rename(&path, &alias).unwrap();
                    symlink(&alias, &path).unwrap();
                }
                _ => unreachable!(),
            }
            let before = fs::symlink_metadata(&path).unwrap();
            let bytes = fs::read(&path).unwrap();
            assert!(store.migrate_lock(Some(group)).is_err(), "{bad}");
            let after = fs::symlink_metadata(&path).unwrap();
            assert_eq!(
                (before.ino(), before.mode(), before.uid(), before.gid()),
                (after.ino(), after.mode(), after.uid(), after.gid()),
                "{bad}"
            );
            assert_eq!(fs::read(&path).unwrap(), bytes);
            fs::remove_file(&path).unwrap();
            let _ = fs::remove_file(alias);
        }
        seed();
        let held =
            Flock::lock(File::open(&path).unwrap(), FlockArg::LockExclusiveNonblock).unwrap();
        assert!(
            store
                .migrate_lock(Some(group))
                .unwrap_err()
                .contains("busy")
        );
        drop(held);
        let inode = fs::metadata(&path).unwrap().ino();
        store.migrate_lock(Some(group)).unwrap();
        let migrated = fs::metadata(&path).unwrap();
        assert_ne!(inode, migrated.ino());
        assert!(private_record(&migrated));
        assert_eq!(migrated.len(), 0);
        store.migrate_lock(Some(group)).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().ino(), migrated.ino());
        let lock = store.lock(false).unwrap();
        assert!(matches!(store.lock_typed(false), Err(LockError::Busy(_))));
        drop(lock);
        fs::remove_dir_all(store.0).unwrap();
    }
}
