//! Installed executable identity, pinned with O_PATH handles. The runtime image
//! stays metadata-only; only our own integration images expose build probes.
//! Matching an image identifies a diagnostic role, never a writer or holder.
use super::*;

#[derive(Debug)]
struct Entry {
    key: ProcessKey,
    _handle: File,
}

#[derive(Debug)]
pub(crate) struct RoleTable {
    entries: [Option<Entry>; 4],
    #[cfg(test)]
    pub(super) fixture_singleton: bool,
    #[cfg(test)]
    pub(super) walks: std::sync::atomic::AtomicU64,
}

impl RoleTable {
    /// Only Tessera's own launcher, adapter and CLI may be build-probed. The
    /// compatibility runtime's handle is metadata-only and is never exposed.
    pub(crate) fn own_build_images(&self) -> Option<[(&'static str, &File); 3]> {
        Some([
            ("launcher", &self.entries[0].as_ref()?._handle),
            ("report-adapter", &self.entries[2].as_ref()?._handle),
            ("cli", &self.entries[3].as_ref()?._handle),
        ])
    }

    /// A registration pins the complete role set, including absent roles.
    /// Installing/replacing any image requires a new runtime registration.
    pub(crate) fn same_images(&self, other: &Self) -> bool {
        self.entries
            .iter()
            .zip(&other.entries)
            .all(|(a, b)| a.as_ref().map(|entry| entry.key) == b.as_ref().map(|entry| entry.key))
    }

    pub(crate) fn installed() -> Result<Self, Failure> {
        Self::resolve(
            [
                Path::new("/usr/bin/tessera-controller-start"),
                Path::new("/usr/lib/tessera/cmc-probe"),
                Path::new("/usr/bin/tessera-controller-command"),
                Path::new("/usr/bin/tesseractl"),
            ],
            true,
        )
    }

    #[cfg(test)]
    pub(super) fn empty() -> Self {
        Self {
            entries: std::array::from_fn(|_| None),
            fixture_singleton: false,
            walks: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn resolve(paths: [&Path; 4], check_parents: bool) -> Result<Self, Failure> {
        let mut entries: [Option<Entry>; 4] = std::array::from_fn(|_| None);
        for (index, path) in paths.into_iter().enumerate() {
            let entry = match lookup(path, check_parents) {
                Ok(entry) => Some(entry),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(_) => return Err(Failure::Unavailable),
            };
            if entry.as_ref().is_some_and(|entry| {
                entries
                    .iter()
                    .flatten()
                    .any(|previous| previous.key == entry.key)
            }) {
                return Err(Failure::ImageAliased);
            }
            entries[index] = entry;
        }
        Ok(Self {
            entries,
            #[cfg(test)]
            fixture_singleton: false,
            #[cfg(test)]
            walks: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Own fixture images only. Singleton injection is explicitly test context,
    /// never a production cgroup or prior-runtime-absence observation.
    #[cfg(test)]
    pub(crate) fn fixture(paths: [&Path; 4], singleton: bool) -> Result<Self, Failure> {
        let mut table = Self::resolve(paths, false)?;
        table.fixture_singleton = singleton;
        Ok(table)
    }

    pub(super) fn classify(&self, image: ProcessKey) -> Image {
        let roles = [
            Image::Launcher,
            Image::Runtime,
            Image::ReportAdapter,
            Image::ForwardedCli,
        ];
        self.entries
            .iter()
            .zip(roles)
            .find_map(|(entry, role)| {
                entry
                    .as_ref()
                    .filter(|entry| entry.key == image)
                    .map(|_| role)
            })
            .unwrap_or(Image::Other)
    }
}

fn lookup(path: &Path, check_parents: bool) -> io::Result<Entry> {
    if !path.is_absolute() {
        return Err(invalid("role image path must be absolute"));
    }
    let mut directory = File::open("/")?;
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            if component == Component::RootDir {
                continue;
            }
            return Err(invalid("unsupported role path component"));
        };
        if check_parents {
            let metadata = directory.metadata()?;
            if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
                return Err(invalid("role parent directory is not trusted"));
            }
        }
        let last = components.peek().is_none();
        let flags = if last {
            OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
        } else {
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
        };
        let handle =
            File::from(openat(&directory, name, flags, Mode::empty()).map_err(io::Error::from)?);
        if last {
            let metadata = handle.metadata()?;
            if !metadata.is_file()
                || metadata.mode() & 0o022 != 0
                || metadata.mode() & 0o111 == 0
                || (check_parents && metadata.uid() != 0)
            {
                return Err(invalid("role image metadata is not trusted"));
            }
            return Ok(Entry {
                key: ProcessKey {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                },
                _handle: handle,
            });
        }
        directory = handle;
    }
    Err(invalid("missing role image"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn roles_have_distinct_pinned_inodes_and_ambiguous_tables_refuse() {
        let scratch = super::super::tests::Scratch::new();
        let paths = ["launcher", "runtime", "adapter", "cli"].map(|name| scratch.0.join(name));
        for path in &paths {
            std::fs::write(path, "own role fixture\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let table = RoleTable::fixture(paths.each_ref().map(|p| p.as_path()), false).unwrap();
        let unchanged = RoleTable::resolve(paths.each_ref().map(|p| p.as_path()), false).unwrap();
        assert!(table.same_images(&unchanged));
        for (entry, expected) in table.entries.iter().zip([
            Image::Launcher,
            Image::Runtime,
            Image::ReportAdapter,
            Image::ForwardedCli,
        ]) {
            assert_eq!(table.classify(entry.as_ref().unwrap().key), expected);
        }
        std::fs::remove_file(&paths[1]).unwrap();
        std::fs::write(&paths[1], "replacement runtime fixture\n").unwrap();
        std::fs::set_permissions(&paths[1], std::fs::Permissions::from_mode(0o755)).unwrap();
        let changed = RoleTable::resolve(paths.each_ref().map(|p| p.as_path()), false).unwrap();
        assert!(!table.same_images(&changed));
        std::fs::remove_file(&paths[1]).unwrap();
        std::fs::hard_link(&paths[0], &paths[1]).unwrap();
        assert_eq!(
            RoleTable::resolve(paths.each_ref().map(|p| p.as_path()), false).unwrap_err(),
            Failure::ImageAliased
        );
        // O_PATH handles still pin all original inodes after unlink/replacement.
        assert_eq!(
            table.classify(table.entries[1].as_ref().unwrap().key),
            Image::Runtime
        );
        std::fs::remove_file(&paths[1]).unwrap();
        std::os::unix::fs::symlink(&paths[0], &paths[1]).unwrap();
        assert_eq!(
            RoleTable::resolve(paths.each_ref().map(|p| p.as_path()), false).unwrap_err(),
            Failure::Unavailable
        );
    }
}
