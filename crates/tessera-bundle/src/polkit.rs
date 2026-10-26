//! The portal's polkit rules file, written the same way by the package hook and by
//! the bundle installer.
use crate::{Error, Result};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Where polkit reads the rules for the portal's service switches.
pub const RULES: &str = "/etc/polkit-1/rules.d/49-tessera-web-appliance.rules";

/// Replace the rules file under `root` with `rules` (the output of
/// `tessera-web appliance polkit`): a private temporary file in the same directory,
/// made world-readable and renamed over the old one, so polkit never reads a partial
/// file.
///
/// The directory is polkitd's (0750 root:polkitd): no tmpfiles line of ours declares it,
/// and it is left as found, with one repair ([`repair_directory`]). Only where polkit is
/// absent is it created (0755 under the caller's umask), and the rules wait there for
/// polkit.
pub fn write(root: &Path, rules: &str) -> Result<()> {
    let target = root.join(RULES.trim_start_matches('/'));
    let directory = target
        .parent()
        .ok_or_else(|| Error("missing polkit rules directory".into()))?;
    fs::create_dir_all(directory)?;
    repair_directory(root, directory)?;
    let mut temporary = None;
    for serial in 0..100 {
        let path = directory.join(format!(".tessera-web.{}.{serial}", std::process::id()));
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o644)
            .open(&path)
        {
            Ok(file) => {
                temporary = Some((path, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let (temp, mut file) =
        temporary.ok_or_else(|| Error("cannot allocate temporary policy".into()))?;
    let result = (|| {
        file.write_all(rules.as_bytes())?;
        file.sync_all()?;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o644))?;
        fs::rename(&temp, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// The gid of group `name` in `root`'s `/etc/group`.
fn group_id(root: &Path, name: &str) -> Option<u32> {
    fs::read_to_string(root.join("etc/group"))
        .ok()?
        .lines()
        .find_map(|line| {
            let mut fields = line.split(':');
            (fields.next() == Some(name))
                .then(|| fields.nth(1)?.parse().ok())
                .flatten()
        })
}

/// Put polkitd's directory back to what polkitd made it, 0750 root:polkitd, where an
/// earlier 0.2.x package left it 0755: those packages declared the directory in their
/// tmpfiles file and so loosened it (0.3.0 declares nothing and never did this). Only that
/// exact state is touched, and only where the group `polkitd` exists, so a mode an
/// administrator chose and a host without polkitd stay as they are. Rule files are read by
/// polkitd, which is in the group, so the tighter mode costs nothing.
fn repair_directory(root: &Path, directory: &Path) -> Result<()> {
    let Some(group) = group_id(root, "polkitd") else {
        return Ok(());
    };
    let metadata = fs::metadata(directory)?;
    if metadata.permissions().mode() & 0o7777 != 0o755 {
        return Ok(());
    }
    std::os::unix::fs::chown(directory, None, Some(group))?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o750))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn the_rules_replace_the_old_file_atomically_with_mode_0644() {
        let root = std::env::temp_dir().join(format!("tessera-polkit-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        write(&root, "first\n").unwrap();
        let path = root.join(RULES.trim_start_matches('/'));
        assert_eq!(fs::read_to_string(&path).unwrap(), "first\n");
        write(&root, "second\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o644
        );
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            ["49-tessera-web-appliance.rules"],
            "no temporary file is left"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_rules_install_into_a_directory_polkitd_owns_and_leave_it_as_it_is() {
        let root =
            std::env::temp_dir().join(format!("tessera-polkit-owned-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        // polkitd ships /etc/polkit-1/rules.d as 0750 root:polkitd; no file of ours
        // declares it, so it is whatever polkitd made it.
        let directory = root.join("etc/polkit-1/rules.d");
        fs::create_dir_all(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o750)).unwrap();
        let before = fs::metadata(&directory).unwrap();
        write(&root, "rules\n").unwrap();
        let after = fs::metadata(&directory).unwrap();
        assert_eq!(after.permissions().mode() & 0o7777, 0o750);
        assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
        let file = root.join(RULES.trim_start_matches('/'));
        assert_eq!(fs::read_to_string(&file).unwrap(), "rules\n");
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o7777,
            0o644
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_directory_an_earlier_package_left_at_0755_goes_back_to_polkitds_mode() {
        let root =
            std::env::temp_dir().join(format!("tessera-polkit-repair-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let directory = root.join("etc/polkit-1/rules.d");
        fs::create_dir_all(&directory).unwrap();
        let mode =
            |directory: &Path| fs::metadata(directory).unwrap().permissions().mode() & 0o7777;
        // No polkitd group (polkit is absent or another one): the directory is left as found.
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        write(&root, "rules\n").unwrap();
        assert_eq!(mode(&directory), 0o755);
        // With the group, the 0.2.x state is repaired: 0750, group polkitd.
        let gid = fs::metadata(&directory).unwrap().gid();
        fs::write(
            root.join("etc/group"),
            format!("root:x:0:\npolkitd:x:{gid}:\npolkitd-extra:x:1:\n"),
        )
        .unwrap();
        write(&root, "rules\n").unwrap();
        assert_eq!(mode(&directory), 0o750);
        assert_eq!(fs::metadata(&directory).unwrap().gid(), gid);
        // The repair touches nothing else: the rules are in place, and a mode somebody
        // chose (not the loosened 0755) stays.
        assert_eq!(
            fs::read_to_string(root.join(RULES.trim_start_matches('/'))).unwrap(),
            "rules\n"
        );
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        write(&root, "more rules\n").unwrap();
        assert_eq!(mode(&directory), 0o700);
        // A group that only starts with the name is another group.
        fs::write(root.join("etc/group"), format!("polkitd-extra:x:{gid}:\n")).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        write(&root, "rules\n").unwrap();
        assert_eq!(mode(&directory), 0o755);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_rules_install_where_polkit_is_absent_and_its_directory_does_not_exist() {
        let root =
            std::env::temp_dir().join(format!("tessera-polkit-absent-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("etc")).unwrap();
        assert!(!root.join("etc/polkit-1").exists());
        write(&root, "rules\n").unwrap();
        assert_eq!(
            fs::read_to_string(root.join(RULES.trim_start_matches('/'))).unwrap(),
            "rules\n"
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
