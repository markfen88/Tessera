//! Native dpkg hooks. Account/directory definitions live in sysusers.d and
//! tmpfiles.d; non-declarative migrations are shared with the one-shot command.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tessera_bundle::{Error, Result, Runner};

const LEGACY: &str = "/var/lib/tessera-package/controller-legacy-instances";
/// What the sysctl.d file changed, one `key was raised` line per ceiling: the values
/// to put back when the package is removed.
const SYSCTL_RECORD: &str = "/var/lib/tessera-package/sysctl-previous";
/// The ceilings of `/usr/lib/sysctl.d/60-tessera.conf`.
const SOCKET_BUFFERS: [&str; 2] = ["net.core.rmem_max", "net.core.wmem_max"];
/// Where systemd installs `systemd-sysctl`: in neither `bin` directory, so it is not on
/// dpkg's `PATH`. `/usr/lib/systemd` is the packaged place (Debian 13, Ubuntu 26.04);
/// `/lib/systemd` is the same directory behind a link on a merged-usr system and the
/// real one on an older layout.
const SYSCTL_PROGRAMS: [&str; 2] = [
    "/usr/lib/systemd/systemd-sysctl",
    "/lib/systemd/systemd-sysctl",
];
/// The unit the portal's service-switch helper jobs run in: systemd's slice for the
/// instances of `tessera-web-unitfile@.service`. It stays loaded, empty, after removal.
const UNITFILE_SLICE: &str = "system-tessera\\x2dweb\\x2dunitfile.slice";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hook {
    Preinst,
    Postinst,
    Prerm,
    Postrm,
}

pub fn hook(name: &str) -> Result<Hook> {
    match name {
        "preinst" => Ok(Hook::Preinst),
        "postinst" => Ok(Hook::Postinst),
        "prerm" => Ok(Hook::Prerm),
        "postrm" => Ok(Hook::Postrm),
        _ => Err(Error("unknown package hook".into())),
    }
}

pub fn apply(
    root: &Path,
    package: &str,
    hook: Hook,
    args: &[String],
    runner: &mut impl Runner,
) -> Result<()> {
    if !matches!(package, "tessera" | "tessera-web" | "tessera-dsp") {
        return Err(Error("unsupported package".into()));
    }
    let mut lifecycle = Lifecycle { root, runner };
    let action = args.first().map(String::as_str).unwrap_or("");
    match (package, hook, action) {
        ("tessera", Hook::Preinst, "install" | "upgrade") => lifecycle.capture(),
        ("tessera", Hook::Postinst, "configure") => {
            lifecycle.core(args.get(1).is_some_and(|arg| !arg.is_empty()))
        }
        ("tessera-web", Hook::Postinst, "configure") => {
            lifecycle.web(args.get(1).is_some_and(|arg| !arg.is_empty()))
        }
        ("tessera-dsp", Hook::Postinst, "configure") => {
            lifecycle.dsp(args.get(1).is_some_and(|arg| !arg.is_empty()))
        }
        ("tessera", Hook::Prerm, "remove" | "deconfigure") => {
            if lifecycle.systemd() {
                lifecycle.best(
                    "systemctl",
                    &[
                        "stop",
                        "tessera-controller@main.service",
                        "tesserad.service",
                    ],
                );
            }
            Ok(())
        }
        ("tessera-web", Hook::Prerm, "remove" | "deconfigure") => {
            if lifecycle.systemd() {
                lifecycle.best("systemctl", &["stop", "tessera-web.service"]);
                lifecycle.best(
                    "systemctl",
                    &[
                        "stop",
                        "tessera-network-recovery.timer",
                        "tessera-network-recovery.service",
                        "tessera-network.service",
                    ],
                );
                lifecycle.call("/usr/bin/tessera-network", &["boot-recover"])?;
            }
            Ok(())
        }
        // Stop on removal, never on upgrade: the running engine keeps playing.
        ("tessera-dsp", Hook::Prerm, "remove" | "deconfigure") => {
            if lifecycle.systemd() {
                lifecycle.best(
                    "systemctl",
                    &["stop", "camillagui.service", "camilladsp.service"],
                );
            }
            Ok(())
        }
        (_, Hook::Postrm, _) => lifecycle.remove(package, action),
        _ => Ok(()),
    }
}

struct Lifecycle<'a, R> {
    root: &'a Path,
    runner: &'a mut R,
}
impl<R: Runner> Lifecycle<'_, R> {
    fn at(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }
    fn systemd(&self) -> bool {
        self.root == Path::new("/") && self.at("/run/systemd/system").is_dir()
    }
    fn call(&mut self, program: &str, args: &[&str]) -> Result<String> {
        let reply = self.runner.command(
            program,
            &args.iter().map(|arg| (*arg).into()).collect::<Vec<_>>(),
        )?;
        if !reply.success {
            return Err(Error(format!("{program} failed: {}", reply.stderr)));
        }
        Ok(reply.stdout)
    }
    fn best(&mut self, program: &str, args: &[&str]) {
        let _ = self.call(program, args);
    }
    fn success(&mut self, program: &str, args: &[&str]) -> bool {
        self.call(program, args).is_ok()
    }
    fn instances(&self) -> Result<Vec<String>> {
        let mut instances = Vec::new();
        let directory = self.at("/etc/systemd/system");
        if !directory.is_dir() {
            return Ok(instances);
        }
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if !entry.file_name().to_string_lossy().ends_with(".wants") || !entry.path().is_dir() {
                continue;
            }
            for unit in fs::read_dir(entry.path())? {
                let unit = unit?;
                let name = unit.file_name().to_string_lossy().into_owned();
                if name.starts_with("tessera-controller@")
                    && name.ends_with(".service")
                    && unit.file_type()?.is_symlink()
                {
                    instances.push(name);
                }
            }
        }
        instances.sort();
        instances.dedup();
        Ok(instances)
    }
    fn sysctl_path(&self, key: &str) -> PathBuf {
        self.at(&format!("/proc/sys/{}", key.replace('.', "/")))
    }
    /// The `systemd-sysctl` to run, by absolute path (see [`SYSCTL_PROGRAMS`]); the bare
    /// name only where neither exists, so the failure is reported rather than guessed.
    fn sysctl_program(&self) -> String {
        SYSCTL_PROGRAMS
            .into_iter()
            .find(|path| self.at(path).is_file())
            .unwrap_or("systemd-sysctl")
            .to_owned()
    }
    /// Apply the package's sysctl.d file now, so the raised ceilings are there for the
    /// record that follows and for the daemon's first start. A failure is said, not
    /// fatal: the same file is applied at the next boot.
    fn apply_sysctl(&mut self) {
        let program = self.sysctl_program();
        if let Err(Error(why)) = self.call(&program, &["60-tessera.conf"]) {
            eprintln!(
                "tessera: the socket buffer limits of 60-tessera.conf are applied at the next boot ({why})"
            );
        }
    }
    fn sysctl_value(&self, key: &str) -> Option<String> {
        fs::read_to_string(self.sysctl_path(key))
            .ok()
            .map(|value| value.trim().to_owned())
    }
    /// Remember what the sysctl.d file changed, so that removing the package can put
    /// the values back. Only once (an upgrade finds the ceilings raised already), and
    /// only a value that actually changed.
    fn record_sysctl(&mut self, before: &[Option<String>; 2]) -> Result<()> {
        let record = self.at(SYSCTL_RECORD);
        if record.exists() {
            return Ok(());
        }
        let mut text = String::new();
        for (key, was) in SOCKET_BUFFERS.iter().zip(before) {
            if let (Some(was), Some(now)) = (was, self.sysctl_value(key)) {
                if *was != now {
                    text.push_str(&format!("{key} {was} {now}\n"));
                }
            }
        }
        if text.is_empty() {
            return Ok(());
        }
        let parent = self.at("/var/lib/tessera-package");
        fs::create_dir_all(&parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        fs::write(&record, text)?;
        fs::set_permissions(record, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }
    /// Put the ceilings back where they were before the package raised them, each only
    /// if it still has the value the package gave it (an administrator's later change
    /// is kept). The record goes with it: a reinstall records afresh.
    fn restore_sysctl(&mut self) -> Result<()> {
        let record = self.at(SYSCTL_RECORD);
        let Ok(text) = fs::read_to_string(&record) else {
            return Ok(());
        };
        let number = |value: &str| !value.is_empty() && value.bytes().all(|c| c.is_ascii_digit());
        for line in text.lines() {
            let fields: Vec<_> = line.split(' ').collect();
            let [key, was, raised] = fields[..] else {
                continue;
            };
            if SOCKET_BUFFERS.contains(&key)
                && number(was)
                && number(raised)
                && self.sysctl_value(key).as_deref() == Some(raised)
            {
                fs::write(self.sysctl_path(key), was)?;
            }
        }
        fs::remove_file(record)?;
        Ok(())
    }
    fn capture(&mut self) -> Result<()> {
        if self
            .at("/etc/systemd/system/tessera-controller@.service")
            .exists()
        {
            return Ok(());
        }
        let old =
            fs::read_to_string(self.at("/usr/lib/systemd/system/tessera-controller@.service"))
                .unwrap_or_default();
        if old.lines().any(|line| line == "User=%i") {
            let parent = self.at("/var/lib/tessera-package");
            fs::create_dir_all(&parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            let instances = self.instances()?;
            fs::write(
                self.at(LEGACY),
                if instances.is_empty() {
                    String::new()
                } else {
                    instances.join("\n") + "\n"
                },
            )?;
        }
        Ok(())
    }
    fn helper(&mut self, unit: &str, force: bool, suppress: bool) {
        if !suppress {
            self.best("deb-systemd-helper", &["unmask", unit]);
        }
        if !suppress
            && (force || self.success("deb-systemd-helper", &["--quiet", "was-enabled", unit]))
        {
            self.best("deb-systemd-helper", &["enable", unit]);
        } else {
            self.best("deb-systemd-helper", &["update-state", unit]);
        }
    }
    fn core(&mut self, upgrade: bool) -> Result<()> {
        self.call("systemd-sysusers", &["tessera.conf"])?;
        self.best("systemd-tmpfiles", &["--create", "tessera.conf"]);
        let before = SOCKET_BUFFERS.map(|key| self.sysctl_value(key));
        if self.systemd() {
            self.apply_sysctl();
        }
        self.record_sysctl(&before)?;
        let marker = self.at(LEGACY);
        let mut disabled = false;
        let mut migrated = false;
        if marker.exists() {
            let instances = fs::read_to_string(&marker)?;
            if instances.is_empty() {
                disabled = true;
            } else {
                for unit in instances.lines() {
                    // Unit names originate in directory entries. Fixed argv and
                    // the explicit separator prevent option interpretation.
                    if !unit.starts_with("tessera-controller@")
                        || !unit.ends_with(".service")
                        || unit.contains('/')
                    {
                        return Err(Error("invalid legacy Controller instance".into()));
                    }
                    self.call("systemctl", &["--no-reload", "disable", unit])?;
                }
                migrated = true;
                eprintln!(
                    "tessera: legacy Controller instances migrated to @main on the next daemon restart."
                );
            }
            fs::remove_file(marker)?;
        }
        if self
            .at("/etc/systemd/system/tessera-controller@.service")
            .exists()
        {
            disabled = true;
            eprintln!(
                "tessera: local Controller template overrides the packaged unit; review/remove it before switching to @main."
            );
        }
        if self
            .instances()?
            .iter()
            .any(|unit| unit != "tessera-controller@main.service")
        {
            disabled = true;
        }
        self.helper("tessera-controller@.service", migrated, disabled);
        self.helper("tesserad.service", false, false);
        if self.systemd() {
            self.best("systemctl", &["daemon-reload"]);
            if upgrade && self.success("systemctl", &["--quiet", "is-active", "tesserad.service"]) {
                eprintln!(
                    "tessera: tesserad is running the old version; restart it when convenient:"
                );
                eprintln!(
                    "         systemctl restart tesserad   (also activates Controller compatibility; drops active audio flows)"
                );
            }
        }
        Ok(())
    }
    fn web(&mut self, upgrade: bool) -> Result<()> {
        self.call("systemd-sysusers", &["tessera.conf", "tessera-web.conf"])?;
        self.best("systemd-tmpfiles", &["--create", "tessera-web.conf"]);
        self.call("tessera-web", &["init-password"])?;
        let rules = self.call("tessera-web", &["appliance", "polkit"])?;
        tessera_bundle::polkit::write(self.root, &rules)?;
        self.best(
            "deb-systemd-helper",
            &[
                "enable",
                "tessera-network.service",
                "tessera-network-recovery.timer",
            ],
        );
        self.helper("tessera-web.service", false, false);
        if self.systemd() {
            self.best("systemctl", &["daemon-reload"]);
            if !upgrade {
                self.best("systemctl", &["start", "tessera-network-recovery.timer"]);
                self.best("systemctl", &["start", "tessera-web.service"]);
            } else if self.success(
                "systemctl",
                &["--quiet", "is-active", "tessera-web.service"],
            ) {
                self.best("systemctl", &["restart", "tessera-web.service"]);
            }
        }
        Ok(())
    }
    /// The CamillaDSP account and data directories (sysusers.d/tmpfiles.d), an
    /// empty state file, the portal's account in group `camilla` so it can write
    /// the EQ files, and the units enabled but not started: starting is left to
    /// the administrator or the bundle installer, because CamillaDSP takes the
    /// sound card.
    fn dsp(&mut self, upgrade: bool) -> Result<()> {
        self.call("systemd-sysusers", &["tessera.conf", "tessera-dsp.conf"])?;
        self.call("systemd-tmpfiles", &["--create", "tessera-dsp.conf"])?;
        // No configuration chosen yet: the engine waits (camilladsp -w) until one is
        // activated. Group camilla may write the file. The directory is the camilla
        // account's, which runs a GUI without a login: whatever is there already (a
        // planted link included) is kept, and the file is created, not written through.
        let state = self.at("/opt/camilladsp/statefile.yml");
        if tessera_bundle::data::create_new(
            &state,
            0o664,
            "config_path: null\nmute:\n- false\n- false\n- false\n- false\n- false\nvolume:\n- 0.0\n- 0.0\n- 0.0\n- 0.0\n- 0.0\n",
        )? {
            self.call(
                "chown",
                &["-h", "camilla:camilla", &state.to_string_lossy()],
            )?;
        }
        if self.success("getent", &["passwd", "tessera-web"]) {
            let groups = self.call("id", &["-nG", "tessera-web"]).unwrap_or_default();
            if !groups.split_whitespace().any(|group| group == "camilla") {
                self.call("usermod", &["-aG", "camilla", "tessera-web"])?;
                eprintln!(
                    "tessera-dsp: tessera-web joined group camilla; restart tessera-web to apply it."
                );
            }
        }
        for unit in ["camilladsp.service", "camillagui.service"] {
            self.helper(unit, false, false);
        }
        if self.systemd() {
            self.best("systemctl", &["daemon-reload"]);
            if upgrade && self.success("systemctl", &["--quiet", "is-active", "camilladsp.service"])
            {
                eprintln!(
                    "tessera-dsp: CamillaDSP is running the old version; restart it when convenient:"
                );
                eprintln!(
                    "             systemctl restart camilladsp camillagui   (a few seconds of silence)"
                );
            }
        }
        Ok(())
    }
    fn remove(&mut self, package: &str, action: &str) -> Result<()> {
        if package == "tessera" && matches!(action, "remove" | "purge") {
            let _ = fs::remove_dir(self.at("/run/tessera"));
            // What the daemon and its controller leave loaded until the next boot: the
            // raised socket-buffer ceilings and two empty slices.
            self.restore_sysctl()?;
            self.best(
                "systemctl",
                &[
                    "stop",
                    "tesseraaudio.slice",
                    "system-tessera\\x2dcontroller.slice",
                ],
            );
        }
        if package == "tessera-web" && matches!(action, "remove" | "purge") {
            // The helper's firewall table filters the portal's port and the audio slice's
            // traffic: it must not outlive the helper. The network files it committed
            // stay (see packaging/README.md). The slice of the service-switch helper
            // jobs is ours by its name (the template's) and stays loaded when empty.
            self.best(
                "/usr/sbin/nft",
                &["delete", "table", "inet", "tessera_network"],
            );
            self.best("systemctl", &["stop", "tesseraaudio.slice"]);
            self.best("systemctl", &["stop", UNITFILE_SLICE]);
        }
        // The operator's CamillaDSP configurations and filter files stay on a
        // plain remove; the units are masked so no enable link outlives them.
        if package == "tessera-dsp" && action == "remove" {
            self.best(
                "deb-systemd-helper",
                &["mask", "camilladsp.service", "camillagui.service"],
            );
        }
        if action == "purge" {
            if package == "tessera-dsp" {
                self.best(
                    "deb-systemd-helper",
                    &["purge", "camilladsp.service", "camillagui.service"],
                );
                self.best(
                    "deb-systemd-helper",
                    &["unmask", "camilladsp.service", "camillagui.service"],
                );
                // The camilla account is never deleted by the package
                // (systemd-sysusers semantics); the bundle's uninstall --purge
                // removes the account it created.
                //
                // The directories are shared with an engine the operator may have run
                // before: only what the package made goes (the state file when it names
                // no configuration that exists, the directories once they are empty).
                // The configurations and filter files are the operator's, and so is a
                // program of their own. dpkg removes the conffiles of /etc/camilladsp.
                tessera_bundle::data::remove_idle_state(
                    &self.at("/opt/camilladsp/statefile.yml"),
                    |config| self.at(config).exists(),
                )?;
                for directory in [
                    "/opt/camilladsp/configs",
                    "/opt/camilladsp/coeffs",
                    "/opt/camilladsp",
                    "/etc/camilladsp",
                ] {
                    tessera_bundle::data::remove_empty_directory(&self.at(directory));
                }
            } else if package == "tessera" {
                for unit in ["tesserad.service", "tessera-controller@.service"] {
                    self.best("deb-systemd-helper", &["purge", unit]);
                    self.best("deb-systemd-helper", &["unmask", unit]);
                }
                for directory in [
                    "/etc/tessera",
                    "/var/lib/tessera",
                    "/var/lib/tessera-controller",
                    "/var/lib/tessera-package",
                    "/run/tessera",
                ] {
                    remove_tree(&self.at(directory))?;
                }
            } else {
                self.best("deb-systemd-helper", &["purge", "tessera-web.service"]);
                self.best(
                    "deb-systemd-helper",
                    &[
                        "purge",
                        "tessera-network.service",
                        "tessera-network-recovery.timer",
                    ],
                );
                self.best("deb-systemd-helper", &["unmask", "tessera-web.service"]);
                for path in [
                    "/etc/tessera/web.toml",
                    "/etc/tessera/web-password",
                    "/etc/polkit-1/rules.d/49-tessera-web-appliance.rules",
                ] {
                    match fs::remove_file(self.at(path)) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                // The network helper's journal and runtime, and the portal's own
                // state (its saved amplifier volume, systemd's StateDirectory).
                for directory in [
                    "/var/lib/tessera-network",
                    "/run/tessera-network",
                    "/var/lib/tessera-web",
                ] {
                    remove_tree(&self.at(directory))?;
                }
            }
        }
        if self.systemd() {
            self.best("systemctl", &["daemon-reload"]);
        }
        Ok(())
    }
}

fn remove_tree(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        fs::remove_file(path)?;
        return Ok(());
    }
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_bundle::Reply;

    struct Quiet;
    impl Runner for Quiet {
        fn command(&mut self, _: &str, _: &[String]) -> Result<Reply> {
            Ok(Reply {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        fn confirm(&mut self, _: &str) -> Result<bool> {
            Err(Error("unexpected prompt".into()))
        }
    }

    /// A root with the two ceilings at `value`.
    fn root(name: &str, value: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("tessera-sysctl-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("proc/sys/net/core")).unwrap();
        for file in ["rmem_max", "wmem_max"] {
            fs::write(
                root.join("proc/sys/net/core").join(file),
                format!("{value}\n"),
            )
            .unwrap();
        }
        root
    }

    fn set(root: &Path, file: &str, value: &str) {
        fs::write(
            root.join("proc/sys/net/core").join(file),
            format!("{value}\n"),
        )
        .unwrap();
    }

    fn get(root: &Path, file: &str) -> String {
        fs::read_to_string(root.join("proc/sys/net/core").join(file))
            .unwrap()
            .trim()
            .to_owned()
    }

    fn record(root: &Path) -> Option<String> {
        fs::read_to_string(root.join("var/lib/tessera-package/sysctl-previous")).ok()
    }

    #[test]
    fn the_ceilings_are_recorded_once_and_put_back_on_removal() {
        let root = root("restore", "212992");
        let mut runner = Quiet;
        let mut lifecycle = Lifecycle {
            root: &root,
            runner: &mut runner,
        };
        let before = SOCKET_BUFFERS.map(|key| lifecycle.sysctl_value(key));
        set(&root, "rmem_max", "8388608");
        set(&root, "wmem_max", "8388608");
        lifecycle.record_sysctl(&before).unwrap();
        assert_eq!(
            record(&root).as_deref(),
            Some("net.core.rmem_max 212992 8388608\nnet.core.wmem_max 212992 8388608\n")
        );
        assert_eq!(
            fs::metadata(root.join("var/lib/tessera-package/sysctl-previous"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        // An upgrade finds them raised already and keeps the first record.
        let raised = [Some("8388608".to_owned()), Some("8388608".to_owned())];
        lifecycle.record_sysctl(&raised).unwrap();
        assert_eq!(record(&root).unwrap().lines().count(), 2);
        lifecycle.restore_sysctl().unwrap();
        assert_eq!(get(&root, "rmem_max"), "212992");
        assert_eq!(get(&root, "wmem_max"), "212992");
        assert_eq!(record(&root), None);
        // Nothing left to do the second time (purge follows remove).
        lifecycle.restore_sysctl().unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_ceiling_somebody_else_changed_since_is_left_alone() {
        let root = root("changed", "212992");
        let mut runner = Quiet;
        let mut lifecycle = Lifecycle {
            root: &root,
            runner: &mut runner,
        };
        let before = SOCKET_BUFFERS.map(|key| lifecycle.sysctl_value(key));
        set(&root, "rmem_max", "8388608");
        set(&root, "wmem_max", "8388608");
        lifecycle.record_sysctl(&before).unwrap();
        set(&root, "rmem_max", "33554432");
        lifecycle.restore_sysctl().unwrap();
        assert_eq!(
            get(&root, "rmem_max"),
            "33554432",
            "the administrator's value stays"
        );
        assert_eq!(get(&root, "wmem_max"), "212992");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn nothing_is_recorded_when_the_file_changed_nothing() {
        let root = root("same", "8388608");
        let mut runner = Quiet;
        let mut lifecycle = Lifecycle {
            root: &root,
            runner: &mut runner,
        };
        let before = SOCKET_BUFFERS.map(|key| lifecycle.sysctl_value(key));
        lifecycle.record_sysctl(&before).unwrap();
        assert_eq!(record(&root), None);
        lifecycle.restore_sysctl().unwrap();
        assert_eq!(get(&root, "rmem_max"), "8388608");
        // A host without the files (a container) records and restores nothing.
        let bare = std::env::temp_dir().join(format!("tessera-sysctl-bare-{}", std::process::id()));
        fs::create_dir_all(&bare).unwrap();
        let mut runner = Quiet;
        let mut lifecycle = Lifecycle {
            root: &bare,
            runner: &mut runner,
        };
        lifecycle.record_sysctl(&[None, None]).unwrap();
        lifecycle.restore_sysctl().unwrap();
        assert!(!bare.join("var").exists());
        fs::remove_dir_all(&bare).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    /// A system whose one `systemd-sysctl` is at `installed` and nowhere on the `PATH` of
    /// a maintainer script: running that path raises the two ceilings under `root`, any
    /// other name is "not found" (what dpkg's `PATH` does to the bare name).
    struct Sysctl {
        root: PathBuf,
        installed: &'static str,
        calls: Vec<String>,
    }

    impl Runner for Sysctl {
        fn command(&mut self, program: &str, args: &[String]) -> Result<Reply> {
            self.calls.push(format!("{program} {}", args.join(" ")));
            if program != self.installed {
                return Err(Error(format!("{program}: not found")));
            }
            set(&self.root, "rmem_max", "8388608");
            set(&self.root, "wmem_max", "8388608");
            Ok(Reply {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        fn confirm(&mut self, _: &str) -> Result<bool> {
            Err(Error("unexpected prompt".into()))
        }
    }

    /// Where `systemd-sysctl` is installed under `root`, an empty executable stand-in.
    fn install_sysctl(root: &Path, directory: &str) {
        fs::create_dir_all(root.join(directory)).unwrap();
        fs::write(root.join(directory).join("systemd-sysctl"), "").unwrap();
    }

    #[test]
    fn systemd_sysctl_is_run_by_the_path_systemd_installs_it_at() {
        for (directory, program) in [
            ("usr/lib/systemd", "/usr/lib/systemd/systemd-sysctl"),
            ("lib/systemd", "/lib/systemd/systemd-sysctl"),
        ] {
            let root = root("path", "212992");
            install_sysctl(&root, directory);
            let mut runner = Sysctl {
                root: root.clone(),
                installed: program,
                calls: Vec::new(),
            };
            let mut lifecycle = Lifecycle {
                root: &root,
                runner: &mut runner,
            };
            assert_eq!(lifecycle.sysctl_program(), program);
            // What the postinst does around the call: the ceilings are raised by it, so
            // the record exists and removal puts them back (finding N3 of TV-3).
            let before = SOCKET_BUFFERS.map(|key| lifecycle.sysctl_value(key));
            lifecycle.apply_sysctl();
            lifecycle.record_sysctl(&before).unwrap();
            assert_eq!(
                record(&root).as_deref(),
                Some("net.core.rmem_max 212992 8388608\nnet.core.wmem_max 212992 8388608\n")
            );
            lifecycle.restore_sysctl().unwrap();
            assert_eq!(runner.calls, [format!("{program} 60-tessera.conf")]);
            assert_eq!(get(&root, "rmem_max"), "212992");
            assert_eq!(get(&root, "wmem_max"), "212992");
            fs::remove_dir_all(&root).unwrap();
        }
    }

    #[test]
    fn without_systemd_sysctl_nothing_is_recorded_and_the_bare_name_is_what_is_tried() {
        let root = root("none", "212992");
        let mut runner = Sysctl {
            root: root.clone(),
            installed: "/usr/lib/systemd/systemd-sysctl",
            calls: Vec::new(),
        };
        let mut lifecycle = Lifecycle {
            root: &root,
            runner: &mut runner,
        };
        assert_eq!(lifecycle.sysctl_program(), "systemd-sysctl");
        let before = SOCKET_BUFFERS.map(|key| lifecycle.sysctl_value(key));
        lifecycle.apply_sysctl();
        lifecycle.record_sysctl(&before).unwrap();
        assert_eq!(record(&root), None);
        assert_eq!(runner.calls, ["systemd-sysctl 60-tessera.conf"]);
        assert_eq!(get(&root, "rmem_max"), "212992");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_record_can_name_only_the_two_ceilings_and_plain_numbers() {
        let root = root("hostile", "8388608");
        fs::create_dir_all(root.join("proc/sys/kernel")).unwrap();
        fs::write(root.join("proc/sys/kernel/panic"), "0\n").unwrap();
        fs::create_dir_all(root.join("var/lib/tessera-package")).unwrap();
        fs::write(
            root.join("var/lib/tessera-package/sysctl-previous"),
            "kernel.panic 5 0\nnet.core.rmem_max ../x 8388608\nnet.core.wmem_max 100 8388608 extra\nnet.core.rmem_max 100 8388608\n",
        )
        .unwrap();
        let mut runner = Quiet;
        let mut lifecycle = Lifecycle {
            root: &root,
            runner: &mut runner,
        };
        lifecycle.restore_sysctl().unwrap();
        assert_eq!(
            fs::read_to_string(root.join("proc/sys/kernel/panic")).unwrap(),
            "0\n"
        );
        assert_eq!(get(&root, "wmem_max"), "8388608");
        assert_eq!(get(&root, "rmem_max"), "100");
        fs::remove_dir_all(&root).unwrap();
    }
}
