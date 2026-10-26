//! Writes into the data directory of an unprivileged service account.
//!
//! `/opt/camilladsp` belongs to `camilla`, which also runs a web GUI without a login. The root
//! installer and the package hooks must never follow a link that account could have planted: a
//! path is created with `O_EXCL` (which fails on any existing entry, a dangling link included),
//! or rewritten only when it is a regular file opened with `O_NOFOLLOW`, and the mode is set on
//! the open descriptor, never on a path.
use crate::{Error, Result, ensure};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Create `path` with `content` and `mode` (whatever the umask), unless anything is there.
/// `Ok(false)` when something is: a file, a directory or a link, which is left alone.
pub fn create_new(path: &Path, mode: u32, content: &str) -> Result<bool> {
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    file.write_all(content.as_bytes())?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.sync_all()?;
    Ok(true)
}

/// Replace the content of the regular file `path`, keeping its owner and group. A link, a
/// directory or a special file is refused, and nothing is truncated before that is known.
pub fn rewrite_regular(path: &Path, mode: u32, content: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| Error(format!("{}: {error}", path.display())))?;
    ensure(
        file.metadata()?.is_file(),
        &format!("{} is not a regular file", path.display()),
    )?;
    file.set_len(0)?;
    file.write_all(content.as_bytes())?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.sync_all()?;
    Ok(())
}

/// Remove the CamillaDSP state file `path` when it holds no choice of the operator: it names
/// no configuration, or `exists` says the configuration it names is gone. A state file that
/// names a configuration that is there is the operator's setup (a hand-made engine's state
/// included) and stays. A link or a special file is left alone, whatever it holds.
pub fn remove_idle_state(path: &Path, exists: impl Fn(&str) -> bool) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    let mut text = String::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?
        .read_to_string(&mut text)?;
    let idle = crate::dsp::state_config(&text).is_none_or(|config| !exists(&config));
    if idle {
        fs::remove_file(path)?;
    }
    Ok(idle)
}

/// Remove the directory `path` if nothing is in it; a link, a directory with entries and a
/// missing path are left as they are.
pub fn remove_empty_directory(path: &Path) -> bool {
    fs::remove_dir(path).is_ok()
}

/// `path` as a directory that is not a link, created when missing.
pub fn real_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    ensure(
        fs::symlink_metadata(path)?.is_dir(),
        &format!(
            "{} is not a directory (links are not followed)",
            path.display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tessera-data-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_new_file_gets_its_mode_whatever_the_umask_and_an_existing_entry_is_kept() {
        let dir = scratch("create");
        // The usual umask 022 would leave 0644 without the fchmod.
        assert!(create_new(&dir.join("state"), 0o664, "one\n").unwrap());
        assert_eq!(
            fs::metadata(dir.join("state"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o664
        );
        assert!(!create_new(&dir.join("state"), 0o664, "two\n").unwrap());
        assert_eq!(fs::read_to_string(dir.join("state")).unwrap(), "one\n");
        fs::create_dir(dir.join("directory")).unwrap();
        assert!(!create_new(&dir.join("directory"), 0o664, "x").unwrap());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_planted_link_is_never_written_through() {
        let dir = scratch("link");
        let target = dir.join("elsewhere");
        // A dangling link: creating through it would create the target.
        symlink(&target, dir.join("state")).unwrap();
        assert!(!create_new(&dir.join("state"), 0o664, "planted\n").unwrap());
        assert!(!target.exists());
        assert!(rewrite_regular(&dir.join("state"), 0o664, "planted\n").is_err());
        assert!(!target.exists());
        // A link to an existing file: its target keeps content and mode.
        fs::write(&target, "precious\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!create_new(&dir.join("state"), 0o664, "planted\n").unwrap());
        assert!(rewrite_regular(&dir.join("state"), 0o664, "planted\n").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "precious\n");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rewriting_keeps_regular_files_only_and_sets_the_mode_on_the_descriptor() {
        let dir = scratch("rewrite");
        let path = dir.join("state");
        fs::write(&path, "a long first content\n").unwrap();
        rewrite_regular(&path, 0o664, "short\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "short\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o664
        );
        for refused in ["missing", "directory"] {
            if refused == "directory" {
                fs::create_dir(dir.join(refused)).unwrap();
            }
            assert!(
                rewrite_regular(&dir.join(refused), 0o664, "x").is_err(),
                "{refused}"
            );
        }
        // A FIFO neither blocks the open nor is truncated.
        nix::unistd::mkfifo(
            &dir.join("fifo"),
            nix::sys::stat::Mode::from_bits_truncate(0o600),
        )
        .unwrap();
        assert!(rewrite_regular(&dir.join("fifo"), 0o664, "x").is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_state_file_goes_only_when_it_holds_no_choice_of_the_operator() {
        let dir = scratch("idle");
        let state = dir.join("statefile.yml");
        let present = |config: &str| config == "/opt/camilladsp/configs/Flat.yml";
        // Missing: nothing to do.
        assert!(!remove_idle_state(&state, present).unwrap());
        // The package's empty file, and one that names a configuration that is gone.
        for idle in [
            "config_path: null\nmute:\n- false\n",
            "",
            "config_path: /opt/camilladsp/configs/Speaker.yml\nvolume:\n- 0.0\n",
        ] {
            fs::write(&state, idle).unwrap();
            assert!(remove_idle_state(&state, present).unwrap(), "{idle:?}");
            assert!(!state.exists());
        }
        // The owner's file names a configuration that is there.
        let owner = "config_path: /opt/camilladsp/configs/Flat.yml\nvolume:\n- -3.0\n";
        fs::write(&state, owner).unwrap();
        assert!(!remove_idle_state(&state, present).unwrap());
        assert_eq!(fs::read_to_string(&state).unwrap(), owner);
        // A link is never followed, whatever the file behind it holds.
        let target = dir.join("elsewhere");
        fs::write(&target, "config_path: null\n").unwrap();
        fs::remove_file(&state).unwrap();
        symlink(&target, &state).unwrap();
        assert!(!remove_idle_state(&state, present).unwrap());
        assert!(fs::symlink_metadata(&state).is_ok() && target.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_an_empty_directory_is_removed() {
        let dir = scratch("empty");
        fs::create_dir_all(dir.join("full")).unwrap();
        fs::write(dir.join("full/file"), "x").unwrap();
        fs::create_dir(dir.join("empty")).unwrap();
        assert!(!remove_empty_directory(&dir.join("full")));
        assert!(!remove_empty_directory(&dir.join("missing")));
        assert!(remove_empty_directory(&dir.join("empty")));
        assert!(!dir.join("empty").exists() && dir.join("full/file").exists());
        symlink(dir.join("full"), dir.join("linked")).unwrap();
        assert!(!remove_empty_directory(&dir.join("linked")));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_directory_that_is_a_link_is_refused() {
        let dir = scratch("directory");
        fs::create_dir(dir.join("real")).unwrap();
        real_directory(&dir.join("real")).unwrap();
        real_directory(&dir.join("made/deeply")).unwrap();
        symlink(dir.join("real"), dir.join("linked")).unwrap();
        assert!(real_directory(&dir.join("linked")).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
