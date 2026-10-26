use crate::{Failure, Result};
use std::fs::{self, DirBuilder};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new(prefix: &str) -> Result<Self> {
        Self::in_directory(&std::env::temp_dir(), prefix)
    }

    pub(crate) fn in_directory(directory: &Path, prefix: &str) -> Result<Self> {
        for _ in 0..100 {
            let path = directory.join(format!(
                "{prefix}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut builder = DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(Failure::new("cannot allocate temporary directory"))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn regular_files(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !directory.exists() {
        return Ok(files);
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            files.extend(regular_files(&entry.path())?);
        } else if kind.is_file() {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_directory_is_private_and_removed_with_its_files() {
        let temp = TempDir::new("tessera-temp-test").unwrap();
        let path = temp.path().to_owned();
        fs::write(path.join("fixture"), "contents").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        drop(temp);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn regular_file_walk_does_not_follow_package_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = TempDir::new("tessera-walk-test").unwrap();
        let outside = TempDir::new("tessera-outside-test").unwrap();
        fs::write(temp.path().join("own"), "own").unwrap();
        fs::write(outside.path().join("foreign"), "foreign").unwrap();
        symlink(outside.path(), temp.path().join("directory-link")).unwrap();
        symlink(
            outside.path().join("foreign"),
            temp.path().join("file-link"),
        )
        .unwrap();
        assert_eq!(
            regular_files(temp.path()).unwrap(),
            [temp.path().join("own")]
        );
    }
}
