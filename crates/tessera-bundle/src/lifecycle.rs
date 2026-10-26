use crate::manifest::{self, Deb, Manifest};
use crate::pcm;
use crate::record::{self, Record};
use crate::{
    Error, Options, Paths, Result, Runner, checked, command, data, dsp, ensure, polkit, portal,
    visible,
};
use std::fs;
use std::path::Path;
use std::time::Duration;

/// The speaker profile's device configuration, written once and then the operator's.
const SPEAKER_CONFIG: &str = "/etc/tessera/config.d/60-speaker.toml";
/// What the CamillaDSP stage keeps: the engine's configurations and its state file.
const DSP_CONFIGS: &str = "/opt/camilladsp/configs";
const DSP_COEFFS: &str = "/opt/camilladsp/coeffs";
const DSP_STATE: &str = "/opt/camilladsp/statefile.yml";
const DSP_UNITS: [&str; 2] = ["camilladsp", "camillagui"];
/// The GUI's configuration, a conffile of the `tessera-dsp` package.
const GUI_CONFIG: &str = "/etc/camilladsp/camillagui.yml";
/// A unit file here wins over the packaged one in `/usr/lib/systemd/system`.
const UNIT_DIRECTORY: &str = "/etc/systemd/system";
/// What an unit file or drop-in directory of the operator is renamed to when the packaged
/// unit takes over.
const ASIDE: &str = "pre-tessera-dsp";
/// The portal's configuration drop-in of the speaker profile, its unit drop-in, and the example
/// unit drop-in the documentation of earlier builds told the operator to copy next to it.
const PORTAL_DROPIN: &str = "/etc/tessera/web.d/60-speaker.toml";
const PORTAL_UNIT_DROPIN: &str = "/etc/systemd/system/tessera-web.service.d/60-speaker.conf";
const PORTAL_EXAMPLE_DROPIN: &str =
    "/etc/systemd/system/tessera-web.service.d/appliance-paths.conf";
/// A line of that example the portal does not need since 0.3.0: it keeps the amplifier volume
/// in its own state directory (`StateDirectory=`), and no longer saves the mixer with `alsactl`
/// under `/var/lib/alsa`. Opening that directory for writing only gave the portal more than it
/// uses, so the file is removed with the line, and the line is not copied into the profile's
/// drop-in.
const SUPERSEDED: [&str; 1] = ["ReadWritePaths=-/var/lib/alsa"];

/// What the CamillaDSP units were before this run, asked before any package changes them.
#[derive(Debug, Default)]
struct Before {
    engine_enabled: bool,
    gui_enabled: bool,
}

struct Lifecycle<'a, R> {
    options: Options,
    paths: &'a Paths,
    runner: &'a mut R,
    step: &'static str,
    started: bool,
    record: Record,
    arch: String,
    /// The speaker profile with the CamillaDSP stage (`tessera-dsp`).
    use_dsp: bool,
    /// An earlier tessera-only speaker is converted: its own bridge is removed.
    strip_bridge: bool,
    bridge_stripped: bool,
    before: Before,
    /// Which of the stage's units this run leaves enabled: an upgrade keeps what the speaker
    /// had, so an engine or GUI that was off stays off, and a speaker that had its own
    /// engine and no GUI does not get one.
    want_engine: bool,
    want_gui: bool,
    /// The `bind_address` of the GUI's configuration before this run, asked before any package
    /// changes it (dpkg replaces a file the operator never changed by the new package's).
    gui_bound: Option<String>,
    /// The address the GUI's configuration says after this run, when it is not the one it had
    /// before: a running `camillagui` keeps its old socket until it is restarted.
    gui_rebound: Option<String>,
}

/// What [`Lifecycle::gui_binding`] decided for the EQ engine's GUI.
struct Binding {
    /// The management address the GUI is on and listens on: what the portal's link uses.
    address: Option<String>,
    /// The `bind_address` the file says now, when it differs from the one before the run.
    rebound: Option<String>,
}

/// The packages of the bundle that this run installs: `tessera-dsp` only for the
/// CamillaDSP speaker.
fn selected(manifest: &Manifest, use_dsp: bool) -> Vec<&Deb> {
    manifest
        .debs
        .iter()
        .filter(|deb| use_dsp || deb.package != "tessera-dsp")
        .collect()
}

pub(crate) fn run(options: &Options, paths: &Paths, runner: &mut impl Runner) -> Result<()> {
    let mut state = Lifecycle {
        options: options.clone(),
        paths,
        runner,
        step: "parse",
        started: false,
        record: Record::default(),
        arch: String::new(),
        use_dsp: false,
        strip_bridge: false,
        bridge_stripped: false,
        before: Before::default(),
        want_engine: false,
        want_gui: false,
        gui_bound: None,
        gui_rebound: None,
    };
    let result = if options.action == "install" {
        state.install()
    } else {
        state.uninstall()
    };
    if let Err(error) = result {
        if state.started {
            if options.action == "install" {
                state.record.complete = false;
                let _ = state.record.write(paths);
                eprintln!(
                    "install: failed at {} (exit 1); packages may remain installed",
                    state.step
                );
            } else {
                if state.options.purge {
                    state.record.purge_pending = true;
                    state.record.keep_users = state.options.keep;
                }
                let _ = state.record.write(paths);
                eprintln!("uninstall: failed at {} (exit 1)", state.step);
                if state.step == "record-directory" {
                    state.residue();
                }
            }
        }
        return Err(Error(format!(
            "{}: {}: {error}",
            options.action, state.step
        )));
    }
    Ok(())
}

impl<R: Runner> Lifecycle<'_, R> {
    fn save(&mut self) -> Result<()> {
        self.started = true;
        self.record.write(self.paths)
    }

    fn read_record(&mut self) -> Result<bool> {
        self.step = "record";
        if self.options.dry && self.paths.uid != 0 {
            println!("installed state unknown (run as root to read the record)");
            return Ok(false);
        }
        match Record::read(self.paths)? {
            Some(record) => {
                self.record = record;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn root(&self) -> Result<()> {
        ensure(
            self.options.dry || self.paths.root != Path::new("/") || self.paths.uid == 0,
            "requires root",
        )
    }

    fn platform(&mut self, manifest: &Manifest) -> Result<()> {
        self.step = "platform";
        self.arch = checked(self.runner, "dpkg", &["--print-architecture"])?;
        ensure(
            self.arch == manifest.arch,
            &format!("requires architecture {}", manifest.arch),
        )?;
        let os = manifest::assignments(&fs::read_to_string(&self.paths.os_release)?)?;
        let field = |key: &str| {
            os.get(key)
                .and_then(|v| v.first())
                .map(String::as_str)
                .unwrap_or("")
        };
        match (manifest.os.id.as_str(), manifest.os.codename.as_str()) {
            ("debian", "trixie") => {
                ensure(
                    !self.options.allow_ubuntu,
                    "Ubuntu opt-in requires an Ubuntu bundle",
                )?;
                ensure(
                    field("ID") == "debian" && field("VERSION_CODENAME") == "trixie",
                    "requires Debian trixie",
                )?;
            }
            ("ubuntu", "noble") => {
                ensure(
                    self.options.allow_ubuntu,
                    "experimental Ubuntu requires --allow-ubuntu-24.04",
                )?;
                ensure(
                    manifest.os.version_id == "24.04"
                        && manifest.arch == "arm64"
                        && field("ID") == "ubuntu"
                        && field("VERSION_ID") == "24.04"
                        && field("VERSION_CODENAME") == "noble",
                    "requires experimental Ubuntu 24.04 noble arm64",
                )?;
                eprintln!(
                    "WARNING: experimental Ubuntu 24.04 arm64 bundle; live audio interoperability unverified"
                );
            }
            _ => return Err(Error("unsupported bundle platform".into())),
        }
        let version = checked(self.runner, "systemctl", &["--version"])?;
        let version = version
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        ensure(version >= 253, "requires systemd >= 253")?;
        self.root()
    }

    fn package_field(&mut self, name: &str, field: &str) -> Result<Option<String>> {
        let name = name.split(':').next().unwrap_or(name);
        let mut query = format!("{name}:{}", self.arch);
        let format = format!("-f=${{{field}}}\n");
        let mut value = command(self.runner, "dpkg-query", &["-W", &format, &query])?;
        if !value.success {
            let architecture = command(
                self.runner,
                "dpkg-query",
                &["-W", "-f=${Architecture}\n", name],
            )?;
            if !architecture.success || architecture.stdout.trim() != "all" {
                return Ok(None);
            }
            query = name.into();
            value = command(self.runner, "dpkg-query", &["-W", &format, &query])?;
        }
        let value = value.stdout.trim_end();
        if value.is_empty() || value.chars().any(char::is_whitespace) {
            return Ok(None);
        }
        Ok(Some(value.into()))
    }

    fn version(&mut self, name: &str) -> Result<Option<String>> {
        let version = self.package_field(name, "Version")?;
        if version.is_some()
            && self.package_field(name, "db:Status-Status")?.as_deref() == Some("installed")
        {
            Ok(version)
        } else {
            Ok(None)
        }
    }

    fn compare(&mut self, left: &str, op: &str, right: &str) -> Result<bool> {
        Ok(command(
            self.runner,
            "dpkg",
            &["--compare-versions", left, op, right],
        )?
        .success)
    }

    /// The versions the installed packages that provide `name` state (`Provides: name (= 1.2)`),
    /// `None` for one that states none. dpkg and apt let such a package stand in for `name`
    /// (`libasound2t64` provides `libasound2` on trixie and noble); a lookup by the package's
    /// own name does not see it.
    fn providers(&mut self, name: &str) -> Result<Vec<Option<String>>> {
        let reply = command(
            self.runner,
            "dpkg-query",
            &[
                "-W",
                "-f=${binary:Package}\t${db:Status-Status}\t${Provides}\n",
            ],
        )?;
        let mut found = Vec::new();
        if !reply.success {
            return Ok(found);
        }
        for line in reply.stdout.lines() {
            let mut fields = line.split('\t');
            let (Some(package), Some("installed"), Some(provides)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if package
                .split_once(':')
                .is_some_and(|(_, arch)| arch != self.arch)
            {
                continue;
            }
            for provided in provides.split(',').map(str::trim) {
                let (provided, version) = match provided.split_once(" (= ") {
                    Some((provided, rest)) => (provided, rest.strip_suffix(')').map(str::to_owned)),
                    None => (provided, None),
                };
                if provided == name {
                    found.push(version);
                }
            }
        }
        Ok(found)
    }

    /// Whether a package that provides `name` meets `constraint`. A provider without a version
    /// meets only a dependency without one, as in dpkg.
    fn provided(&mut self, name: &str, constraint: Option<(&str, &str)>) -> Result<bool> {
        for version in self.providers(name)? {
            match (constraint, version) {
                (None, _) => return Ok(true),
                (Some((op, required)), Some(version)) => {
                    if self.compare(&version, op, required)? {
                        return Ok(true);
                    }
                }
                (Some(_), None) => {}
            }
        }
        Ok(false)
    }

    fn dependencies(&mut self, manifest: &Manifest) -> Result<()> {
        for dep in &manifest.apt_depends {
            let (name, constraint) = manifest::dependency(dep)?;
            let Some(installed) = self.version(name)? else {
                ensure(
                    self.provided(name, constraint)?,
                    &format!("missing dependency {name}"),
                )?;
                continue;
            };
            if let Some((op, required)) = constraint {
                ensure(
                    self.compare(&installed, op, required)?,
                    "dependency too old",
                )?;
            }
        }
        Ok(())
    }

    /// Ask apt, without changing anything, whether it can install `names`: a name it cannot
    /// resolve refuses the run before the first record is written.
    fn simulate_dependencies(&mut self, names: &[String]) -> Result<()> {
        let mut argv = vec![
            "-s".to_owned(),
            "install".to_owned(),
            "-y".to_owned(),
            "--no-install-recommends".to_owned(),
        ];
        argv.extend_from_slice(names);
        let reply = self.runner.command("apt-get", &argv)?;
        ensure(
            reply.success,
            &format!(
                "apt cannot install the missing dependencies ({}): {}",
                names.join(", "),
                visible(reply.stderr.trim())
            ),
        )
    }

    fn apt_install(&mut self, args: &[String]) -> Result<()> {
        let mut argv = vec!["install".into(), "--no-remove".into()];
        argv.extend_from_slice(args);
        let reply = self.runner.command("apt-get", &argv)?;
        print!("{}", reply.stdout);
        eprint!("{}", reply.stderr);
        if !reply.success {
            eprintln!(
                "install: refusing package removal; upgrade tessera-web and tessera-pw together with tessera to the bundle version first (or explicitly remove them beforehand)"
            );
            return Err(Error(visible(&reply.stderr)));
        }
        Ok(())
    }

    fn groups(&mut self, user: &str) -> Result<Vec<String>> {
        Ok(checked(self.runner, "id", &["-nG", user])?
            .split_whitespace()
            .map(str::to_owned)
            .collect())
    }

    /// Decide whether this run installs the CamillaDSP stage, and whether it converts a
    /// tessera-only speaker. `--no-dsp` keeps an existing tessera-only speaker; going
    /// from CamillaDSP back to tessera only needs the bundle removed first.
    fn decide_dsp(&mut self, manifest: &Manifest) -> Result<()> {
        self.step = "dsp";
        let speaker = self.options.profile == "speaker";
        self.use_dsp = speaker && !self.options.no_dsp && manifest.dsp;
        if speaker && !self.options.no_dsp && !manifest.dsp {
            eprintln!(
                "note: this bundle was built without CamillaDSP; installing the speaker profile without it"
            );
        }
        let owned = self.record.packages.iter().any(|pkg| pkg == "tessera-dsp");
        ensure(
            !(owned && speaker && !self.use_dsp),
            "the installed bundle includes CamillaDSP: run uninstall first to install with --no-dsp",
        )?;
        self.strip_bridge = false;
        let config = self.paths.at(SPEAKER_CONFIG);
        if self.use_dsp && config.is_file() {
            // The bundle's own [[bridge]] is removed because CamillaDSP takes the sound
            // card. Any other bridge is the operator's and is left alone, so the install
            // refuses rather than guess.
            let found = dsp::bridges(&fs::read_to_string(config)?);
            if found.total > 0 && found.total == found.ours {
                self.strip_bridge = true;
            } else if found.total > 0 {
                return Err(Error(
                    "existing /etc/tessera/config.d/60-speaker.toml has a [[bridge]] that is not the speaker profile's: CamillaDSP and a bridge cannot share the sound card; remove it, or install with --no-dsp".into(),
                ));
            }
        }
        if self.use_dsp {
            self.snapshot_stage()?;
        }
        Ok(())
    }

    /// An unit file of this name in one of systemd's unit directories. A mask (a link to
    /// `/dev/null`) is none.
    fn unit_file(&self, unit: &str) -> bool {
        [
            UNIT_DIRECTORY,
            "/usr/lib/systemd/system",
            "/lib/systemd/system",
        ]
        .iter()
        .any(|directory| {
            fs::symlink_metadata(self.paths.at(&format!("{directory}/{unit}.service")))
                .is_ok_and(|metadata| metadata.is_file())
        })
    }

    fn unit_enabled(&mut self, unit: &str) -> Result<bool> {
        Ok(command(
            self.runner,
            "systemctl",
            &["--quiet", "is-enabled", &format!("{unit}.service")],
        )?
        .success)
    }

    /// Ask what the engine and its GUI were before the packages change them, and decide what
    /// this run leaves enabled. The installer enables the stage on a speaker that has none; a
    /// speaker that has it, or an engine of its own, keeps what it had: an engine or GUI that
    /// was switched off stays off, and a speaker with its own engine and no GUI does not get
    /// the unauthenticated GUI.
    fn snapshot_stage(&mut self) -> Result<()> {
        let stage = self.version("tessera-dsp")?.is_some();
        let engine = stage || self.unit_file(DSP_UNITS[0]);
        self.before = Before {
            engine_enabled: engine && self.unit_enabled(DSP_UNITS[0])?,
            gui_enabled: self.unit_enabled(DSP_UNITS[1])?,
        };
        self.gui_bound = fs::read_to_string(self.paths.at(GUI_CONFIG))
            .ok()
            .and_then(|config| dsp::gui_bind(&config));
        self.want_engine = !engine || self.before.engine_enabled;
        self.want_gui = if stage {
            self.before.gui_enabled
        } else {
            !engine
        };
        Ok(())
    }

    fn install(&mut self) -> Result<()> {
        self.step = "manifest";
        let manifest = manifest::load(&self.paths.bundle, self.runner)?;
        self.platform(&manifest)?;
        self.step = "checksum";
        manifest::verify(&self.paths.bundle, &manifest, self.runner)?;
        self.read_record()?;
        ensure(
            self.record.profile.is_empty() || self.options.profile == self.record.profile,
            &format!(
                "profile is {}; remove it before changing profiles",
                self.record.profile
            ),
        )?;
        self.decide_dsp(&manifest)?;
        self.step = "plan";
        println!(
            "Plan: tessera {} ({}), profile {}{}",
            manifest.version,
            manifest.arch,
            self.options.profile,
            if self.use_dsp { " with CamillaDSP" } else { "" }
        );
        for dep in &manifest.apt_depends {
            println!("  apt dependency: {dep}");
        }
        for deb in selected(&manifest, self.use_dsp) {
            println!("  package: {}", deb.file);
        }
        if self.options.profile != "none" {
            println!(
                "  create if absent: /etc/tessera/config.d/60-{}.toml and profile unit drop-ins",
                self.options.profile
            );
        }
        if self.use_dsp {
            println!(
                "  CamillaDSP: seed /opt/camilladsp/configs/Speaker.yml if no configuration is active; portal drop-ins; {}",
                match (self.want_engine, self.want_gui) {
                    (true, true) =>
                        "start camilladsp and camillagui (the GUI on the management address only)",
                    (true, false) => "start camilladsp; the GUI stays off",
                    (false, true) => "start camillagui; the engine stays off",
                    (false, false) => "the engine and the GUI stay off",
                }
            );
            let aside = moves_aside(self.paths);
            if !aside.is_empty() {
                println!(
                    "  moved aside, kept as *.{ASIDE} (the packaged unit takes over): {}",
                    aside.join(", ")
                );
            }
        }
        if self.strip_bridge {
            println!(
                "  upgrade: remove the speaker profile's [[bridge]] from {SPEAKER_CONFIG} (a running daemon reloads to release the sound card)"
            );
        }
        println!(
            "  apply admin memberships; enable daemon; verify status; write /var/lib/tessera-bundle/record.json"
        );
        if self.options.offline {
            println!("  offline: verify all dependencies; install with --no-download");
        }
        if self.options.dry {
            println!("dry-run: no changes");
            return Ok(());
        }
        if !self.options.yes {
            ensure(
                self.runner.confirm("Install bundle?")?,
                "installation declined",
            )?;
        }
        for user in &self.options.admins {
            ensure(
                command(self.runner, "id", &[user])?.success,
                &format!("unknown admin user: {user}"),
            )?;
        }
        // A restart that was pending is done once the daemon is another run than the one the
        // record was written for (or is not running: it starts with the new binary).
        let mut restart_done = false;
        if self.record.restart_pending && !self.record.daemon_invocation.is_empty() {
            let active = command(
                self.runner,
                "systemctl",
                &["--quiet", "is-active", "tesserad"],
            )?
            .success;
            let current = if active {
                self.invocation()?
            } else {
                String::new()
            };
            if !active || (!current.is_empty() && current != self.record.daemon_invocation) {
                self.record.restart_pending = false;
                self.record.daemon_invocation.clear();
                restart_done = true;
                println!("restart done: tesserad has been restarted since the last install");
            }
        }
        let old = self.record.clone();
        let installed = self.version("tessera")?;
        let alsa = self.version("tessera-alsa")?;
        let web = self.version("tessera-web")?;
        let dsp_installed = self.version("tessera-dsp")?;
        let dsp_current = !self.use_dsp || dsp_installed.as_deref() == Some(&manifest.version);
        self.record.version = manifest.version.clone();
        self.record.commit = manifest.commit.clone();
        self.record.profile = self.options.profile.clone();
        self.record.packages = vec!["tessera".into(), "tessera-alsa".into()];
        if old.packages.iter().any(|pkg| pkg == "tessera-web") || web.is_none() {
            self.record.packages.push("tessera-web".into());
        }
        if self.use_dsp {
            self.record.packages.push("tessera-dsp".into());
        }
        self.record.removed = false;
        self.step = "version";
        if let Some(version) = &installed {
            ensure(
                !self.compare(version, "gt", &manifest.version)?,
                "refusing downgrade",
            )?;
        }
        ensure(
            installed.as_deref() != Some(&manifest.version)
                || old.commit == manifest.commit
                || self.options.reinstall,
            "same version has different or unknown build provenance; use --reinstall to install the bundle build",
        )?;
        self.step = "dependencies";
        let mut admins_satisfied = true;
        for user in self.options.admins.clone() {
            let groups = self.groups(&user)?;
            admins_satisfied &= ["tessera", "tessera-admin"]
                .iter()
                .all(|group| groups.iter().any(|value| value == group));
        }
        if old.complete
            && old.version == manifest.version
            && installed.as_deref() == Some(&manifest.version)
            && alsa.as_deref() == Some(&manifest.version)
            && web.as_deref() == Some(&manifest.version)
            && dsp_current
            && !self.options.restart
            && !self.options.reinstall
            && admins_satisfied
        {
            self.dependencies(&manifest)?;
            self.step = "verification";
            self.status(false)?;
            if self.use_dsp {
                // Naming the management port and installing again must work without
                // `--reinstall` (HW-3, D16): the configuration is applied, nothing is installed.
                self.step = "gui";
                self.gui_settings()?;
            }
            if restart_done {
                // Only the flag changes: the record is what the last run wrote.
                self.record = old.clone();
                self.save()?;
            }
            if old.restart_pending {
                println!("restart pending");
            }
            println!("install: already installed; verified");
            return Ok(());
        }
        self.record.timestamp = checked(self.runner, "date", &["-u", "+%FT%TZ"])?;
        self.record.complete = false;
        if self.options.offline {
            self.dependencies(&manifest)
                .map_err(|_| Error("offline dependency missing or too old".into()))?;
        } else {
            // Naming a package to apt marks it manually installed. A dependency that is
            // already there keeps the mark it has: it is named only if it is too old (apt
            // upgrades it), and then an automatic mark is put back afterwards.
            let mut names = Vec::new();
            let mut was_auto = Vec::new();
            for dep in &manifest.apt_depends {
                let (name, constraint) = manifest::dependency(dep)?;
                let Some(installed) = self.version(name)? else {
                    // Another package may stand in for it (Provides): nothing to install.
                    if !self.provided(name, constraint)? {
                        self.record.dependencies_installed.push(name.into());
                        names.push(name.to_owned());
                    }
                    continue;
                };
                let too_old = match constraint {
                    Some((op, required)) => !self.compare(&installed, op, required)?,
                    None => false,
                };
                if too_old {
                    let marks = command(self.runner, "apt-mark", &["showauto", name])?;
                    if marks.success && marks.stdout.lines().any(|line| line.trim() == name) {
                        was_auto.push(name.to_owned());
                    }
                    names.push(name.to_owned());
                }
            }
            // A dependency apt cannot resolve refuses the run here, before anything is written.
            if !names.is_empty() {
                self.simulate_dependencies(&names)?;
            }
            self.record.files.push(record::FILE.into());
            self.save()?;
            if !names.is_empty() {
                let mut args = vec!["-y".into(), "--no-install-recommends".into()];
                args.extend(names.clone());
                self.apt_install(&args)?;
                // Only a package apt really installed is the bundle's to mark again at
                // purge; a name another package provides is not one.
                for name in names {
                    if self.version(&name)?.is_none() {
                        self.record
                            .dependencies_installed
                            .retain(|kept| *kept != name);
                    }
                }
                for name in was_auto {
                    checked(self.runner, "apt-mark", &["auto", &name])?;
                }
                self.save()?;
            }
            self.dependencies(&manifest)?;
        }
        self.step = "packages";
        let running = command(
            self.runner,
            "systemctl",
            &["--quiet", "is-active", "tesserad"],
        )?
        .success;
        // Which run of the daemon a pending restart is about (the old binary's).
        let daemon_run = if running {
            self.invocation()?
        } else {
            String::new()
        };
        for group in ["tessera", "tessera-admin"] {
            if !command(self.runner, "getent", &["group", group])?.success {
                self.record.groups_created.push(group.into());
            }
        }
        if !command(self.runner, "getent", &["passwd", "tessera"])?.success {
            self.record.users_created.push("tessera".into());
        }
        if self.use_dsp {
            // The tessera-dsp package creates the account; the record lets uninstall
            // remove what this bundle created.
            if !command(self.runner, "getent", &["group", "camilla"])?.success {
                self.record.groups_created.push("camilla".into());
            }
            if !command(self.runner, "getent", &["passwd", "camilla"])?.success {
                self.record.users_created.push("camilla".into());
            }
        }
        self.record.files.push(record::FILE.into());
        if running && (installed.as_deref() != Some(&manifest.version) || self.options.reinstall) {
            self.record.restart_pending = true;
        }
        self.save()?;
        if installed.as_deref() != Some(&manifest.version)
            || alsa.as_deref() != Some(&manifest.version)
            || web.as_deref() != Some(&manifest.version)
            || !dsp_current
            || self.options.reinstall
        {
            let mut args = vec!["-y".into(), "--no-install-recommends".into()];
            if self.options.offline {
                args.push("--no-download".into());
            }
            if self.options.reinstall {
                args.push("--reinstall".into());
            }
            args.extend(
                [
                    "-o",
                    "Dpkg::Options::=--force-confdef",
                    "-o",
                    "Dpkg::Options::=--force-confold",
                ]
                .map(str::to_owned),
            );
            args.extend(selected(&manifest, self.use_dsp).iter().map(|deb| {
                self.paths
                    .bundle
                    .join(&deb.file)
                    .to_string_lossy()
                    .into_owned()
            }));
            self.apt_install(&args)?;
        }
        self.step = "profile";
        self.profile(running)?;
        self.step = "dsp";
        if self.use_dsp {
            self.dsp()?;
        }
        self.step = "memberships";
        for user in self.options.admins.clone() {
            let groups = self.groups(&user)?;
            for group in ["tessera", "tessera-admin"] {
                if !groups.iter().any(|value| value == group) {
                    self.record.users_modified.push(format!("{user}:{group}"));
                    self.save()?;
                }
            }
            checked(
                self.runner,
                "usermod",
                &["-aG", "tessera,tessera-admin", &user],
            )?;
        }
        self.step = "service";
        checked(self.runner, "systemctl", &["daemon-reload"])?;
        if running {
            checked(self.runner, "systemctl", &["enable", "tesserad"])?;
            if self.options.restart {
                checked(self.runner, "systemctl", &["restart", "tesserad"])?;
                self.record.restart_pending = false;
            } else if installed.as_deref() != Some(&manifest.version) || self.record.restart_pending
            {
                self.record.restart_pending = true;
                println!("restart pending");
            }
        } else {
            checked(self.runner, "systemctl", &["enable", "--now", "tesserad"])?;
            self.record.restart_pending = false;
        }
        if self.use_dsp {
            self.step = "dsp-services";
            self.dsp_services(running)?;
        }
        self.step = "verification";
        self.status(true)?;
        if self.options.profile == "speaker" && !self.use_dsp {
            let bridges = checked(self.runner, "tesseractl", &["--json", "bridges"])?;
            if bridges.contains("reopening") {
                eprintln!("warning: sound card unavailable; bridge reopening");
            }
        }
        if self.use_dsp {
            self.dsp_ready()?;
        }
        self.step = "record";
        self.record.complete = true;
        self.record.daemon_invocation = if self.record.restart_pending {
            daemon_run
        } else {
            String::new()
        };
        self.save()?;
        println!("install: complete");
        Ok(())
    }

    /// systemd's `InvocationID` of the daemon: another one after every start of the unit.
    fn invocation(&mut self) -> Result<String> {
        let reply = command(
            self.runner,
            "systemctl",
            &[
                "show",
                "--property=InvocationID",
                "--value",
                "tesserad.service",
            ],
        )?;
        let id = reply.stdout.trim();
        Ok(
            if reply.success && id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit()) {
                id.to_ascii_lowercase()
            } else {
                String::new()
            },
        )
    }

    fn status(&mut self, retry: bool) -> Result<()> {
        for attempt in 0..if retry { 20 } else { 1 } {
            let reply = command(self.runner, "tesseractl", &["status"])?;
            if reply.success {
                println!(
                    "{}",
                    visible(reply.stdout.lines().next().unwrap_or_default())
                );
                return Ok(());
            }
            if attempt < 19 && retry {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        Err(Error("daemon status not ready".into()))
    }

    fn profile(&mut self, running: bool) -> Result<()> {
        if self.options.profile == "none" {
            return Ok(());
        }
        let profile = self
            .paths
            .bundle
            .join("profiles")
            .join(&self.options.profile);
        let dest = format!("/etc/tessera/config.d/60-{}.toml", self.options.profile);
        if !self.paths.at(&dest).exists() {
            if running {
                self.record.restart_pending = true;
            }
            self.record.files.push(dest.clone());
            self.save()?;
            fs::create_dir_all(self.paths.at(&dest).parent().expect("config parent"))?;
            // The CamillaDSP speaker has no in-daemon bridge: the card has one owner.
            let template = if self.use_dsp {
                format!("60-{}-dsp.toml.in", self.options.profile)
            } else {
                format!("60-{}.toml.in", self.options.profile)
            };
            let content = fs::read_to_string(profile.join(template))?
                .replace("@AUDIO_CARD@", &self.options.card);
            fs::write(self.paths.at(&dest), content)?;
        } else if self.strip_bridge {
            // Keep everything else exactly as the operator has it.
            let path = self.paths.at(&dest);
            let content = dsp::without_speaker_bridge(&fs::read_to_string(&path)?);
            fs::write(&path, content)?;
            self.bridge_stripped = true;
        }
        // The bridge's unit drop-in (SupplementaryGroups=audio) is only for the
        // in-daemon bridge.
        let dropins = profile.join("tesserad.service.d");
        if dropins.is_dir() && !self.use_dsp {
            for entry in fs::read_dir(dropins)? {
                let entry = entry?;
                if entry
                    .path()
                    .extension()
                    .is_none_or(|extension| extension != "conf")
                {
                    continue;
                }
                let dest = format!(
                    "/etc/systemd/system/tesserad.service.d/{}",
                    entry.file_name().to_string_lossy()
                );
                if !self.paths.at(&dest).exists() {
                    if running {
                        self.record.restart_pending = true;
                    }
                    self.record.files.push(dest.clone());
                    self.save()?;
                    fs::create_dir_all(self.paths.at(&dest).parent().expect("drop-in parent"))?;
                    fs::copy(entry.path(), self.paths.at(&dest))?;
                } else if !self.record.files.contains(&dest) {
                    eprintln!(
                        "warning: existing bridge.conf retained; verify SupplementaryGroups=audio so the daemon can open the card"
                    );
                }
            }
        }
        Ok(())
    }

    /// Whether this run may change the host it runs on: a test root never reaches the
    /// real accounts, mixers and polkit.
    fn live(&self) -> bool {
        self.paths.root == Path::new("/")
    }

    /// Write `content` to a path the installer owns, once: recorded, so uninstall
    /// removes it, and never over a file that is already there.
    fn write_once(&mut self, dest: &str, content: &str) -> Result<()> {
        if self.paths.at(dest).exists() {
            return Ok(());
        }
        self.record.files.push(dest.into());
        self.save()?;
        fs::create_dir_all(self.paths.at(dest).parent().expect("drop-in parent"))?;
        fs::write(self.paths.at(dest), content)?;
        Ok(())
    }

    /// The playback format of the seeded configuration: the first of S32_LE, S24_3_LE,
    /// S24_4_LE and S16_LE (the EQ engine's names) that the card plays, asked of the
    /// kernel when the card can be opened. S32_LE otherwise: a `plug` selector converts any format, and a card that is
    /// absent or in use cannot be asked.
    fn playback_format(&self, card: Option<&dsp::Card>) -> &'static str {
        let Some(card) = card.filter(|card| self.live() && !card.plug) else {
            return dsp::DEFAULT_FORMAT;
        };
        let index = fs::read_link(format!("/proc/asound/{}", card.name))
            .ok()
            .and_then(|target| dsp::card_index(&target.to_string_lossy()));
        match index.and_then(|index| pcm::playback_format(index, card.device)) {
            Some(format) => {
                if format != dsp::DEFAULT_FORMAT {
                    eprintln!(
                        "note: the sound card does not play {}; the configuration plays {format}",
                        dsp::DEFAULT_FORMAT
                    );
                }
                format
            }
            None => {
                eprintln!(
                    "note: the sound card's formats could not be read (it may be absent or in use); the configuration plays {}. If the EQ engine cannot start, set devices.playback.format in its GUI",
                    dsp::DEFAULT_FORMAT
                );
                dsp::DEFAULT_FORMAT
            }
        }
    }

    /// The CamillaDSP stage. The tessera-dsp package created the account, the data
    /// directories and an empty state; this chooses the sound card, seeds a flat
    /// configuration only when none is active (an existing setup is never touched) and
    /// turns on the portal's Speaker page.
    fn dsp(&mut self) -> Result<()> {
        self.shadowing_units()?;
        let selector = self.options.card.clone();
        let card = dsp::card(&selector);
        let playback = card.as_ref().map_or(selector, |card| card.playback.clone());
        let rate = dsp::sample_rate(
            &fs::read_to_string(self.paths.at(SPEAKER_CONFIG)).unwrap_or_default(),
        );
        let configs = self.paths.at(DSP_CONFIGS);
        data::real_directory(&configs)?;
        let state = self.paths.at(DSP_STATE);
        // /opt/camilladsp is the camilla account's: a link planted there is never followed.
        let current = match fs::symlink_metadata(&state) {
            Ok(metadata) => {
                ensure(
                    metadata.is_file(),
                    &format!("{DSP_STATE} is not a regular file"),
                )?;
                Some(fs::read_to_string(&state)?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let current_text = current.as_deref().unwrap_or_default();
        if dsp::state_unset(current_text) {
            let seed = configs.join("Speaker.yml");
            let template = fs::read_to_string(
                self.paths
                    .bundle
                    .join("profiles/speaker/camilladsp-Speaker.yml.in"),
            )?;
            let format = self.playback_format(card.as_ref());
            // Recorded before it is made, so a purge removes this file and no other of the
            // data directory; a file that is there already is not ours.
            let ours = self.record.files.iter().any(|file| file == record::SEED);
            if !ours {
                self.record.files.push(record::SEED.into());
                self.save()?;
            }
            if !data::create_new(&seed, 0o664, &dsp::seed(&template, rate, &playback, format))?
                && !ours
            {
                self.record.files.retain(|file| file != record::SEED);
                self.save()?;
            }
            let named = format!("{DSP_CONFIGS}/Speaker.yml");
            let content = if current_text.trim().is_empty() {
                dsp::new_state(&named)
            } else {
                dsp::state_with_config(current_text, &named)
            };
            if current.is_some() {
                data::rewrite_regular(&state, 0o664, &content)?;
            } else {
                data::create_new(&state, 0o664, &content)?;
            }
            if self.live() {
                checked(
                    self.runner,
                    "chown",
                    &[
                        "-h",
                        "camilla:camilla",
                        &seed.to_string_lossy(),
                        &state.to_string_lossy(),
                    ],
                )?;
            }
        }
        // Portal: the Speaker page, the service switches, the EQ engine, and the amplifier
        // volume if the card has one.
        let mut volume = None;
        if self.live() {
            if let Some(card) = &card {
                // The card must be there now; its index is not recorded anywhere, the
                // portal follows the card by its name.
                let present = fs::read_link(format!("/proc/asound/{}", card.name))
                    .ok()
                    .is_some_and(|target| dsp::card_index(&target.to_string_lossy()).is_some());
                let controls = command(self.runner, "amixer", &["-c", &card.name, "scontrols"])?;
                if present {
                    volume = dsp::volume_control(&controls.stdout);
                }
            }
        }
        if volume.is_none() {
            eprintln!(
                "note: no volume control found on the sound card; the portal will have no Volume card"
            );
        }
        let binding = self.gui_binding()?;
        let gui_address = binding.address;
        self.gui_rebound = binding.rebound;
        let mut portal = fs::read_to_string(
            self.paths
                .bundle
                .join("profiles/speaker/web.d/60-speaker.toml.in"),
        )?;
        if !self.paths.at(PORTAL_DROPIN).exists() && self.portal_settings().services {
            // A list in a drop-in replaces the whole list of the earlier files: the
            // operator's own is never written over.
            portal = portal::without_services(&portal);
            println!(
                "note: the portal's service list in /etc/tessera is yours and is kept as it is; the EQ engine switch is on the Services page only if your list has camilladsp (see {PORTAL_DROPIN})"
            );
        }
        portal = portal::with_gui_link(&portal, gui_address.as_deref());
        if let (Some(control), Some(card)) = (volume, &card) {
            portal.push_str(&format!(
                "\n[appliance.volume]\ncard = \"{}\"\ncontrol = \"{control}\"\nmin_db = -60\nmax_db = 0\n",
                card.name
            ));
        }
        self.write_once(PORTAL_DROPIN, &portal)?;
        if let Some(address) = &gui_address {
            self.gui_link(address)?;
        }
        let mut unit = String::from("[Service]\n");
        for path in [DSP_CONFIGS, DSP_COEFFS, DSP_STATE] {
            unit.push_str(&format!("ReadWritePaths=-{path}\n"));
        }
        if volume.is_some() {
            unit.push_str(&dsp::control_devices());
        }
        self.write_once(PORTAL_UNIT_DROPIN, &unit)?;
        self.drop_redundant_example()?;
        if self.live() {
            // The portal writes the EQ files, so its account is in group camilla. The
            // tessera-dsp package does this too, but only if tessera-web is already
            // configured when it is, which one apt run does not guarantee.
            if command(self.runner, "getent", &["passwd", "tessera-web"])?.success
                && !self.groups("tessera-web")?.iter().any(|g| g == "camilla")
            {
                checked(self.runner, "usermod", &["-aG", "camilla", "tessera-web"])?;
            }
            // The polkit rules follow the portal's service list, drop-ins included;
            // check the result first.
            checked(self.runner, "tessera-web", &["check"])?;
            let rules = command(self.runner, "tessera-web", &["appliance", "polkit"])?;
            ensure(rules.success, "tessera-web appliance polkit failed")?;
            polkit::write(&self.paths.root, &rules.stdout)?;
        }
        Ok(())
    }

    /// A unit file of the operator's in `/etc/systemd/system` wins over the packaged unit in
    /// `/usr/lib/systemd/system`, and would keep the engine on its old definition (its old
    /// program, none of the restart and start links with the daemon) without a word. Move it
    /// aside, with the drop-ins that were written for it, and say so. Drop-ins next to the
    /// packaged unit are the operator's changes to it and stay.
    fn shadowing_units(&mut self) -> Result<()> {
        let mut moved = false;
        for unit in DSP_UNITS {
            let file = format!("{UNIT_DIRECTORY}/{unit}.service");
            let path = self.paths.at(&file);
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                if fs::read_link(&path).is_ok_and(|target| target == Path::new("/dev/null")) {
                    eprintln!(
                        "warning: {file} masks the unit: the stage's {unit} cannot start until `systemctl unmask {unit}`"
                    );
                }
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let aside = Self::free_name(self.paths, &format!("{file}.{ASIDE}"));
            fs::rename(&path, self.paths.at(&aside))?;
            let dropins = format!("{file}.d");
            let kept = if fs::symlink_metadata(self.paths.at(&dropins))
                .is_ok_and(|metadata| metadata.is_dir())
            {
                let target = Self::free_name(
                    self.paths,
                    &format!("{UNIT_DIRECTORY}/{unit}.service.d.{ASIDE}"),
                );
                fs::rename(self.paths.at(&dropins), self.paths.at(&target))?;
                format!(" and its drop-ins to {target}")
            } else {
                String::new()
            };
            let links = self.drop_enable_links(unit, &file)?;
            let enable = if links.is_empty() {
                String::new()
            } else {
                format!(" and systemctl enable {unit}")
            };
            println!(
                "note: {file} is not part of this bundle and would have kept the old definition of {unit}; moved to {aside}{kept}. The packaged unit takes over (a running {unit} keeps its old program until it is restarted: systemctl restart {unit}). To go back: stop and disable {unit}, move the file back and run systemctl daemon-reload{enable}."
            );
            for link in links {
                println!(
                    "note: removed {link}: it pointed at the unit that moved and would have stayed dangling; {unit} is enabled from the packaged unit instead"
                );
            }
            moved = true;
        }
        if moved {
            checked(self.runner, "systemctl", &["daemon-reload"])?;
            println!("systemd reloaded: the packaged units are in effect");
        }
        Ok(())
    }

    /// The enable links of a unit that was moved aside (`multi-user.target.wants/camilladsp.service`
    /// and the like, made by `systemctl enable` of the operator's unit) point at a file that is
    /// no longer there: `is-enabled` still says `enabled` and nothing repairs them (HW-3, D17).
    /// They are removed; `systemctl enable` of the packaged unit makes its own. Answers the
    /// links removed.
    fn drop_enable_links(&self, unit: &str, file: &str) -> Result<Vec<String>> {
        let mut removed = Vec::new();
        let Ok(entries) = fs::read_dir(self.paths.at(UNIT_DIRECTORY)) else {
            return Ok(removed);
        };
        for entry in entries.flatten() {
            let directory = entry.file_name().to_string_lossy().into_owned();
            if !directory.ends_with(".wants") && !directory.ends_with(".requires") {
                continue;
            }
            let link = entry.path().join(format!("{unit}.service"));
            // `systemctl enable` links to the unit's path; a relative link says the same.
            let relative = format!("../{unit}.service");
            if fs::read_link(&link)
                .is_ok_and(|target| target == Path::new(file) || target == Path::new(&relative))
            {
                fs::remove_file(&link)?;
                removed.push(format!("{UNIT_DIRECTORY}/{directory}/{unit}.service"));
            }
        }
        removed.sort();
        Ok(removed)
    }

    /// `name`, or `name.2`, `name.3` ... when something is there already: a second move
    /// aside never replaces the first.
    fn free_name(paths: &Paths, name: &str) -> String {
        let mut candidate = name.to_owned();
        let mut serial = 1;
        while fs::symlink_metadata(paths.at(&candidate)).is_ok() {
            serial += 1;
            candidate = format!("{name}.{serial}");
        }
        candidate
    }

    /// What the operator's `web.toml` and `web.d` say that the profile must respect.
    fn portal_settings(&self) -> portal::Settings {
        let mut texts = Vec::new();
        if let Ok(text) = fs::read_to_string(self.paths.at("/etc/tessera/web.toml")) {
            texts.push(text);
        }
        let mut files: Vec<_> = fs::read_dir(self.paths.at("/etc/tessera/web.d"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "toml")
                    && path
                        .file_name()
                        .is_some_and(|name| !name.to_string_lossy().starts_with('.'))
                    && path.is_file()
            })
            .collect();
        files.sort();
        texts.extend(
            files
                .iter()
                .filter_map(|path| fs::read_to_string(path).ok()),
        );
        portal::read(&texts)
    }

    /// The wired port the management address is on, and that address: the port
    /// `[network] interface` names, else the only port with a default route. `Err` says why
    /// there is none (several default routes are as ambiguous here as for the network helper,
    /// which refuses them too).
    fn management(&mut self) -> std::result::Result<(String, String), String> {
        let port = if let Some(port) = self.portal_settings().interface {
            port
        } else {
            let routes = command(self.runner, "ip", &["-o", "-4", "route", "show", "default"])
                .map(|reply| reply.stdout)
                .unwrap_or_default();
            let ports = dsp::default_ports(&routes);
            match ports.as_slice() {
                [port] => port.clone(),
                [] => return Err("no port has a default route".into()),
                many => return Err(format!("{} all have default routes", many.join(", "))),
            }
        };
        if port.starts_with('-')
            || !port
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_.:@-".contains(&c))
        {
            return Err(format!("`{port}` is not a port name"));
        }
        let listing = command(
            self.runner,
            "ip",
            &["-o", "-4", "addr", "show", "dev", &port, "scope", "global"],
        )
        .map(|reply| reply.stdout)
        .unwrap_or_default();
        dsp::first_address(&listing)
            .map(|address| (port.clone(), address))
            .ok_or_else(|| format!("{port} has no IPv4 address"))
    }

    /// Whether `path`, a conffile of `package`, is still the file the package shipped: its md5
    /// is the one dpkg recorded. dpkg replaces such a file on an upgrade and keeps one the
    /// operator changed, so this is "the operator never touched it".
    fn conffile_unmodified(&mut self, package: &str, path: &str) -> bool {
        let query = format!("{package}:{}", self.arch);
        let Ok(listing) = command(
            self.runner,
            "dpkg-query",
            &["-W", "-f=${Conffiles}\n", &query],
        ) else {
            return false;
        };
        let shipped = listing.stdout.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some(path)).then(|| fields.next().map(str::to_owned))?
        });
        let actual = command(
            self.runner,
            "md5sum",
            &["--", &self.paths.at(path).to_string_lossy()],
        )
        .ok()
        .filter(|reply| reply.success)
        .and_then(|reply| reply.stdout.split_whitespace().next().map(str::to_owned));
        listing.success && shipped.is_some() && shipped == actual
    }

    /// Where the EQ engine's GUI listens, which has no login. The packaged configuration is
    /// loopback; a GUI this run leaves on, with the file as the package shipped it, gets the
    /// management address, so it is reachable from the network the speaker is managed on and
    /// from no other. A GUI that listens on every interface (0.0.0.0, the default of the
    /// first 0.3.0 candidates) is narrowed the same way, a changed file included; any other
    /// address the operator chose is kept. Answers the address the portal's link uses, if the
    /// GUI is on and listens on one, and the address the file says when this run changed it.
    fn gui_binding(&mut self) -> Result<Binding> {
        let path = self.paths.at(GUI_CONFIG);
        let Ok(config) = fs::read_to_string(&path) else {
            return Ok(Binding {
                address: None,
                rebound: None,
            });
        };
        let now = dsp::gui_bind(&config).unwrap_or_default();
        let adopt = dsp::wildcard(&now)
            || (self.want_gui
                && now == dsp::GUI_LOOPBACK
                && self.conffile_unmodified("tessera-dsp", GUI_CONFIG));
        let mut address = now.clone();
        let mut rebound = None;
        if adopt {
            address = dsp::GUI_LOOPBACK.to_owned();
            if self.want_gui {
                match self.management() {
                    Ok((port, management)) => {
                        println!(
                            "GUI: {GUI_CONFIG} now listens on {management}, the management address ({port}), not on every interface"
                        );
                        address = management;
                    }
                    Err(why) => eprintln!(
                        "note: the EQ engine's GUI listens on this machine only: the management port is unknown ({why}). Name it with `[network] interface = \"<port>\"` in /etc/tessera/web.toml or a web.d drop-in and install again, or set bind_address in {GUI_CONFIG} to this speaker's address and restart camillagui."
                    ),
                }
            } else if dsp::wildcard(&now) {
                println!("GUI: {GUI_CONFIG} listened on every interface; now on this machine only");
            }
            if address != now {
                fs::write(&path, dsp::with_gui_bind(&config, &address))?;
            }
        }
        // The running GUI has the address the file said before the run, whoever changed it since.
        if self
            .gui_bound
            .as_deref()
            .is_some_and(|before| before != address)
        {
            rebound = Some(address.clone());
        }
        Ok(Binding {
            address: (self.want_gui && address != dsp::GUI_LOOPBACK && !dsp::wildcard(&address))
                .then_some(address),
            rebound,
        })
    }

    /// The portal's link to the GUI names the address the GUI listens on (`gui_url`, HW-3,
    /// D17). A drop-in that was written before the address was known has none: the line is
    /// added to its `[appliance.camilladsp]` table, and only that line. A drop-in that
    /// has a `gui_url` of its own, or no such table, is the operator's: the line to set is
    /// printed.
    fn gui_link(&mut self, address: &str) -> Result<()> {
        let path = self.paths.at(PORTAL_DROPIN);
        let Ok(text) = fs::read_to_string(&path) else {
            return Ok(());
        };
        if text.contains(address) {
            return Ok(());
        }
        let url = portal::gui_url(address);
        if let Some(linked) = portal::add_gui_link(&text, address) {
            fs::write(&path, linked)?;
            println!(
                "note: the EQ GUI listens on {address}; added gui_url = \"{url}\" under [appliance.camilladsp] in {PORTAL_DROPIN} for the portal's EQ engine link"
            );
        } else {
            println!(
                "note: the EQ GUI listens on {address}; for the portal's EQ engine link set gui_url = \"{url}\" under [appliance.camilladsp] in {PORTAL_DROPIN}"
            );
        }
        Ok(())
    }

    /// What a run of the version that is installed still sets, the way the first run does it:
    /// the GUI's address and the portal's link to it. No package is installed and the audio
    /// path is not touched.
    fn gui_settings(&mut self) -> Result<()> {
        let binding = self.gui_binding()?;
        if let Some(address) = &binding.address {
            self.gui_link(address)?;
        }
        if let Some(address) = binding.rebound {
            if self.unit_active(DSP_UNITS[1])? {
                self.restart_gui(&address)?;
            }
        }
        Ok(())
    }

    fn unit_active(&mut self, unit: &str) -> Result<bool> {
        Ok(command(self.runner, "systemctl", &["--quiet", "is-active", unit])?.success)
    }

    /// A running `camillagui` keeps the socket it opened: it listens on the new address only
    /// after a restart. It has no audio, so the installer does it, and says so.
    fn restart_gui(&mut self, address: &str) -> Result<()> {
        checked(self.runner, "systemctl", &["restart", DSP_UNITS[1]])?;
        println!(
            "camillagui restarted: it listens on {address} now (the EQ engine and the audio were not touched)"
        );
        Ok(())
    }

    /// The unit drop-in an earlier build's documentation told the operator to copy
    /// (`appliance-paths.conf`) is removed when every setting in it is in the profile's
    /// drop-in as well, or is one the portal no longer needs ([`SUPERSEDED`]): it only repeats
    /// the profile, and the lines would be in the unit twice. A setting that is neither is the
    /// operator's, and keeps the file.
    fn drop_redundant_example(&mut self) -> Result<()> {
        let example = self.paths.at(PORTAL_EXAMPLE_DROPIN);
        if !fs::symlink_metadata(&example).is_ok_and(|metadata| metadata.is_file()) {
            return Ok(());
        }
        let (Ok(old), Ok(ours)) = (
            fs::read_to_string(&example),
            fs::read_to_string(self.paths.at(PORTAL_UNIT_DROPIN)),
        ) else {
            return Ok(());
        };
        let settings = |text: &str| -> Vec<String> {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#') && *line != "[Service]")
                .map(str::to_owned)
                .collect()
        };
        let (old, ours) = (settings(&old), settings(&ours));
        if !old.is_empty()
            && old
                .iter()
                .all(|line| ours.contains(line) || SUPERSEDED.contains(&line.as_str()))
        {
            fs::remove_file(&example)?;
            println!(
                "note: removed {PORTAL_EXAMPLE_DROPIN}: every setting in it is in {PORTAL_UNIT_DROPIN} or not needed any more"
            );
        }
        Ok(())
    }

    /// Started after the daemon: CamillaDSP opens the PCM "tessera" and the card. A
    /// running engine is left alone on an upgrade (it keeps playing); restart it when
    /// convenient. What this run leaves enabled is decided by [`Self::snapshot_stage`].
    fn dsp_services(&mut self, running: bool) -> Result<()> {
        if self.bridge_stripped && running {
            // The bridge was dropped from the configuration: a running daemon lets go of
            // the card on a (confirmed) reload, which is all CamillaDSP needs from it.
            checked(self.runner, "tesseractl", &["--yes", "config", "reload"])?;
        }
        let wanted: Vec<&str> = [
            (DSP_UNITS[0], self.want_engine),
            (DSP_UNITS[1], self.want_gui),
        ]
        .into_iter()
        .filter_map(|(unit, wanted)| wanted.then_some(unit))
        .collect();
        if !wanted.is_empty() {
            let mut argv = vec!["enable"];
            argv.extend(&wanted);
            checked(self.runner, "systemctl", &argv)?;
        }
        // A GUI that runs keeps its old socket after its address changed (HW-3, D16); one that
        // is started below opens the new one.
        let rebound = match self.gui_rebound.take() {
            Some(address) if self.unit_active(DSP_UNITS[1])? => Some(address),
            _ => None,
        };
        for unit in DSP_UNITS {
            if wanted.contains(&unit) {
                if !command(self.runner, "systemctl", &["--quiet", "is-active", unit])?.success {
                    checked(self.runner, "systemctl", &["start", unit])?;
                }
            } else {
                // The package turned the unit on when it was installed; this speaker keeps
                // what it had. Nothing is stopped: a unit that runs without being enabled
                // was started by hand.
                checked(self.runner, "systemctl", &["disable", unit])?;
                println!("{unit} stays off, as it was on this speaker");
            }
        }
        if let Some(address) = rebound {
            self.restart_gui(&address)?;
        }
        // The portal picks up web.d, its drop-in and the camilla group; it is not part of
        // the audio path.
        if command(
            self.runner,
            "systemctl",
            &["--quiet", "is-active", "tessera-web"],
        )?
        .success
        {
            checked(self.runner, "systemctl", &["restart", "tessera-web"])?;
        }
        Ok(())
    }

    fn dsp_ready(&mut self) -> Result<()> {
        if !self.want_engine && !self.want_gui {
            return Ok(());
        }
        for attempt in 0..20 {
            let mut ready = true;
            for (unit, wanted) in [
                (DSP_UNITS[0], self.want_engine),
                (DSP_UNITS[1], self.want_gui),
            ] {
                if wanted {
                    ready &=
                        command(self.runner, "systemctl", &["--quiet", "is-active", unit])?.success;
                }
            }
            if ready {
                return Ok(());
            }
            if attempt < 19 {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        eprintln!(
            "warning: CamillaDSP is not running (sound card missing or busy?); see journalctl -u camilladsp"
        );
        Ok(())
    }

    fn uninstall(&mut self) -> Result<()> {
        self.root()?;
        let found = self.read_record()?;
        if !found && !(self.options.dry && self.paths.uid != 0) {
            println!(
                "No bundle record; removing whatever tessera packages are installed; account ownership is unknown."
            );
        }
        self.options.purge |= self.record.purge_pending;
        self.options.keep |= self.record.keep_users;
        self.arch = checked(self.runner, "dpkg", &["--print-architecture"])?;
        let owned_web = self.record.packages.iter().any(|pkg| pkg == "tessera-web");
        let owned_dsp = self.record.packages.iter().any(|pkg| pkg == "tessera-dsp");
        let mut packages = Vec::new();
        for pkg in ["tessera-dsp", "tessera-web", "tessera-alsa", "tessera"] {
            if (pkg == "tessera-web" && !owned_web) || (pkg == "tessera-dsp" && !owned_dsp) {
                continue;
            }
            let status = self.package_field(pkg, "db:Status-Status")?;
            if status.as_deref() == Some("installed")
                || (self.options.purge && status.as_deref() == Some("config-files"))
            {
                packages.push(pkg.to_owned());
            }
        }
        let mut extras = Vec::new();
        if !packages.is_empty() && (!self.options.dry || self.paths.uid == 0) {
            let mut args = vec!["-s".into(), "remove".into()];
            args.extend(packages.clone());
            let reply = self.runner.command("apt-get", &args)?;
            ensure(reply.success, "apt removal simulation failed")?;
            for line in reply.stdout.lines() {
                let fields: Vec<_> = line.split_whitespace().collect();
                if fields.first() == Some(&"Remv") {
                    if let Some(pkg) = fields.get(1) {
                        let name = pkg.split(':').next().unwrap_or(pkg);
                        if !matches!(name, "tessera" | "tessera-alsa")
                            && !(name == "tessera-web" && owned_web)
                            && !(name == "tessera-dsp" && owned_dsp)
                        {
                            extras.push((*pkg).to_owned());
                        }
                    }
                }
            }
        }
        println!(
            "Plan: disable daemon; {} packages; remove recorded files; purge={} keep-users={}; extra removals: {}",
            if packages.is_empty() {
                "no".into()
            } else {
                packages.join(" ")
            },
            u8::from(self.options.purge),
            u8::from(self.options.keep),
            if extras.is_empty() {
                "none".into()
            } else {
                extras.join(" ")
            }
        );
        if self.options.dry {
            return Ok(());
        }
        ensure(
            extras.is_empty() || self.options.remove_dependents,
            "extra removals require --remove-dependents; --yes alone does not authorize them",
        )?;
        if !self.options.yes {
            ensure(self.runner.confirm("Remove bundle?")?, "removal declined")?;
        }
        self.started = found;
        if self.options.purge && found {
            self.record.purge_pending = true;
            self.record.keep_users = self.options.keep;
            self.save()?;
        }
        self.step = "service";
        if owned_dsp {
            // The GUI first: it talks to the engine.
            for unit in DSP_UNITS.into_iter().rev() {
                if command(
                    self.runner,
                    "systemctl",
                    &["cat", &format!("{unit}.service")],
                )?
                .success
                {
                    checked(self.runner, "systemctl", &["disable", "--now", unit])?;
                }
            }
        }
        if command(self.runner, "systemctl", &["cat", "tesserad.service"])?.success {
            checked(self.runner, "systemctl", &["disable", "--now", "tesserad"])?;
        }
        self.step = "packages";
        if !packages.is_empty() {
            let mut args = vec![
                if self.options.purge {
                    "purge".into()
                } else {
                    "remove".into()
                },
                "-y".into(),
            ];
            args.extend(packages);
            let reply = self.runner.command("apt-get", &args)?;
            ensure(
                reply.success,
                &format!("apt-get failed: {}", visible(&reply.stderr)),
            )?;
        }
        self.step = "files";
        for file in &self.record.files {
            // The seed configuration is the operator's data once the GUI has edited it: only
            // a purge removes it.
            if file != record::FILE && (file != record::SEED || self.options.purge) {
                remove_file(&self.paths.at(file))?;
            }
        }
        self.keep_aside_report();
        for directory in [
            "/etc/systemd/system/tesserad.service.d",
            "/etc/systemd/system/tessera-web.service.d",
            "/etc/tessera/web.d",
        ] {
            let _ = fs::remove_dir(self.paths.at(directory));
        }
        if self.live() && !self.options.purge && self.portal_stays()? {
            // A portal that stays installed must not keep polkit rules for the services
            // of a removed web.d.
            self.step = "polkit";
            let rules = command(self.runner, "tessera-web", &["appliance", "polkit"])?;
            if rules.success {
                polkit::write(&self.paths.root, &rules.stdout)?;
                checked(self.runner, "systemctl", &["daemon-reload"])?;
                let _ = command(self.runner, "systemctl", &["try-restart", "tessera-web"]);
            }
            self.step = "files";
        }
        if self.options.purge {
            // CamillaDSP's data: what this bundle created goes, and so does the state the
            // engine kept for it. Anything else in the data directory is the operator's
            // (an engine of their own, their configurations and filter files) and stays.
            if owned_dsp {
                self.purge_stage()?;
            }
            for directory in [
                "/etc/tessera",
                "/var/lib/tessera",
                "/var/lib/tessera-controller",
                "/var/lib/tessera-package",
                "/run/tessera",
            ] {
                remove_tree(&self.paths.at(directory))?;
            }
            // A separately installed portal retains its private journal, its
            // runtime and its saved volume. Also clean old-package residue when
            // no portal remains.
            if self
                .package_field("tessera-web", "db:Status-Status")?
                .is_none()
            {
                for directory in [
                    "/var/lib/tessera-network",
                    "/run/tessera-network",
                    "/var/lib/tessera-web",
                ] {
                    remove_tree(&self.paths.at(directory))?;
                }
            }
            if !self.options.keep {
                self.step = "memberships";
                for membership in self.record.users_modified.clone() {
                    let (user, group) = membership.split_once(':').expect("validated membership");
                    if command(self.runner, "getent", &["passwd", user])?.success
                        && command(self.runner, "getent", &["group", group])?.success
                        && self.groups(user)?.iter().any(|value| value == group)
                    {
                        checked(self.runner, "gpasswd", &["-d", user, group])?;
                    }
                }
                self.step = "accounts";
                for user in &self.record.users_created {
                    if command(self.runner, "getent", &["passwd", user])?.success {
                        checked(self.runner, "userdel", &[user])?;
                    }
                }
                for group in &self.record.groups_created {
                    if command(self.runner, "getent", &["group", group])?.success {
                        checked(self.runner, "groupdel", &[group])?;
                    }
                }
            }
            self.step = "dependencies";
            for dep in self.record.dependencies_installed.clone() {
                if self.package_field(&dep, "db:Status-Status")?.as_deref() == Some("installed") {
                    checked(self.runner, "apt-mark", &["auto", &dep])?;
                }
            }
        }
        self.step = "reload";
        checked(self.runner, "systemctl", &["daemon-reload"])?;
        if !self.options.purge && found {
            self.step = "record";
            self.record.complete = false;
            self.record.removed = true;
            // The seed configuration was kept (it is the operator's data now); a later purge
            // still has to know that the bundle created it.
            self.record.files.retain(|file| file == record::SEED);
            self.record
                .packages
                .retain(|pkg| pkg != "tessera-web" || owned_web);
            self.save()?;
        } else {
            self.step = "record-directory";
            let directory = self.paths.at(record::DIRECTORY);
            if directory.is_dir() {
                for entry in fs::read_dir(&directory)? {
                    let entry = entry?;
                    if entry.file_type()?.is_file()
                        && entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with("record.json.")
                    {
                        fs::remove_file(entry.path())?;
                    }
                }
                remove_file(&self.paths.at(record::FILE))?;
                fs::remove_dir(directory)?;
            }
        }
        self.step = "residue";
        self.residue();
        println!("uninstall: complete");
        Ok(())
    }

    /// After a purge: the state file that holds no choice of the operator, the directories
    /// that are empty now, and a list of what is left because the bundle did not create it.
    fn purge_stage(&mut self) -> Result<()> {
        let paths = self.paths;
        data::remove_idle_state(&paths.at(DSP_STATE), |config| paths.at(config).exists())?;
        for directory in [
            DSP_CONFIGS,
            DSP_COEFFS,
            "/opt/camilladsp",
            "/etc/camilladsp",
        ] {
            data::remove_empty_directory(&paths.at(directory));
        }
        let mut left = Vec::new();
        for directory in ["/opt/camilladsp", "/etc/camilladsp"] {
            Self::list_files(paths, directory, &mut left);
        }
        if !left.is_empty() {
            println!(
                "Left in place (the bundle did not create {}; remove by hand if unwanted):",
                if left.len() == 1 { "it" } else { "them" }
            );
            for path in left {
                println!("  {path}");
            }
        }
        Ok(())
    }

    /// The files and links below `directory`, as paths of the host, links not followed.
    fn list_files(paths: &Paths, directory: &str, found: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(paths.at(directory)) else {
            return;
        };
        let mut names: Vec<_> = entries
            .flatten()
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    entry.path(),
                )
            })
            .collect();
        names.sort();
        for (name, path) in names {
            let child = format!("{directory}/{name}");
            if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
                Self::list_files(paths, &child, found);
            } else {
                found.push(child);
            }
        }
    }

    /// Units of the operator's that the installer moved aside stay where they are; say how to
    /// get one back.
    fn keep_aside_report(&self) {
        for unit in DSP_UNITS {
            let file = format!("{UNIT_DIRECTORY}/{unit}.service.{ASIDE}");
            if fs::symlink_metadata(self.paths.at(&file)).is_ok() {
                println!(
                    "note: {file} is the {unit} unit this speaker had before the bundle; it is kept. To use it again: move it to {UNIT_DIRECTORY}/{unit}.service (replacing the link the package removal left), then systemctl daemon-reload and systemctl enable --now {unit}"
                );
            }
        }
    }

    /// The portal is installed and its rules directory exists, after this uninstall
    /// removed what it owned.
    fn portal_stays(&mut self) -> Result<bool> {
        Ok(self.paths.at("/etc/polkit-1/rules.d").is_dir()
            && self
                .package_field("tessera-web", "db:Status-Status")?
                .as_deref()
                == Some("installed"))
    }

    fn residue(&mut self) {
        println!("Residue (informational):");
        // find is kept for -xdev semantics. Its expression is fixed argv, and
        // the bundle directory is excluded just as in the original scan.
        let args = vec![
            self.paths.root.to_string_lossy().into_owned(),
            "-xdev".into(),
            "(".into(),
            "-path".into(),
            self.paths.at("/proc").to_string_lossy().into_owned(),
            "-o".into(),
            "-path".into(),
            self.paths.at("/sys").to_string_lossy().into_owned(),
            "-o".into(),
            "-path".into(),
            self.paths
                .at("/var/lib/apt/lists")
                .to_string_lossy()
                .into_owned(),
            "-o".into(),
            "-path".into(),
            self.paths.bundle.to_string_lossy().into_owned(),
            ")".into(),
            "-prune".into(),
            "-o".into(),
            "-iname".into(),
            "*tessera*".into(),
            "-print".into(),
        ];
        if let Ok(reply) = self.runner.command("find", &args) {
            print!("{}", visible(&reply.stdout));
        }
    }
}

/// What [`Lifecycle::shadowing_units`] will move: the unit files of the operator's in
/// `/etc/systemd/system` that the stage's units would otherwise be shadowed by, and the
/// drop-in directories beside them. The plan of a run, a dry run included, names them.
fn moves_aside(paths: &Paths) -> Vec<String> {
    let mut found = Vec::new();
    for unit in DSP_UNITS {
        let file = format!("{UNIT_DIRECTORY}/{unit}.service");
        if fs::symlink_metadata(paths.at(&file)).is_ok_and(|metadata| metadata.is_file()) {
            found.push(file.clone());
            if fs::symlink_metadata(paths.at(&format!("{file}.d")))
                .is_ok_and(|metadata| metadata.is_dir())
            {
                found.push(format!("{file}.d"));
            }
        }
    }
    found
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_tree(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return remove_file(path);
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

    fn paths(root: &Path) -> Paths {
        Paths {
            root: root.to_owned(),
            bundle: root.join("bundle"),
            os_release: root.join("os-release"),
            uid: 0,
        }
    }

    #[test]
    fn the_plan_names_the_units_of_the_operator_that_the_packaged_ones_replace() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let units = root.path().join("etc/systemd/system");
        fs::create_dir_all(&units).unwrap();
        assert!(moves_aside(&paths).is_empty());
        // A unit file, with its drop-in directory; a mask and a directory are not moved.
        fs::write(units.join("camilladsp.service"), "[Service]\n").unwrap();
        fs::create_dir(units.join("camilladsp.service.d")).unwrap();
        std::os::unix::fs::symlink("/dev/null", units.join("camillagui.service")).unwrap();
        assert_eq!(
            moves_aside(&paths),
            [
                "/etc/systemd/system/camilladsp.service",
                "/etc/systemd/system/camilladsp.service.d"
            ]
        );
        fs::remove_file(units.join("camillagui.service")).unwrap();
        fs::write(units.join("camillagui.service"), "[Service]\n").unwrap();
        assert_eq!(moves_aside(&paths).len(), 3);
    }
}
