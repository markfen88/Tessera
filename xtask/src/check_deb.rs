use crate::files::{TempDir, regular_files};
use crate::{Failure, Result, env_or, output};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Default)]
struct Checks {
    failed: bool,
    support: PathBuf,
}

impl Checks {
    fn check(&mut self, passed: bool, success: &str, failure: &str) {
        if passed {
            println!("  ok:   {success}");
        } else {
            println!("  FAIL: {failure}");
            self.failed = true;
        }
    }
}

fn field(deb: &Path, name: &str) -> Result<String> {
    let result = output(Command::new("dpkg-deb").arg("-f").arg(deb).arg(name))?;
    Ok(String::from_utf8_lossy(&result.stdout)
        .trim_end()
        .to_owned())
}

/// The files a Markdown text links to: targets that are neither a URL nor only an anchor, with
/// their anchors removed.
fn markdown_file_links(text: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("](") {
        let tail = &rest[at + 2..];
        let end = tail.find(')').unwrap_or(tail.len());
        let file = tail[..end].split('#').next().unwrap_or_default();
        if !file.is_empty() && !file.contains(':') {
            links.push(file.to_owned());
        }
        rest = &tail[end..];
    }
    links
}

/// The names in a Depends field, without versions and alternatives' other branches.
fn dependency_names(depends: &str) -> Vec<String> {
    depends
        .split(',')
        .filter_map(|dep| {
            let first = dep.split('|').next()?;
            first
                .split(['(', ' '])
                .find(|part| !part.is_empty())
                .map(str::to_owned)
        })
        .collect()
}

/// What the ELF objects a package ships link that the package does not bring itself, and the
/// glibc they need.
#[derive(Debug, Default)]
struct Linkage {
    /// The `NEEDED` sonames that no shared object shipped beside the program provides: the
    /// system's libraries, which the package must declare.
    needed: Vec<String>,
    /// The highest glibc symbol version any ELF object needs.
    glibc: Option<(u32, u32)>,
}

/// Every library an ELF object of the package links is shipped with it or provided by a package
/// it depends on, and the glibc it needs is the glibc it declares. A package whose programs
/// link a library must depend on the package a native build of the distribution names for it,
/// and no package may depend on a name that is only virtual on Debian 13 and Ubuntu 24.04. A
/// cross build reads the same table (`LIBRARY_PACKAGES`), so the two kinds of package agree;
/// the installer's check also honours `Provides`, which keeps bundles built before this guard
/// installable. A library that is neither shipped nor in the table is a failure: a new one
/// must be named there.
fn library_problems(depends: &str, linkage: &Linkage) -> Vec<String> {
    let names = dependency_names(depends);
    let mut problems = Vec::new();
    let system: BTreeSet<&String> = linkage.needed.iter().collect();
    for soname in system {
        match crate::build_deb::LIBRARY_PACKAGES
            .iter()
            .find(|(known, _, _)| known == soname)
        {
            Some((_, package, _)) if !names.iter().any(|name| name == package) => {
                problems.push(format!(
                    "links {soname} but does not depend on {package} (Depends: {depends})"
                ));
            }
            Some(_) => {}
            None => problems.push(format!(
                "links {soname}, which the package does not ship and no package of LIBRARY_PACKAGES provides: bundle it or name its package there"
            )),
        }
    }
    for (_, package, virtual_name) in crate::build_deb::LIBRARY_PACKAGES {
        if let Some(virtual_name) = virtual_name {
            if names.iter().any(|name| name == virtual_name) {
                problems.push(format!(
                    "depends on {virtual_name}, which is a virtual package on trixie and noble (a dpkg query does not see it); the package is {package}"
                ));
            }
        }
    }
    if let Some((major, minor)) = linkage.glibc {
        let declared = depends
            .split(',')
            .map(str::trim)
            .find(|entry| *entry == "libc6" || entry.starts_with("libc6 ("))
            .and_then(crate::build_deb::dependency_floor);
        if declared.is_none_or(|declared| declared < (major, minor)) {
            problems.push(format!(
                "an ELF object of the package needs glibc {major}.{minor} but Depends does not say libc6 (>= {major}.{minor}) (Depends: {depends})"
            ));
        }
    }
    problems
}

/// The directory whose shared objects an ELF object finds beside it: a program directory that
/// has an `_internal` directory (a PyInstaller bundle, whose bootloader looks there) for
/// everything inside it, else the directory the object is in. Nothing outside the extracted
/// package `root` counts.
fn bundle_root(root: &Path, elf: &Path) -> PathBuf {
    for ancestor in elf.ancestors().skip(1) {
        if ancestor == root || !ancestor.starts_with(root) {
            break;
        }
        if ancestor.join("_internal").is_dir() {
            return ancestor.to_owned();
        }
    }
    elf.parent().unwrap_or(root).to_owned()
}

/// The libraries the ELF objects among `files` (an extracted package at `root`) link that no
/// object beside them provides, and their glibc floor.
fn linkage(root: &Path, files: &[PathBuf]) -> Linkage {
    let elf = crate::build_deb::elf_files(files);
    // What each ELF object links, and what each directory of shared objects provides: by
    // soname and by file name.
    let mut provided: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    let mut links = Vec::new();
    for path in &elf {
        let tree = bundle_root(root, path);
        let provides = provided.entry(tree.clone()).or_default();
        if let Some(name) = path.file_name() {
            provides.insert(name.to_string_lossy().into_owned());
        }
        if let Ok(dynamic) = Command::new("readelf").arg("-d").arg(path).output() {
            let dynamic = String::from_utf8_lossy(&dynamic.stdout);
            provides.extend(crate::build_deb::sonames(&dynamic));
            links.push((tree, crate::build_deb::needed_sonames(&dynamic)));
        }
    }
    let mut linkage = Linkage::default();
    for (tree, needed) in links {
        linkage.needed.extend(
            needed
                .into_iter()
                .filter(|soname| !provided[&tree].contains(soname)),
        );
    }
    linkage.glibc = crate::build_deb::glibc_ceiling_of(&elf);
    linkage
}

/// Packages of one name built for several architectures declare the same dependency names:
/// a name differs only when a cross build guessed it.
fn architecture_problems(packages: &[(String, String, Vec<String>)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (index, (name, arch, names)) in packages.iter().enumerate() {
        for (other, other_arch, other_names) in &packages[index + 1..] {
            if name != other || arch == other_arch {
                continue;
            }
            let mut left: Vec<_> = names
                .iter()
                .filter(|dep| !dep.starts_with("tessera"))
                .collect();
            let mut right: Vec<_> = other_names
                .iter()
                .filter(|dep| !dep.starts_with("tessera"))
                .collect();
            left.sort();
            right.sort();
            if left != right {
                problems.push(format!(
                    "{name}: {arch} depends on {left:?} but {other_arch} on {right:?}"
                ));
            }
        }
    }
    problems
}

fn archive_problems(list: &str) -> Vec<&'static str> {
    let mut owners = false;
    let mut modes = false;
    let mut paths = false;
    for line in list.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 6 {
            owners = true;
            continue;
        }
        owners |= fields[1] != "root/root";
        let mode = fields[0].as_bytes();
        if mode.len() != 10 {
            modes = true;
        } else if mode[0] != b'l' {
            modes |= matches!(mode[3], b's' | b'S')
                || matches!(mode[6], b's' | b'S')
                || mode[8] == b'w'
                || matches!(mode[9], b't' | b'T');
        }
        paths |= ["./tmp/", "./home/", "./var/lib/", "./run/"]
            .iter()
            .any(|prefix| fields[5].starts_with(prefix));
    }
    let mut problems = Vec::new();
    if owners {
        problems.push("entries not owned by root/root");
    }
    if modes {
        problems.push("setuid/setgid/sticky or world-writable entry");
    }
    if paths {
        problems.push("package ships files under tmp/home/var/lib/run");
    }
    problems
}

fn executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn has_build_path(text: &str) -> bool {
    text.contains("/build/tessera/../")
        || text.match_indices("/home/").any(|(index, _)| {
            let rest = &text[index + 6..];
            rest.split_once('/').is_some_and(|(user, _)| {
                !user.is_empty() && user.bytes().all(|c| c.is_ascii_lowercase())
            })
        })
}

fn directory_policy(tmpfiles: &[String], units: &[String]) -> Vec<String> {
    let mut problems = Vec::new();
    for (path, mode, owner, group) in [
        ("/etc/tessera", "0755", "root", "root"),
        ("/etc/tessera/config.d", "0755", "root", "root"),
        ("/var/lib/tessera", "0750", "tessera", "tessera"),
        ("/var/lib/tessera-controller", "0755", "tessera", "tessera"),
        ("/run/tessera", "0755", "tessera", "tessera"),
        ("/var/lib/tessera-network", "0700", "root", "root"),
        ("/run/tessera-network", "0750", "root", "tessera-web"),
    ] {
        let declarations: Vec<_> = tmpfiles
            .iter()
            .flat_map(|text| text.lines())
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
            .filter(|fields| fields.get(1) == Some(&path))
            .collect();
        if declarations.len() != 1
            || declarations[0].get(..5) != Some(&["d", path, mode, owner, group][..])
        {
            problems.push(format!(
                "{path}: expected exactly one tmpfiles owner ({mode} {owner}:{group})"
            ));
        }
    }
    // polkitd ships and owns its rules directory (0750 root:polkitd): the rules file
    // is written into it, the directory is never declared here.
    for line in tmpfiles.iter().flat_map(|text| text.lines()) {
        if let Some(path) = line.split_whitespace().nth(1) {
            if Path::new(path).starts_with("/etc/polkit-1") {
                problems.push(format!(
                    "{path}: owned by polkitd, not declared by a tmpfiles line of ours"
                ));
            }
        }
    }
    for text in units {
        for line in text.lines().map(str::trim) {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let prefix = match key.trim() {
                "StateDirectory" => "/var/lib",
                "RuntimeDirectory" => "/run",
                "ConfigurationDirectory" => "/etc",
                _ => continue,
            };
            for directory in value.split_whitespace() {
                let directory = directory.split(':').next().unwrap_or_default();
                let path = Path::new(prefix).join(directory);
                let network = Path::new(prefix).join("tessera-network");
                let shared = Path::new(prefix).join("tessera");
                let controller = Path::new(prefix).join("tessera-controller");
                if network.starts_with(&path)
                    || path.starts_with(&network)
                    || shared.starts_with(&path)
                    || path.starts_with(&shared)
                    || controller.starts_with(&path)
                    || path.starts_with(&controller)
                {
                    problems.push(format!(
                        "{key}={directory} competes with network tmpfiles ownership"
                    ));
                }
            }
        }
    }
    problems
}

fn verify_network_directories(checks: &mut Checks) -> Result<()> {
    let root = checks.support.clone();
    if !root
        .join("usr/lib/systemd/system/tessera-network.service")
        .is_file()
    {
        return Ok(());
    }
    let tmpfiles = regular_files(&root.join("usr/lib/tmpfiles.d"))?
        .iter()
        .map(fs::read_to_string)
        .collect::<std::io::Result<Vec<_>>>()?;
    let units = regular_files(&root.join("usr/lib/systemd/system"))?
        .iter()
        .map(fs::read_to_string)
        .collect::<std::io::Result<Vec<_>>>()?;
    let problems = directory_policy(&tmpfiles, &units);
    for problem in &problems {
        println!("    {problem}");
    }
    checks.check(
        problems.is_empty(),
        "NE-6.2: sole tmpfiles ownership, root-private journal and web-accessible runtime",
        "network directory ownership conflicts with NE-6.2",
    );
    let helper = fs::read_to_string(root.join("usr/lib/systemd/system/tessera-network.service"))?;
    checks.check(
        helper.lines().any(|line| line == "User=root")
            && helper.lines().any(|line| line == "Group=tessera-web"),
        "helper runs as root with the portal socket group",
        "unexpected helper user/socket group",
    );
    Ok(())
}

/// The CamillaDSP units run only root-owned programs from the package's own tree and may
/// write only the data directory: a service account that can replace what it executes
/// would turn a flaw in a GUI into code running as that account at every start.
fn dsp_unit_problems(units: &[(&str, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, text) in units {
        for line in text.lines().map(str::trim) {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "ExecStart" => {
                    let program = value.trim_start_matches(['-', '@', '+', '!', ':']);
                    let program = program.split_whitespace().next().unwrap_or_default();
                    if !program.starts_with("/usr/lib/tessera-dsp/") {
                        problems.push(format!("{name}: ExecStart runs {program}"));
                    }
                }
                "WorkingDirectory" if !value.starts_with("/usr/lib/tessera-dsp/") => {
                    problems.push(format!("{name}: WorkingDirectory={value}"));
                }
                "ReadWritePaths" => {
                    for path in value.split_whitespace() {
                        if path.trim_start_matches('-') != "/opt/camilladsp" {
                            problems.push(format!("{name}: ReadWritePaths={path}"));
                        }
                    }
                }
                _ => {}
            }
        }
        if *name == "camilladsp.service" {
            problems.extend(engine_start_problems(text));
        }
    }
    problems
}

/// The engine follows the daemon in both directions. `PartOf=` carries a stop and a restart of
/// `tesserad.service` to it but starts nothing: after `stop` then `start` of the daemon (what
/// `tessera-network` does on every network change) the engine would stay stopped and the speaker
/// silent. `WantedBy=tesserad.service` makes enabling the engine link it into the daemon's
/// `.wants`, so every start of the daemon starts an engine the administrator has not switched off.
fn engine_start_problems(unit: &str) -> Vec<String> {
    let lists = |key: &str, wanted: &str| {
        unit.lines()
            .map(str::trim)
            .filter_map(|line| line.strip_prefix(key))
            .any(|value| value.split_whitespace().any(|word| word == wanted))
    };
    let mut problems = Vec::new();
    for (key, wanted) in [
        ("PartOf=", "tesserad.service"),
        ("WantedBy=", "tesserad.service"),
        ("WantedBy=", "multi-user.target"),
    ] {
        if !lists(key, wanted) {
            problems.push(format!("camilladsp.service: no {key}{wanted}"));
        }
    }
    problems
}

/// The portal's sound-card refresh: the udev rule must start the unit the package ships under
/// the name it uses, and the rule must parse.
fn verify_web_sound(checks: &mut Checks, root: &Path) -> Result<()> {
    let rules = root.join("usr/lib/udev/rules.d/60-tessera-web-sound.rules");
    let rule = fs::read_to_string(&rules).unwrap_or_default();
    checks.check(
        rule.contains("ENV{SYSTEMD_WANTS}+=\"tessera-web-sound.service\"")
            && root
                .join("usr/lib/systemd/system/tessera-web-sound.service")
                .is_file(),
        "the udev rule starts the shipped sound-card unit",
        "tessera-web: the sound-card udev rule or its unit is missing",
    );
    let result = Command::new("udevadm").arg("verify").arg(&rules).output()?;
    if !result.status.success() {
        println!(
            "    {}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
    checks.check(
        result.status.success(),
        "udevadm verify accepts the udev rule",
        "udevadm verify rejects the udev rule",
    );
    Ok(())
}

fn verify_dsp(checks: &mut Checks, root: &Path, conffiles: &str, temp: &Path) -> Result<()> {
    checks.check(
        executable(&root.join("usr/lib/tessera-dsp/camilladsp")),
        "/usr/lib/tessera-dsp/camilladsp is executable",
        "camilladsp binary missing or not executable",
    );
    checks.check(
        executable(&root.join("usr/lib/tessera-dsp/gui/camillagui_backend/camillagui_backend")),
        "the GUI backend is executable",
        "GUI backend missing or not executable",
    );
    for path in [
        "/etc/camilladsp/camillagui.yml",
        "/etc/camilladsp/gui-config.yml",
    ] {
        checks.check(
            conffiles.lines().any(|line| line == path),
            &format!("conffiles lists {path}"),
            &format!("conffiles does not list {path}"),
        );
    }
    for file in [
        "usr/share/doc/tessera-dsp/dsp.lock",
        "usr/share/doc/tessera-dsp/vendored.txt",
        "usr/share/doc/tessera-dsp/README.md",
        "usr/share/doc/tessera-dsp/examples/Speaker.yml.in",
        "usr/lib/systemd/system/camilladsp.service",
        "usr/lib/systemd/system/camillagui.service",
    ] {
        checks.check(
            root.join(file).is_file(),
            &format!("ships {file}"),
            &format!("missing {file}"),
        );
    }
    // The README is docs/dsp.md: its links to the other documents must find them where the
    // packages put them (the selected packages are all extracted under `support`).
    let readme = fs::read_to_string(root.join("usr/share/doc/tessera-dsp/README.md"))?;
    let missing: Vec<_> = markdown_file_links(&readme)
        .into_iter()
        .filter(|link| {
            !checks
                .support
                .join("usr/share/doc/tessera-dsp")
                .join(link)
                .is_file()
        })
        .collect();
    checks.check(
        missing.is_empty(),
        "the README's links to other documents resolve",
        &format!("README links to files no selected package ships: {missing:?}"),
    );
    checks.check(
        fs::read_to_string(root.join("usr/share/doc/tessera-dsp/copyright"))
            .is_ok_and(|text| text.contains("GPL-3.0")),
        "copyright names the GPL for the vendored programs",
        "copyright lacks the GPL notice",
    );
    let mut units = Vec::new();
    for name in ["camilladsp.service", "camillagui.service"] {
        if let Ok(text) = fs::read_to_string(root.join("usr/lib/systemd/system").join(name)) {
            units.push((name, text));
        }
    }
    let problems = dsp_unit_problems(&units);
    for problem in &problems {
        println!("    {problem}");
    }
    checks.check(
        problems.is_empty() && units.len() == 2,
        "the units run root-owned programs and write only /opt/camilladsp",
        "a CamillaDSP unit runs or writes outside its allowed trees",
    );
    let sysusers = root.join("usr/lib/sysusers.d/tessera-dsp.conf");
    if sysusers.is_file() {
        let sysroot = temp.join("sysroot-dsp");
        fs::create_dir_all(sysroot.join("etc"))?;
        fs::write(sysroot.join("etc/passwd"), "")?;
        fs::write(sysroot.join("etc/group"), "audio:x:29:\ntessera:x:900:\n")?;
        let result = Command::new("systemd-sysusers")
            .arg("--dry-run")
            .arg(format!("--root={}", sysroot.display()))
            .arg(sysusers)
            .output()?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        if !text.contains("Creating user 'camilla'") {
            println!("    {text}");
        }
        checks.check(
            text.contains("Creating user 'camilla'"),
            "sysusers.d creates the camilla account",
            "tessera-dsp sysusers.d",
        );
    } else {
        checks.check(false, "", "no tessera-dsp sysusers.d file");
    }
    Ok(())
}

fn verify_unit(checks: &mut Checks, unit: &Path, root: &Path) -> Result<()> {
    let mut command = Command::new("systemd-analyze");
    let user = unit.components().any(|part| part.as_os_str() == "user");
    if user {
        command.arg("--user");
    }
    let kind = if user { "user" } else { "system" };
    command.env(
        "SYSTEMD_UNIT_PATH",
        format!(
            "{}:",
            checks
                .support
                .join(format!("usr/lib/systemd/{kind}"))
                .display()
        ),
    );
    let result = command.arg("verify").arg(unit).output()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let mut complaints = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let packaged_command = line
            .split_once("Command ")
            .and_then(|(_, rest)| rest.split_once(" is not executable"))
            .map(|(binary, _)| binary);
        if packaged_command.is_some_and(|binary| {
            binary.starts_with('/')
                && (executable(&root.join(binary.trim_start_matches('/')))
                    || executable(&checks.support.join(binary.trim_start_matches('/'))))
        }) {
            continue;
        }
        complaints.push(line);
    }
    for complaint in &complaints {
        println!("    {complaint}");
    }
    checks.check(
        complaints.is_empty(),
        &format!("systemd-analyze verify {}", unit.display()),
        &format!("unit {}", unit.display()),
    );
    Ok(())
}

fn verify_man(checks: &mut Checks, path: &Path) -> Result<()> {
    let text = output(Command::new("gzip").arg("-cd").arg(path))?.stdout;
    let mut child = Command::new("groff")
        .args(["-man", "-ww", "-z"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().expect("piped stdin").write_all(&text)?;
    let result = child.wait_with_output()?;
    checks.check(
        result.stdout.is_empty() && result.stderr.is_empty(),
        &format!("man page {} renders", path.display()),
        &format!(
            "man page {}: {}",
            path.display(),
            String::from_utf8_lossy(&result.stderr)
        ),
    );
    Ok(())
}

fn package(checks: &mut Checks, deb: &Path) -> Result<()> {
    println!(
        "== {}",
        deb.file_name().unwrap_or_default().to_string_lossy()
    );
    let temp = TempDir::new("tessera-check-deb")?;
    let root = temp.path().join("root");
    let control = temp.path().join("ctl");
    output(Command::new("dpkg-deb").arg("-x").arg(deb).arg(&root))?;
    output(Command::new("dpkg-deb").arg("-e").arg(deb).arg(&control))?;
    let pkg = field(deb, "Package")?;
    for name in [
        "Package",
        "Version",
        "Architecture",
        "Maintainer",
        "Installed-Size",
        "Depends",
        "Section",
        "Priority",
        "Description",
    ] {
        checks.check(
            field(deb, name).is_ok_and(|value| !value.is_empty()),
            &format!("control field {name}"),
            &format!("control field {name} missing"),
        );
    }
    println!(
        "  ok:   control: {} {}, Depends: {}",
        field(deb, "Version")?,
        field(deb, "Architecture")?,
        field(deb, "Depends")?
    );
    checks.check(
        field(deb, "Maintainer")? == crate::build_deb::MAINTAINER,
        "maintainer",
        "maintainer placeholder",
    );
    let problems = library_problems(
        &field(deb, "Depends")?,
        &linkage(&root, &regular_files(&root)?),
    );
    checks.check(
        problems.is_empty(),
        "every library an ELF object links is shipped or declared, and its glibc floor is stated",
        &problems.join("; "),
    );
    // Keep the existing policy check without introducing a vendor identifier.
    let prohibited = [100_u8, 97, 110, 116, 101];
    let description = field(deb, "Description")?.to_ascii_lowercase();
    checks.check(
        !description
            .as_bytes()
            .windows(prohibited.len())
            .any(|word| word == prohibited.as_slice()),
        "control description",
        "control description names the vendor",
    );
    let listing = output(Command::new("dpkg-deb").arg("-c").arg(deb))?;
    let problems = archive_problems(&String::from_utf8_lossy(&listing.stdout));
    for (success, failure) in [
        ("all entries root/root", "entries not owned by root/root"),
        (
            "no setuid / world-writable entries",
            "setuid/setgid/sticky or world-writable entry",
        ),
        (
            "nothing under /tmp /home /var/lib /run",
            "package ships files under tmp/home/var/lib/run",
        ),
    ] {
        checks.check(!problems.contains(&failure), success, failure);
    }
    for binary in regular_files(&root.join("usr/bin"))? {
        let strings = output(Command::new("strings").arg(&binary))?;
        checks.check(
            !has_build_path(&String::from_utf8_lossy(&strings.stdout)),
            "binaries carry no /home paths",
            &format!("{} embeds a home directory", binary.display()),
        );
    }
    let md5sums = control.join("md5sums");
    let md5 = Command::new("md5sum")
        .current_dir(&root)
        .args(["--quiet", "-c"])
        .arg(&md5sums)
        .output()?;
    checks.check(md5.status.success(), "md5sums verify", "md5sums");
    let files = regular_files(&root)?;
    let sums = fs::read_to_string(md5sums)?.lines().count();
    checks.check(
        files.len() == sums,
        &format!("md5sums covers all {} files", files.len()),
        &format!("md5sums lists {sums} of {} files", files.len()),
    );
    for script in ["postinst", "prerm", "postrm", "preinst"] {
        let path = control.join(script);
        if !path.is_file() {
            continue;
        }
        checks.check(
            executable(&path),
            "maintainer mode",
            &format!("{script} not executable"),
        );
        checks.check(
            fs::read(&path)?.starts_with(b"\x7fELF"),
            "native Rust maintainer hook (no shell)",
            &format!("{script}: expected tessera-package ELF"),
        );
    }
    if matches!(pkg.as_str(), "tessera" | "tessera-web") {
        for (directory, name) in [
            ("usr/lib/sysusers.d", format!("{pkg}.conf")),
            ("usr/lib/tmpfiles.d", format!("{pkg}.conf")),
            ("usr/lib/systemd/system-preset", format!("80-{pkg}.preset")),
        ] {
            checks.check(
                root.join(directory).join(&name).is_file(),
                "declarative package integration",
                &format!("missing {directory}/{name}"),
            );
        }
        checks.check(
            root.join(format!(
                "usr/lib/systemd/system/{pkg}-package-migrate.service"
            ))
            .is_file(),
            "Rust migration oneshot unit",
            "missing package migration unit",
        );
        for hook in if pkg == "tessera" {
            &["preinst", "postinst", "prerm", "postrm"][..]
        } else {
            &["postinst", "prerm", "postrm"][..]
        } {
            checks.check(
                control.join(hook).is_file(),
                "native lifecycle hook present",
                &format!("missing {hook}"),
            );
        }
    }
    if pkg == "tessera" {
        checks.check(
            executable(&root.join("usr/bin/tessera-package")),
            "Rust one-shot migration command",
            "missing tessera-package",
        );
        let tmpfiles = fs::read_to_string(root.join("usr/lib/tmpfiles.d/tessera.conf"))?;
        checks.check(
            !tmpfiles.contains("90-probe-flows.toml"),
            "no legacy Controller file creation",
            "tmpfiles creates deprecated Controller file",
        );
    }
    let conffiles = fs::read_to_string(control.join("conffiles")).unwrap_or_default();
    for path in conffiles.lines() {
        checks.check(
            root.join(path.trim_start_matches('/')).is_file(),
            &format!("conffile {path} is in the package"),
            &format!("conffile {path} missing"),
        );
    }
    if pkg == "tessera" {
        checks.check(
            conffiles
                .lines()
                .any(|line| line == "/etc/tessera/config.toml"),
            "DEBIAN/conffiles lists /etc/tessera/config.toml",
            "DEBIAN/conffiles does not list /etc/tessera/config.toml",
        );
    }
    let config = root.join("etc/tessera/config.toml");
    if config.is_file() {
        let reference = fs::read(&config)?;
        checks.check(
            fs::read(root.join(format!("usr/share/doc/{pkg}/examples/config.toml")))
                .is_ok_and(|example| example == reference),
            "examples/config.toml equals the conffile /etc/tessera/config.toml",
            "examples/config.toml differs from /etc/tessera/config.toml",
        );
        checks.check(
            String::from_utf8_lossy(&reference)
                .lines()
                .all(|line| line.is_empty() || line.starts_with('#')),
            "/etc/tessera/config.toml is all comments (the reference)",
            "/etc/tessera/config.toml has an active (uncommented) line",
        );
    } else if pkg == "tessera" {
        checks.check(
            false,
            "",
            "the tessera package has no /etc/tessera/config.toml",
        );
    }
    if pkg == "tessera" {
        checks.check(
            root.join("usr/share/doc/tessera/NEWS.Debian.gz").is_file(),
            "NEWS.Debian.gz is present",
            "no NEWS.Debian.gz",
        );
    }
    if pkg == "tessera-dsp" {
        verify_dsp(checks, &root, &conffiles, temp.path())?;
    }
    if pkg == "tessera-web" {
        verify_web_sound(checks, &root)?;
    }
    for name in ["copyright", "changelog.Debian.gz"] {
        checks.check(
            root.join(format!("usr/share/doc/{pkg}/{name}")).is_file(),
            name,
            &format!("no {name}"),
        );
    }
    for path in files {
        if path.extension().is_some_and(|extension| extension == "gz") {
            let header = fs::read(&path)?;
            checks.check(
                header.get(4..8) == Some(&[0_u8; 4][..]),
                "gzip has no timestamp",
                &format!("{} has a timestamp in its gzip header", path.display()),
            );
            if path.starts_with(root.join("usr/share/man")) {
                verify_man(checks, &path)?;
            }
        }
        if path
            .extension()
            .is_some_and(|extension| extension == "service")
            && (path.starts_with(root.join("usr/lib/systemd/system"))
                || path.starts_with(root.join("usr/lib/systemd/user")))
        {
            verify_unit(checks, &path, &root)?;
        }
    }
    let sysusers = root.join("usr/lib/sysusers.d/tessera.conf");
    if sysusers.is_file() {
        let sysroot = temp.path().join("sysroot");
        fs::create_dir_all(sysroot.join("etc"))?;
        fs::write(sysroot.join("etc/passwd"), "")?;
        fs::write(sysroot.join("etc/group"), "")?;
        let result = Command::new("systemd-sysusers")
            .arg("--dry-run")
            .arg(format!("--root={}", sysroot.display()))
            .arg(sysusers)
            .output()?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        checks.check(
            text.contains("Creating user 'tessera'")
                && text.contains("Creating group 'tessera-admin'"),
            "sysusers.d creates user+group tessera and group tessera-admin",
            "sysusers.d",
        );
    }
    Ok(())
}

pub(crate) fn check(root: &Path, paths: &[PathBuf]) -> Result<()> {
    let mut checks = Checks::default();
    let mut paths = paths.to_vec();
    if paths.is_empty() {
        let target = env_or(
            "CARGO_TARGET_DIR",
            &root.join("target").display().to_string(),
        );
        let directory = Path::new(&target).join("deb");
        if directory.is_dir() {
            paths = fs::read_dir(directory)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<Vec<_>>>()?;
            paths.retain(|path| path.extension().is_some_and(|extension| extension == "deb"));
            paths.sort();
        }
        if paths.is_empty() {
            return Err(Failure::new("no Debian packages to check"));
        }
    }
    // Validate units against the complete selected package set, including
    // executables supplied by an exact-version dependency such as tessera.
    let support = TempDir::new("tessera-unit-dependencies")?;
    checks.support = support.path().to_owned();
    for deb in &paths {
        output(
            Command::new("dpkg-deb")
                .arg("-x")
                .arg(deb)
                .arg(&checks.support),
        )?;
    }
    verify_network_directories(&mut checks)?;
    if paths
        .iter()
        .any(|deb| field(deb, "Package").is_ok_and(|p| p == "tessera-pw"))
        && !Path::new("/usr/lib/systemd/user/pipewire.service").is_file()
    {
        // This is a syntax check on a build host, not a PipeWire lifecycle
        // test. Model its declared external dependency when it is absent.
        let unit = checks.support.join("usr/lib/systemd/user/pipewire.service");
        fs::create_dir_all(unit.parent().unwrap())?;
        fs::write(
            unit,
            "[Unit]\nDescription=PipeWire dependency for offline unit validation\n[Service]\nExecStart=/usr/bin/true\n",
        )?;
    }
    // lintian's answer to a NEWS entry dated after the build is a bare tag;
    // say what is wrong and how to fix it before it runs.
    match crate::news::check_dates(root) {
        Ok(()) => checks.check(
            true,
            "NEWS.Debian entries are not dated after the build",
            "",
        ),
        Err(error) => checks.check(false, "", &error.to_string()),
    }
    // Packages of one name for several architectures (a native and a cross build side by
    // side) declare the same dependency names.
    let mut declared = Vec::new();
    for deb in &paths {
        declared.push((
            field(deb, "Package")?,
            field(deb, "Architecture")?,
            dependency_names(&field(deb, "Depends")?),
        ));
    }
    let problems = architecture_problems(&declared);
    checks.check(
        problems.is_empty(),
        "packages of other architectures declare the same dependency names",
        &problems.join("; "),
    );
    for deb in paths {
        let lint = Command::new("lintian").arg(&deb).output()?;
        let report = String::from_utf8_lossy(&lint.stdout);
        print!("{report}");
        eprint!("{}", String::from_utf8_lossy(&lint.stderr));
        if report.contains("latest-changelog-entry-without-new-date") {
            println!(
                "  hint: the package changelog is dated with SOURCE_DATE_EPOCH (else the commit time) and a NEWS.Debian entry is dated later; date the entry no later than the release commit, or build with a later SOURCE_DATE_EPOCH"
            );
        }
        checks.check(lint.status.success(), "lintian", "lintian errors");
        if let Err(error) = package(&mut checks, &deb) {
            checks.check(false, "", &format!("{}: {error}", deb.display()));
        }
    }
    if checks.failed {
        println!("SOME CHECKS FAILED");
        Err(Failure::new(""))
    } else {
        println!("ALL CHECKS PASSED");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linkage_of(names: &[&str], glibc: Option<(u32, u32)>) -> Linkage {
        Linkage {
            needed: names.iter().map(|name| (*name).to_owned()).collect(),
            glibc,
        }
    }

    #[test]
    fn a_library_that_is_neither_shipped_nor_named_fails_and_glibc_must_be_declared() {
        // A library the table does not know, and one it does but the package does not declare.
        let problems = library_problems(
            "libc6 (>= 2.34), libgcc-s1",
            &linkage_of(&["libssl.so.3", "libc.so.6", "libstdc++.so.6"], None),
        );
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().any(
            |p| p.contains("links libssl.so.3") && p.contains("bundle it or name its package")
        ));
        assert!(
            problems
                .iter()
                .any(|p| p.contains("links libstdc++.so.6 but does not depend on libstdc++6"))
        );
        assert!(
            library_problems(
                "libc6 (>= 2.34), libstdc++6",
                &linkage_of(&["libstdc++.so.6"], None)
            )
            .is_empty()
        );
        // The libc family is one package; its version must cover the highest symbol.
        let libc = linkage_of(
            &["libc.so.6", "libm.so.6", "ld-linux-aarch64.so.1"],
            Some((2, 35)),
        );
        assert!(library_problems("libc6 (>= 2.35), libgcc-s1", &libc).is_empty());
        assert!(library_problems("libc6 (>= 2.40), libgcc-s1", &libc).is_empty());
        for depends in [
            "libc6 (>= 2.34), libgcc-s1",
            "libc6, libgcc-s1",
            "libgcc-s1",
        ] {
            let problems = library_problems(depends, &libc);
            assert!(
                problems.iter().any(|p| p.contains("glibc 2.35")),
                "{depends}: {problems:?}"
            );
        }
        // A package with no glibc symbols states no floor.
        assert!(library_problems("libgcc-s1", &linkage_of(&["libgcc_s.so.1"], None)).is_empty());
    }

    #[test]
    fn a_bundled_python_runtime_finds_its_libraries_beside_it() {
        let dir = std::env::temp_dir().join(format!("tessera-bundle-root-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let app = dir.join("usr/lib/tessera-dsp/gui/backend");
        fs::create_dir_all(app.join("_internal/lib-dynload")).unwrap();
        // Everything inside the program directory, its own executable included, shares one root.
        assert_eq!(
            bundle_root(&dir, &app.join("_internal/lib-dynload/_ssl.so")),
            app
        );
        assert_eq!(bundle_root(&dir, &app.join("_internal/libssl.so.3")), app);
        assert_eq!(bundle_root(&dir, &app.join("backend")), app);
        // A program beside it, in a directory without one, only has its own directory.
        let engine = dir.join("usr/lib/tessera-dsp/camilladsp");
        assert_eq!(bundle_root(&dir, &engine), dir.join("usr/lib/tessera-dsp"));
        // The package root is never a program directory, whatever is in it.
        fs::create_dir_all(dir.join("_internal")).unwrap();
        assert_eq!(
            bundle_root(&dir, &dir.join("usr/bin/tool")),
            dir.join("usr/bin")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_readmes_links_are_the_files_it_points_at() {
        assert_eq!(
            markdown_file_links(
                "[a](web-portal.md#x) [b](../tessera/troubleshooting.md) [c](https://e.org/a.md) [d](#top) [e](mailto:x@y)"
            ),
            ["web-portal.md", "../tessera/troubleshooting.md"]
        );
        assert!(markdown_file_links("").is_empty());
        // The shipped text of docs/dsp.md points only where packages put documents.
        let shipped =
            crate::build_deb::package_links(include_str!("../../docs/dsp.md"), "tessera-dsp");
        for link in markdown_file_links(&shipped) {
            let name = link.rsplit('/').next().unwrap().trim_end_matches(".md");
            assert!(crate::build_deb::doc_package(name).is_some(), "{link}");
            assert!(
                link == format!("{name}.md") || link.starts_with("../tessera"),
                "{link}"
            );
        }
    }

    #[test]
    fn a_package_depends_on_the_package_a_native_build_names_for_a_library() {
        // What dpkg-shlibdeps gives for tessera-alsa on trixie amd64.
        let native =
            "libasound2t64 (>= 1.0.16), libc6 (>= 2.34), libgcc-s1 (>= 4.2), tessera (= 0.3.0-6)";
        let linked = linkage_of(&["libasound.so.2", "libc.so.6"], None);
        assert!(library_problems(native, &linked).is_empty());
        // What the cross build declared before: a name apt resolves and dpkg does not.
        let cross = "libc6, libgcc-s1, libasound2, tessera (= 0.3.0-6)";
        let problems = library_problems(cross, &linked);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems[0].contains("does not depend on libasound2t64"),
            "{problems:?}"
        );
        assert!(problems[1].contains("virtual package"), "{problems:?}");
        // A package that links no ALSA needs no ALSA package, and may not name the virtual one.
        assert!(
            library_problems(
                "libc6 (>= 2.39), libgcc-s1",
                &linkage_of(&["libc.so.6"], None)
            )
            .is_empty()
        );
        assert_eq!(
            library_problems("libc6, libasound2", &Linkage::default()).len(),
            1
        );
        // Alternatives and spacing do not hide a name.
        assert_eq!(
            dependency_names("libc6 (>= 2.34),  libasound2t64|libasound2, systemd"),
            ["libc6", "libasound2t64", "systemd"]
        );
        assert!(dependency_names("").is_empty());
    }

    #[test]
    fn packages_of_one_name_for_two_architectures_declare_the_same_names() {
        let amd64 = dependency_names(
            "libasound2t64 (>= 1.0.16), libc6 (>= 2.39), libgcc-s1 (>= 4.2), tessera (= 1)",
        );
        let arm64 = dependency_names("libasound2t64, libc6, libgcc-s1, tessera (= 1)");
        let old = dependency_names("libc6, libgcc-s1, libasound2, tessera (= 1)");
        let package = |arch: &str, names: &[String]| {
            ("tessera-alsa".to_owned(), arch.to_owned(), names.to_vec())
        };
        assert!(
            architecture_problems(&[package("amd64", &amd64), package("arm64", &arm64)]).is_empty()
        );
        let problems = architecture_problems(&[package("amd64", &amd64), package("arm64", &old)]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("libasound2t64") && problems[0].contains("libasound2\""),
            "{problems:?}"
        );
        // Another package, or the same architecture twice, is not compared.
        let other = ("tessera".to_owned(), "arm64".to_owned(), old.clone());
        assert!(architecture_problems(&[package("amd64", &amd64), other]).is_empty());
        assert!(
            architecture_problems(&[package("amd64", &amd64), package("amd64", &old)]).is_empty()
        );
    }

    #[test]
    fn network_directory_policy_rejects_competing_or_unsafe_owners() {
        let core = "d /etc/tessera 0755 root root -\nd /etc/tessera/config.d 0755 root root -\nd /var/lib/tessera 0750 tessera tessera -\nd /var/lib/tessera-controller 0755 tessera tessera -\nd /run/tessera 0755 tessera tessera -\n".to_owned();
        let web = "d /var/lib/tessera-network 0700 root root -\nd /run/tessera-network 0750 root tessera-web -\n".to_owned();
        assert!(directory_policy(&[core.clone(), web.clone()], &[]).is_empty());
        for bad in [
            web.replace("0700 root root", "0700 root tessera-web"),
            web.replace("0700", "2770"),
            web.replace("0750", "0770"),
            format!("{web}d /etc/tessera 0755 root root -\n"),
            format!("{web}d /var/lib/tessera-network 0700 root root -\n"),
            String::new(),
        ] {
            assert!(!directory_policy(&[core.clone(), bad], &[]).is_empty());
        }
        // The directory of the polkit rules belongs to polkitd (guest verification F8).
        for polkit in [
            "d /etc/polkit-1/rules.d 0755 root root -\n",
            "d /etc/polkit-1 0755 root root -\n",
        ] {
            let bad = format!("{web}{polkit}");
            assert!(!directory_policy(&[core.clone(), bad], &[]).is_empty());
        }
        // What the packages ship declares none of it.
        let shipped = [
            include_str!("../../dist/tmpfiles.d/tessera.conf"),
            include_str!("../../dist/tmpfiles.d/tessera-web.conf"),
        ];
        let shipped: Vec<String> = shipped.iter().map(|text| (*text).to_owned()).collect();
        assert_eq!(directory_policy(&shipped, &[]), Vec::<String>::new());
        for unit in [
            "StateDirectory=tessera-network",
            "StateDirectory=tessera-network/child",
            "RuntimeDirectory=tessera-network",
            "StateDirectory=tessera",
            "StateDirectory=tessera-controller",
            "ConfigurationDirectory=tessera",
        ] {
            assert!(!directory_policy(&[core.clone(), web.clone()], &[unit.into()]).is_empty());
        }
    }

    #[test]
    fn archive_policy_catches_each_unsafe_attribute() {
        for (line, expected) in [
            (
                "-rwxr-xr-x root/root 10 2026-10-25 00:00 ./usr/bin/tool",
                None,
            ),
            (
                "-rwxr-xr-x user/root 10 2026-10-25 00:00 ./usr/bin/tool",
                Some("entries not owned by root/root"),
            ),
            (
                "-rwsr-xr-x root/root 10 2026-10-25 00:00 ./usr/bin/tool",
                Some("setuid/setgid/sticky or world-writable entry"),
            ),
            (
                "-rwxrwxrwx root/root 10 2026-10-25 00:00 ./usr/bin/tool",
                Some("setuid/setgid/sticky or world-writable entry"),
            ),
            (
                "-rw-r--r-- root/root 10 2026-10-25 00:00 ./var/lib/state",
                Some("package ships files under tmp/home/var/lib/run"),
            ),
            (
                "lrwxrwxrwx root/root 0 2026-10-25 00:00 ./etc/link -> /usr/share/file",
                None,
            ),
        ] {
            assert_eq!(
                archive_problems(line),
                expected.into_iter().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn dsp_units_keep_executables_out_of_the_writable_tree() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let read = |name: &str| {
            (
                name.to_owned(),
                fs::read_to_string(root.join("packaging/dsp/dist").join(name)).unwrap(),
            )
        };
        let units = [read("camilladsp.service"), read("camillagui.service")];
        let borrowed: Vec<_> = units.iter().map(|(n, t)| (n.as_str(), t.clone())).collect();
        assert_eq!(dsp_unit_problems(&borrowed), Vec::<String>::new());
        for bad in [
            "ExecStart=/opt/camilladsp/camilladsp -w",
            "ExecStart=-/usr/bin/camilladsp",
            "ExecStart=/usr/lib/tessera-dsp/camilladsp\nReadWritePaths=/usr/lib/tessera-dsp",
            "ExecStart=/usr/lib/tessera-dsp/camilladsp\nReadWritePaths=-/opt/camilladsp /etc/camilladsp",
            "WorkingDirectory=/opt/camilladsp",
        ] {
            assert!(
                !dsp_unit_problems(&[("x.service", bad.to_owned())]).is_empty(),
                "{bad}"
            );
        }
        assert!(
            dsp_unit_problems(&[(
                "x.service",
                "ExecStart=/usr/lib/tessera-dsp/camilladsp -w\nReadWritePaths=-/opt/camilladsp\n"
                    .into()
            )])
            .is_empty()
        );
    }

    #[test]
    fn the_engine_starts_with_the_daemon_and_stops_and_restarts_with_it() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let shipped =
            fs::read_to_string(root.join("packaging/dsp/dist/camilladsp.service")).unwrap();
        assert_eq!(engine_start_problems(&shipped), Vec::<String>::new());
        // Each relationship matters on its own: PartOf alone is the TV-3 defect N1 (the engine
        // stays stopped when the daemon is started again); without PartOf a restart of the
        // daemon leaves the engine without its capture device.
        for (from, to, missing) in [
            (
                "WantedBy=multi-user.target tesserad.service",
                "WantedBy=multi-user.target",
                "WantedBy=tesserad.service",
            ),
            (
                "WantedBy=multi-user.target tesserad.service",
                "WantedBy=tesserad.service",
                "WantedBy=multi-user.target",
            ),
            (
                "PartOf=tesserad.service",
                "Wants=tesserad.service",
                "PartOf=tesserad.service",
            ),
        ] {
            let unit = shipped.replace(from, to);
            let problems = engine_start_problems(&unit);
            assert!(
                problems.iter().any(|problem| problem.ends_with(missing)),
                "{from} -> {to}: {problems:?}"
            );
        }
        // The check is applied to the shipped unit by `check-deb` like the others.
        assert!(
            !dsp_unit_problems(&[("camilladsp.service", shipped.replace("PartOf=", "Wants="))])
                .is_empty()
        );
    }

    #[test]
    fn build_path_check_matches_the_existing_policy() {
        assert!(has_build_path("debug /home/builder/src/main.rs"));
        assert!(has_build_path("debug /build/tessera/../secret"));
        assert!(!has_build_path("debug /build/tessera/src/main.rs"));
    }
}
