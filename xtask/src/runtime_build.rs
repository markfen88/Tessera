//! Identity of our own integration sources, excluding research, protocol
//! runtime/vendor inputs and private repository history. This is a source
//! build label, not authentication or evidence about runtime behavior.
use crate::files::regular_files;
use crate::{Failure, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn source_id(root: &Path) -> Result<String> {
    let mut files = BTreeSet::<PathBuf>::new();
    for name in ["Cargo.toml", "Cargo.lock"] {
        let path = root.join(name);
        if !fs::symlink_metadata(&path)?.is_file() {
            return Err(Failure::new("runtime build input is not a regular file"));
        }
        files.insert(path);
    }
    for name in [
        "rust-toolchain.toml",
        ".cargo/config.toml",
        "dist/controller.args",
    ] {
        add_file(root.join(name), &mut files)?;
    }
    for parent in [root.join("crates"), root.join("xtask")] {
        if parent.file_name().is_some_and(|name| name == "xtask") {
            add_crate(&parent, &mut files)?;
        } else if parent.exists() {
            for entry in fs::read_dir(parent)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    add_crate(&entry.path(), &mut files)?;
                }
            }
        }
    }
    files.extend(regular_files(&root.join("dist/systemd"))?);
    let mut digest = Sha256::new();
    digest.update(b"tessera-runtime-own-source-v1\0");
    for file in files {
        let name = file
            .strip_prefix(root)
            .map_err(|_| Failure::new("build input outside root"))?
            .to_str()
            .ok_or_else(|| Failure::new("non-UTF8 runtime build input"))?;
        let content = fs::read(&file)?;
        digest.update((name.len() as u64).to_be_bytes());
        digest.update(name.as_bytes());
        digest.update((content.len() as u64).to_be_bytes());
        digest.update(content);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn add_file(path: PathBuf, files: &mut BTreeSet<PathBuf>) -> Result<()> {
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => {
            files.insert(path);
        }
        Ok(_) => return Err(Failure::new("runtime build input is not a regular file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn add_crate(path: &Path, files: &mut BTreeSet<PathBuf>) -> Result<()> {
    add_file(path.join("Cargo.toml"), files)?;
    add_file(path.join("build.rs"), files)?;
    for name in ["src", "assets"] {
        files.extend(regular_files(&path.join(name))?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::TempDir;

    #[test]
    fn identity_tracks_source_lockfile_paths_and_ignores_private_research() {
        let tmp = TempDir::new("tessera-runtime-build").unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("crates/own/src")).unwrap();
        fs::write(root.join("Cargo.toml"), "workspace").unwrap();
        fs::write(root.join("Cargo.lock"), "deps").unwrap();
        fs::write(root.join("crates/own/src/lib.rs"), "own source").unwrap();
        let before = source_id(root).unwrap();
        assert_eq!(before.len(), 64);
        fs::create_dir(root.join("research")).unwrap();
        fs::write(root.join("research/fixture.txt"), "excluded fixture").unwrap();
        fs::write(root.join("PROGRESS.md"), "reporting only").unwrap();
        assert_eq!(before, source_id(root).unwrap());
        fs::write(root.join("crates/own/src/lib.rs"), "changed own source").unwrap();
        assert_ne!(before, source_id(root).unwrap());
        fs::write(root.join("crates/own/src/lib.rs"), "own source").unwrap();
        fs::write(root.join("Cargo.lock"), "changed deps").unwrap();
        assert_ne!(before, source_id(root).unwrap());
        fs::write(root.join("Cargo.lock"), "deps").unwrap();
        fs::rename(
            root.join("crates/own/src/lib.rs"),
            root.join("crates/own/src/main.rs"),
        )
        .unwrap();
        assert_ne!(before, source_id(root).unwrap());
    }
}
