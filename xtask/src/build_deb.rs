use crate::build_bundle::{copy, field, target_dir, text};
use crate::files::{TempDir, regular_files};
use crate::{Failure, Result, env_or, output, run};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) const MAINTAINER: &str = "Tessera maintainers <markfen88@users.noreply.github.com>";

/// The Debian package that provides each shared library a shipped program may link and that the
/// package does not bring itself, and the name the same library had before the 64-bit time
/// transition where that is only a virtual package (provided by the real one) on Debian 13 and
/// Ubuntu 24.04: `libasound.so.2` is in `libasound2t64`, `libasound2` is a name apt resolves and
/// `dpkg-query` does not see.
///
/// `dpkg-shlibdeps` derives the package on a native build. A cross build has no installed copy
/// of the library to ask and reads this table instead, and `check-deb` compares the Depends of
/// every package, whatever its architecture, with it. A library that is neither in this table
/// nor shipped inside the package fails `check-deb`: the table is where a new one is named.
pub(crate) const LIBRARY_PACKAGES: [(&str, &str, Option<&str>); 12] = [
    ("libasound.so.2", "libasound2t64", Some("libasound2")),
    ("libc.so.6", "libc6", None),
    ("libm.so.6", "libc6", None),
    ("libpthread.so.0", "libc6", None),
    ("libdl.so.2", "libc6", None),
    ("librt.so.1", "libc6", None),
    ("libutil.so.1", "libc6", None),
    ("libresolv.so.2", "libc6", None),
    ("ld-linux-x86-64.so.2", "libc6", None),
    ("ld-linux-aarch64.so.1", "libc6", None),
    ("libgcc_s.so.1", "libgcc-s1", None),
    ("libstdc++.so.6", "libstdc++6", None),
];

/// The highest `GLIBC_x.y` symbol version in `readelf --dyn-syms` output: the least glibc that
/// can run the program.
pub(crate) fn glibc_ceiling(dynamic_symbols: &str) -> Option<(u32, u32)> {
    dynamic_symbols
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
        .filter_map(|word| word.strip_prefix("GLIBC_"))
        .filter_map(|version| {
            let mut parts = version.split('.');
            Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
        })
        .max()
}

/// Whether a file is an ELF object (shared library or program).
pub(crate) fn is_elf(path: &Path) -> bool {
    use std::io::Read;
    let mut magic = [0_u8; 4];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok()
        && magic == *b"\x7fELF"
}

/// The ELF objects among `files`.
pub(crate) fn elf_files(files: &[PathBuf]) -> Vec<PathBuf> {
    files.iter().filter(|file| is_elf(file)).cloned().collect()
}

/// The glibc floor of the ELF files in `files`, by `readelf --dyn-syms`.
pub(crate) fn glibc_ceiling_of(files: &[PathBuf]) -> Option<(u32, u32)> {
    files
        .iter()
        .filter_map(|file| {
            let output = Command::new("readelf")
                .args(["--dyn-syms", "-W"])
                .arg(file)
                .output()
                .ok()?;
            glibc_ceiling(&String::from_utf8_lossy(&output.stdout))
        })
        .max()
}

/// `depends` with the `libc6` entry at least `libc6 (>= floor)`: `dpkg-shlibdeps` looks at the
/// programs it is given, and the vendored programs of a package are more than those.
pub(crate) fn libc_floor(depends: &str, floor: (u32, u32)) -> String {
    let wanted = format!("libc6 (>= {}.{})", floor.0, floor.1);
    let mut found = false;
    let mut entries: Vec<String> = depends
        .split(',')
        .map(|entry| {
            let entry = entry.trim();
            if entry != "libc6" && !entry.starts_with("libc6 (") {
                return entry.to_owned();
            }
            found = true;
            match dependency_floor(entry) {
                Some(have) if have >= floor => entry.to_owned(),
                _ => wanted.clone(),
            }
        })
        .collect();
    if !found {
        entries.push(wanted);
    }
    entries.join(", ")
}

/// The `(>= major.minor)` of one Depends entry, ignoring a Debian revision.
pub(crate) fn dependency_floor(entry: &str) -> Option<(u32, u32)> {
    let version = entry.split_once("(>=")?.1.split(')').next()?.trim();
    let mut parts = version.split(['.', '-', '~', '+']);
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// The package that ships a documentation file of `docs/` (by name, without `.md`), if any does.
/// The links between these files are relative in the repository; an installed copy finds a
/// sibling in its own directory and any other under `/usr/share/doc/<package>`.
pub(crate) fn doc_package(name: &str) -> Option<&'static str> {
    match name {
        "user-guide" | "rt-tuning" | "troubleshooting" | "configuration" | "network-vlans" => {
            Some("tessera")
        }
        "web-portal" => Some("tessera-web"),
        "dsp" | "raspberry-pi-speaker" => Some("tessera-dsp"),
        _ => None,
    }
}

/// `text`, a file of `docs/`, as it ships in `package`: a link to another file of `docs/` that
/// ships becomes the file's place relative to this one (`name.md` beside it, else
/// `../<package>/name.md`); any other link stays as it is.
pub(crate) fn package_links(text: &str, package: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("](") {
        let (head, tail) = rest.split_at(at + 2);
        out.push_str(head);
        let end = tail.find(')').unwrap_or(tail.len());
        let target = &tail[..end];
        let (file, anchor) = target
            .split_once('#')
            .map_or((target, ""), |(file, _)| (file, &target[file.len()..]));
        let shipped = file
            .strip_suffix(".md")
            .filter(|name| !name.contains('/'))
            .and_then(|name| doc_package(name).map(|owner| (name, owner)));
        match shipped {
            Some((name, owner)) if owner == package => {
                out.push_str(&format!("{name}.md{anchor}"));
            }
            Some((name, owner)) => out.push_str(&format!("../{owner}/{name}.md{anchor}")),
            None => out.push_str(target),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// The bracketed values of the `readelf -d` lines of one tag.
fn dynamic_entries(readelf: &str, tag: &str) -> Vec<String> {
    readelf
        .lines()
        .filter(|line| line.contains(tag))
        .filter_map(|line| {
            let start = line.find('[')? + 1;
            Some(line[start..line.rfind(']')?].to_owned())
        })
        .collect()
}

/// The `NEEDED` sonames of `readelf -d` output.
pub(crate) fn needed_sonames(readelf: &str) -> Vec<String> {
    dynamic_entries(readelf, "(NEEDED)")
}

/// The `SONAME` of `readelf -d` output: what the object is called by those that link it.
pub(crate) fn sonames(readelf: &str) -> Vec<String> {
    dynamic_entries(readelf, "(SONAME)")
}

/// The library part of a cross-built package's Depends: what `dpkg-shlibdeps` names on a native
/// build. The minimum versions are not known without the installed library; the one that is,
/// the glibc floor of the programs, is stated.
pub(crate) fn cross_libraries(needed: &[String], glibc: Option<(u32, u32)>) -> String {
    let mut names = BTreeSet::from(["libc6", "libgcc-s1"]);
    for (soname, package, _) in LIBRARY_PACKAGES {
        if needed.iter().any(|name| name == soname) {
            names.insert(package);
        }
    }
    names
        .into_iter()
        .map(|name| match glibc {
            Some((major, minor)) if name == "libc6" => format!("libc6 (>= {major}.{minor})"),
            _ => name.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    text(Command::new("git").current_dir(root).args(args))
}
fn git_ok(root: &Path, args: &[&str]) -> Result<bool> {
    Ok(Command::new("git")
        .current_dir(root)
        .args(args)
        .output()?
        .status
        .success())
}

pub(crate) fn version(root: &Path, revision: Option<&str>, release: bool) -> Result<String> {
    let manifest = fs::read_to_string(root.join("Cargo.toml"))?;
    let mut workspace = false;
    let mut upstream = None;
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            workspace = line == "[workspace.package]";
        } else if workspace {
            if let Some(("version", value)) = line
                .split_once('=')
                .map(|(key, value)| (key.trim(), value.trim()))
            {
                upstream = Some(value.trim_matches('"').to_owned());
            }
        }
    }
    let upstream = upstream
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Failure::new("cannot read the workspace version from Cargo.toml"))?;
    let have_main = git_ok(root, &["remote", "get-url", "origin"])?
        && git_ok(
            root,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                "refs/remotes/origin/main",
            ],
        )?;
    if !have_main && revision.is_none() {
        return Err(Failure {
            code: 2,
            message: "no origin/main to compare: set TESSERA_DEB_REVISION explicitly".into(),
        });
    }
    let on_main = have_main
        && git_ok(
            root,
            &["merge-base", "--is-ancestor", "HEAD", "origin/main"],
        )?;
    if release && !on_main {
        return Err(Failure::new(
            "TESSERA_RELEASE=1 requires HEAD to be an ancestor of origin/main",
        ));
    }
    let mut revision = if let Some(value) = revision {
        value.to_owned()
    } else {
        if git(root, &["rev-parse", "--is-shallow-repository"])? == "true" {
            return Err(Failure::new(
                "shallow checkout: fetch full history or set TESSERA_DEB_REVISION",
            ));
        }
        let anchor = if on_main {
            "HEAD".into()
        } else {
            git(root, &["merge-base", "HEAD", "origin/main"])?
        };
        git(root, &["rev-list", "--count", "--first-parent", &anchor])?
    };
    if !on_main {
        if let Ok(sha) = git(root, &["rev-parse", "--short=7", "HEAD"]) {
            if !sha.is_empty() {
                revision.push_str(&format!("~g{sha}"));
            }
        }
    }
    let result = format!("{}-{revision}", upstream.replacen('-', "~", 1));
    if !tessera_bundle::manifest::token(&result) {
        return Err(Failure::new("invalid Debian version"));
    }
    Ok(result)
}

#[derive(Debug)]
struct Build<'a> {
    root: &'a Path,
    target: PathBuf,
    rel: PathBuf,
    target_flags: Vec<String>,
    triple: String,
    native: bool,
    arch: &'static str,
    multiarch: &'static str,
    version: String,
    epoch: String,
    upstream: String,
    runtime_build_id: String,
}

pub(crate) fn build(root: &Path, print_version: bool) -> Result<()> {
    let revision = env_or("TESSERA_DEB_REVISION", "");
    let version = version(
        root,
        (!revision.is_empty()).then_some(revision.as_str()),
        env_or("TESSERA_RELEASE", "0") == "1",
    )?;
    if print_version {
        println!("{version}");
        return Ok(());
    }
    let rustc = text(Command::new("rustc").arg("-vV"))?;
    let host = rustc
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .ok_or_else(|| Failure::new("rustc did not report its host"))?;
    let triple = env_or("TARGET", host);
    let native = triple == host;
    let (arch, multiarch) = match triple.as_str() {
        "x86_64-unknown-linux-gnu" => ("amd64", "x86_64-linux-gnu"),
        "aarch64-unknown-linux-gnu" => ("arm64", "aarch64-linux-gnu"),
        "armv7-unknown-linux-gnueabihf" => ("armhf", "arm-linux-gnueabihf"),
        _ => {
            return Err(Failure::new(format!(
                "unsupported target {triple} (amd64, arm64 and armhf Linux/glibc only)"
            )));
        }
    };
    let target = target_dir(root);
    let flags = if native && env_or("FORCE_TARGET_FLAG", "").is_empty() {
        Vec::new()
    } else {
        vec!["--target".into(), triple.clone()]
    };
    let rel = if flags.is_empty() {
        target.join("release")
    } else {
        target.join(&triple).join("release")
    };
    let default_epoch =
        git(root, &["log", "-1", "--format=%ct"]).unwrap_or(text(Command::new("date").arg("+%s"))?);
    let epoch = env_or("SOURCE_DATE_EPOCH", &default_epoch);
    if epoch.parse::<u64>().is_err() {
        return Err(Failure::new("invalid SOURCE_DATE_EPOCH"));
    }
    let upstream = version
        .split('-')
        .next()
        .unwrap_or(&version)
        .replace('~', "-");
    let build = Build {
        root,
        target,
        rel,
        target_flags: flags,
        triple,
        native,
        arch,
        multiarch,
        version,
        epoch,
        upstream,
        runtime_build_id: crate::runtime_build::source_id(root)?,
    };
    build.binaries()?;
    build.packages()
}

impl Build<'_> {
    fn cargo(&self) -> Result<Command> {
        let mut command = Command::new("cargo");
        command
            .current_dir(self.root)
            .env("TESSERA_RUNTIME_BUILD_ID", &self.runtime_build_id)
            .env("SOURCE_DATE_EPOCH", &self.epoch);
        let sysroot = text(Command::new("rustc").args(["--print", "sysroot"]))?;
        let cargo_home = env_or("CARGO_HOME", &format!("{}/.cargo", env_or("HOME", "")));
        let flags = format!(
            "{} --remap-path-prefix={}=/build/tessera --remap-path-prefix={cargo_home}=/build/cargo --remap-path-prefix={sysroot}=/build/rustc -C strip=symbols",
            env_or("RUSTFLAGS", ""),
            self.root.display()
        );
        command.env("RUSTFLAGS", flags);
        Ok(command)
    }
    fn binaries(&self) -> Result<()> {
        if env_or("NO_BUILD", "0") == "1" {
            return Ok(());
        }
        eprintln!(
            "build-deb: building tesserad, tesseractl and tessera-web ({}, release, --locked)",
            self.triple
        );
        let mut command = self.cargo()?;
        command
            .args(["build", "--release", "--locked"])
            .args(&self.target_flags)
            .args([
                "-p",
                "tesserad",
                "-p",
                "tesseractl",
                "-p",
                "tessera-web",
                "-p",
                "tessera-network",
                "-p",
                "tessera-package",
            ]);
        let features = env_or("TESSERA_DAEMON_FEATURES", "").replace(' ', "");
        if !features.is_empty() {
            let features = features
                .split(',')
                .map(|feature| {
                    if feature.starts_with("tesserad/") {
                        feature.into()
                    } else {
                        format!("tesserad/{feature}")
                    }
                })
                .collect::<Vec<String>>()
                .join(",");
            command.args(["--features", &features]);
        }
        run(&mut command)?;
        // Preserve the existing external compatibility build command without
        // inspecting or deriving implementation from its historical location.
        run(self
            .cargo()?
            .args([
                "build",
                "--release",
                "--locked",
                "--manifest-path",
                "lab/cmc-probe/Cargo.toml",
            ])
            .arg("--target-dir")
            .arg(self.target.join("controller"))
            .args(&self.target_flags))?;
        if env_or("SKIP_PW", "0") != "1" {
            run(self
                .cargo()?
                .args(["build", "--release", "--locked"])
                .args(&self.target_flags)
                .args(["-p", "tessera-backend-pipewire", "--features", "pw"]))?;
        }
        if env_or("SKIP_ALSA", "0") != "1" {
            run(self
                .cargo()?
                .args(["build", "--release", "--locked"])
                .args(&self.target_flags)
                .args([
                    "-p",
                    "tessera-backend-alsa",
                    "--no-default-features",
                    "--features",
                    "plugin",
                ]))?;
            fs::copy(
                self.rel.join("libtessera_backend_alsa.so"),
                self.rel.join("libasound_module_pcm_tessera.so"),
            )?;
        }
        let reference = output(self.cargo()?.args([
            "run",
            "--release",
            "--locked",
            "-q",
            "-p",
            "tessera-state",
            "--example",
            "reference_config",
        ]))?
        .stdout;
        if reference != fs::read(self.root.join("dist/etc/tessera/config.toml"))? {
            return Err(Failure::new(
                "dist/etc/tessera/config.toml is stale (it differs from the schema). Regenerate it: cargo run -q -p tessera-state --example reference_config > dist/etc/tessera/config.toml",
            ));
        }
        Ok(())
    }
    fn put(&self, stage: &Path, source: &str, destination: &str, executable: bool) -> Result<()> {
        copy(
            &self.root.join(source),
            &stage.join(destination),
            executable,
        )
    }
    /// A file of `docs/` as it ships in `package`, with its links to the other files of
    /// `docs/` pointed at where those ship.
    fn put_doc(&self, stage: &Path, doc: &str, destination: &str, package: &str) -> Result<()> {
        let text = fs::read_to_string(self.root.join(format!("docs/{doc}.md")))?;
        let destination = stage.join(destination);
        fs::create_dir_all(
            destination
                .parent()
                .ok_or_else(|| Failure::new("missing destination parent"))?,
        )?;
        fs::write(&destination, package_links(&text, package))?;
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o644))?;
        Ok(())
    }
    fn binary(&self, stage: &Path, name: &str) -> Result<()> {
        copy(
            &self.rel.join(name),
            &stage.join("usr/bin").join(name),
            true,
        )
    }
    fn packages(&self) -> Result<()> {
        let out = self.target.join("deb");
        let stage = out.join("stage");
        if stage.exists() {
            fs::remove_dir_all(&stage)?;
        }
        fs::create_dir_all(&stage)?;
        for entry in fs::read_dir(&out)? {
            let path = entry?.path();
            if path.extension().is_some_and(|extension| extension == "deb")
                || path.file_name().is_some_and(|name| name == "SHA256SUMS")
            {
                fs::remove_file(path)?;
            }
        }
        self.core(&stage.join("tessera"))?;
        if env_or("SKIP_PW", "0") != "1" && self.rel.join("tessera-pw").is_file() {
            self.pw(&stage.join("tessera-pw"))?;
        } else {
            eprintln!(
                "build-deb: note: tessera-pw not packaged (SKIP_PW=1 or no binary at {})",
                self.rel.display()
            );
        }
        let alsa = [
            self.rel.join("libasound_module_pcm_tessera.so"),
            self.rel.join("deps/libasound_module_pcm_tessera.so"),
        ]
        .into_iter()
        .find(|path| path.is_file());
        let mut have_alsa = false;
        if env_or("SKIP_ALSA", "0") != "1" {
            if let Some(alsa) = alsa {
                self.alsa(&stage.join("tessera-alsa"), &alsa)?;
                have_alsa = true;
            } else {
                eprintln!(
                    "build-deb: note: tessera-alsa not packaged: libasound_module_pcm_tessera.so not found under {}",
                    self.rel.display()
                );
            }
        }
        self.web(&stage.join("tessera-web"))?;
        // CamillaDSP rides along on amd64 and arm64: it reads the ALSA plugin's PCM.
        if env_or("SKIP_DSP", "0") != "1" && matches!(self.arch, "amd64" | "arm64") && have_alsa {
            let fetched = crate::dsp::fetch(self.root, self.arch)?;
            self.dsp(&stage.join("tessera-dsp"), &fetched)?;
        } else {
            eprintln!(
                "build-deb: note: tessera-dsp not packaged (SKIP_DSP=1, unsupported architecture, or no ALSA plugin)"
            );
        }
        let mut sums = String::new();
        let mut debs = fs::read_dir(&out)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        debs.sort();
        for deb in debs
            .into_iter()
            .filter(|path| path.extension().is_some_and(|ext| ext == "deb"))
        {
            let digest = text(Command::new("sha256sum").arg(&deb))?;
            sums.push_str(&format!(
                "{}  {}\n",
                digest.split_whitespace().next().unwrap_or(""),
                deb.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
        fs::write(out.join("SHA256SUMS"), sums)?;
        eprintln!("build-deb: done:");
        Ok(())
    }
    fn core(&self, stage: &Path) -> Result<()> {
        for name in [
            "tesserad",
            "tesseractl",
            "tessera-controller-start",
            "tessera-controller-command",
            "tessera-package",
        ] {
            self.binary(stage, name)?;
        }
        let controller = if self.target_flags.is_empty() {
            self.target.join("controller/release")
        } else {
            self.target
                .join("controller")
                .join(&self.triple)
                .join("release")
        };
        copy(
            &controller.join("cmc-probe"),
            &stage.join("usr/lib/tessera/cmc-probe"),
            true,
        )?;
        fs::create_dir_all(stage.join("usr/lib/tessera/controller-bin"))?;
        symlink(
            "../../../bin/tessera-controller-command",
            stage.join("usr/lib/tessera/controller-bin/sudo"),
        )?;
        for (source, destination) in [
            ("dist/controller.args", "usr/lib/tessera/controller.args"),
            (
                "dist/systemd/tessera-controller@.service",
                "usr/lib/systemd/system/tessera-controller@.service",
            ),
            (
                "dist/systemd/tesserad.service",
                "usr/lib/systemd/system/tesserad.service",
            ),
            (
                "dist/systemd/tessera-package-migrate.service",
                "usr/lib/systemd/system/tessera-package-migrate.service",
            ),
            (
                "crates/tessera-network/dist/tesseraaudio.slice",
                "usr/lib/systemd/system/tesseraaudio.slice",
            ),
            (
                "dist/systemd/system-preset/80-tessera.preset",
                "usr/lib/systemd/system-preset/80-tessera.preset",
            ),
            (
                "dist/sysusers.d/tessera.conf",
                "usr/lib/sysusers.d/tessera.conf",
            ),
            (
                "dist/tmpfiles.d/tessera.conf",
                "usr/lib/tmpfiles.d/tessera.conf",
            ),
            (
                "dist/sysctl.d/tessera.conf",
                "usr/lib/sysctl.d/60-tessera.conf",
            ),
            ("dist/etc/tessera/config.toml", "etc/tessera/config.toml"),
            (
                "dist/etc/tessera/config.toml",
                "usr/share/doc/tessera/examples/config.toml",
            ),
            (
                "docs/integration-contract.json",
                "usr/share/doc/tessera/integration-contract.json",
            ),
        ] {
            self.put(stage, source, destination, false)?;
        }
        fs::create_dir_all(stage.join("etc/tessera/config.d"))?;
        for doc in [
            "user-guide",
            "rt-tuning",
            "troubleshooting",
            "configuration",
            "network-vlans",
        ] {
            if self.root.join(format!("docs/{doc}.md")).is_file() {
                self.put_doc(
                    stage,
                    doc,
                    &format!("usr/share/doc/tessera/{doc}.md"),
                    "tessera",
                )?;
            }
        }
        if self.native {
            for (shell, path) in [
                ("bash", "usr/share/bash-completion/completions/tesseractl"),
                ("zsh", "usr/share/zsh/vendor-completions/_tesseractl"),
                (
                    "fish",
                    "usr/share/fish/vendor_completions.d/tesseractl.fish",
                ),
            ] {
                let destination = stage.join(path);
                fs::create_dir_all(destination.parent().unwrap())?;
                fs::write(
                    destination,
                    output(Command::new(self.rel.join("tesseractl")).args(["completions", shell]))?
                        .stdout,
                )?;
            }
            self.man(stage, "tesseractl", "1", "control the tessera daemon", true)?;
            self.man(
                stage,
                "tesserad",
                "8",
                "tessera audio-over-IP daemon",
                false,
            )?;
        } else {
            eprintln!(
                "build-deb: cross build: completions and man pages need a runnable binary; skipped"
            );
        }
        fs::create_dir_all(stage.join("DEBIAN"))?;
        fs::write(stage.join("DEBIAN/conffiles"), "/etc/tessera/config.toml\n")?;
        self.hooks(stage, &["preinst", "postinst", "prerm", "postrm"])?;
        crate::news::write_news(
            stage,
            "tessera",
            &self.root.join("packaging/NEWS.Debian"),
            &self.version,
        )?;
        let shlibs = self.shlibs(&[
            stage.join("usr/bin/tesserad"),
            stage.join("usr/bin/tesseractl"),
            stage.join("usr/bin/tessera-controller-start"),
            stage.join("usr/bin/tessera-package"),
            stage.join("usr/lib/tessera/cmc-probe"),
        ])?;
        self.finish(
            stage,
            "tessera",
            &format!(
                "{shlibs}, avahi-daemon, systemd (>= {}), init-system-helpers (>= 1.62)",
                env_or("SYSTEMD_MIN", "253")
            ),
            Some(("Suggests", "tessera-pw, tessera-alsa, rt-tests, linuxptp")),
        )
    }
    fn hooks(&self, stage: &Path, names: &[&str]) -> Result<()> {
        for name in names {
            copy(
                &self.rel.join("tessera-package"),
                &stage.join("DEBIAN").join(name),
                true,
            )?;
        }
        Ok(())
    }
    fn pw(&self, stage: &Path) -> Result<()> {
        self.binary(stage, "tessera-pw")?;
        self.put(
            stage,
            "crates/tessera-backend-pipewire/dist/tessera-pw.service",
            "usr/lib/systemd/user/tessera-pw.service",
            false,
        )?;
        if self.native {
            self.man(
                stage,
                "tessera-pw",
                "1",
                "PipeWire client of tesserad",
                false,
            )?;
        }
        let shlibs = self.shlibs(&[stage.join("usr/bin/tessera-pw")])?;
        self.finish(
            stage,
            "tessera-pw",
            &format!("{shlibs}, tessera (= {}), pipewire", self.version),
            Some(("Recommends", "wireplumber")),
        )
    }
    fn alsa(&self, stage: &Path, source: &Path) -> Result<()> {
        let plugin = stage.join(format!(
            "usr/lib/{}/alsa-lib/libasound_module_pcm_tessera.so",
            self.multiarch
        ));
        copy(source, &plugin, false)?;
        let _ = Command::new("strip")
            .arg("--strip-unneeded")
            .arg(&plugin)
            .output();
        let mut snippets = fs::read_dir(
            self.root
                .join("crates/tessera-backend-alsa/dist/alsa.conf.d"),
        )?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
        snippets.retain(|path| path.extension().is_some_and(|ext| ext == "conf"));
        snippets.sort();
        let snippet = stage.join("usr/share/alsa/alsa.conf.d/60-tessera.conf");
        fs::create_dir_all(snippet.parent().unwrap())?;
        if let Some(source) = snippets.first() {
            copy(source, &snippet, false)?;
        } else {
            fs::write(
                &snippet,
                "# tessera ALSA plugin: provides the PCM \"tessera\".\npcm.tessera {\n    type tessera\n}\n",
            )?;
        }
        fs::create_dir_all(stage.join("etc/alsa/conf.d"))?;
        symlink(
            "/usr/share/alsa/alsa.conf.d/60-tessera.conf",
            stage.join("etc/alsa/conf.d/60-tessera.conf"),
        )?;
        let shlibs = self.shlibs(&[plugin])?;
        self.finish(
            stage,
            "tessera-alsa",
            &format!("{shlibs}, tessera (= {})", self.version),
            None,
        )
    }
    fn web(&self, stage: &Path) -> Result<()> {
        for name in ["tessera-web", "tessera-network"] {
            self.binary(stage, name)?;
        }
        for name in [
            "tessera-network.service",
            "tessera-network-recovery.service",
            "tessera-network-recovery.timer",
        ] {
            self.put(
                stage,
                &format!("crates/tessera-network/dist/{name}"),
                &format!("usr/lib/systemd/system/{name}"),
                false,
            )?;
        }
        for (source, destination) in [
            (
                "crates/tessera-network/dist/network-recovery.conf",
                "usr/lib/systemd/system/tesserad.service.d/standalone-network.conf",
            ),
            (
                "crates/tessera-web/dist/tessera-web.service",
                "usr/lib/systemd/system/tessera-web.service",
            ),
            (
                "dist/systemd/tessera-web-package-migrate.service",
                "usr/lib/systemd/system/tessera-web-package-migrate.service",
            ),
            (
                "crates/tessera-web/dist/tessera-web-unitfile@.service",
                "usr/lib/systemd/system/tessera-web-unitfile@.service",
            ),
            (
                "crates/tessera-web/dist/tessera-web-sound.service",
                "usr/lib/systemd/system/tessera-web-sound.service",
            ),
            (
                "crates/tessera-web/dist/60-tessera-web-sound.rules",
                "usr/lib/udev/rules.d/60-tessera-web-sound.rules",
            ),
            (
                "crates/tessera-web/dist/tessera-web.sysusers.conf",
                "usr/lib/sysusers.d/tessera-web.conf",
            ),
            (
                "dist/tmpfiles.d/tessera-web.conf",
                "usr/lib/tmpfiles.d/tessera-web.conf",
            ),
            (
                "dist/systemd/system-preset/80-tessera-web.preset",
                "usr/lib/systemd/system-preset/80-tessera-web.preset",
            ),
            ("crates/tessera-web/dist/web.toml", "etc/tessera/web.toml"),
            (
                "crates/tessera-web/dist/web.toml",
                "usr/share/doc/tessera-web/examples/web.toml",
            ),
            (
                "crates/tessera-web/dist/appliance-paths.conf",
                "usr/share/doc/tessera-web/examples/appliance-paths.conf",
            ),
        ] {
            self.put(stage, source, destination, false)?;
        }
        self.put_doc(
            stage,
            "web-portal",
            "usr/share/doc/tessera-web/web-portal.md",
            "tessera-web",
        )?;
        if self.native {
            self.man(
                stage,
                "tessera-web",
                "8",
                "web portal of the tessera daemon",
                false,
            )?;
        }
        fs::create_dir_all(stage.join("DEBIAN"))?;
        fs::write(stage.join("DEBIAN/conffiles"), "/etc/tessera/web.toml\n")?;
        self.hooks(stage, &["postinst", "prerm", "postrm"])?;
        let shlibs = self.shlibs(&[
            stage.join("usr/bin/tessera-web"),
            stage.join("usr/bin/tessera-network"),
        ])?;
        self.finish(stage, "tessera-web", &format!("{shlibs}, tessera (= {}), polkitd, systemd (>= {}), init-system-helpers (>= 1.62), iproute2, nftables, iputils-arping", self.version, env_or("SYSTEMD_MIN", "253")), None)
    }
    /// CamillaDSP and its web GUI: the DSP stage of a tessera speaker
    /// (tesserad -> pcm.tessera -> CamillaDSP -> DAC). Both are the official
    /// upstream release binaries pinned in `packaging/dsp/dsp.lock` and shipped
    /// unmodified, in a root-owned tree; only `/opt/camilladsp` (created by
    /// tmpfiles) is writable by the service account.
    fn dsp(&self, stage: &Path, fetched: &crate::dsp::Fetched) -> Result<()> {
        let library = stage.join("usr/lib/tessera-dsp");
        copy(
            &fetched.camilladsp_binary(),
            &library.join("camilladsp"),
            true,
        )?;
        crate::dsp::copy_program_tree(
            &fetched.gui_directory(),
            &library.join("gui/camillagui_backend"),
        )?;
        for (source, destination) in [
            (
                "packaging/dsp/dist/camilladsp.service",
                "usr/lib/systemd/system/camilladsp.service",
            ),
            (
                "packaging/dsp/dist/camillagui.service",
                "usr/lib/systemd/system/camillagui.service",
            ),
            (
                "packaging/dsp/dist/tessera-dsp.sysusers.conf",
                "usr/lib/sysusers.d/tessera-dsp.conf",
            ),
            (
                "packaging/dsp/dist/tessera-dsp.tmpfiles.conf",
                "usr/lib/tmpfiles.d/tessera-dsp.conf",
            ),
            (
                "packaging/dsp/dist/camillagui.yml",
                "etc/camilladsp/camillagui.yml",
            ),
            (
                "packaging/dsp/dist/gui-config.yml",
                "etc/camilladsp/gui-config.yml",
            ),
            (
                "packaging/dsp/dist/Speaker.yml.in",
                "usr/share/doc/tessera-dsp/examples/Speaker.yml.in",
            ),
            (
                "packaging/dsp/dsp.lock",
                "usr/share/doc/tessera-dsp/dsp.lock",
            ),
        ] {
            self.put(stage, source, destination, false)?;
        }
        self.put_doc(
            stage,
            "dsp",
            "usr/share/doc/tessera-dsp/README.md",
            "tessera-dsp",
        )?;
        self.put_doc(
            stage,
            "raspberry-pi-speaker",
            "usr/share/doc/tessera-dsp/raspberry-pi-speaker.md",
            "tessera-dsp",
        )?;
        fs::write(
            stage.join("usr/share/doc/tessera-dsp/vendored.txt"),
            format!(
                "Vendored, unmodified upstream binaries (checksums pinned in dsp.lock):\n  CamillaDSP {}  https://github.com/HEnquist/camilladsp  (GPL-3.0-or-later)\n  CamillaGUI backend {}  https://github.com/HEnquist/camillagui-backend  (GPL-3.0-or-later,\n  with the bundled Python runtime and libraries it was released with)\nCorresponding source: the repositories above, at tags v{} and v{}.\n",
                fetched.camilladsp, fetched.camillagui, fetched.camilladsp, fetched.camillagui
            ),
        )?;
        fs::create_dir_all(stage.join("DEBIAN"))?;
        fs::write(
            stage.join("DEBIAN/conffiles"),
            "/etc/camilladsp/camillagui.yml\n/etc/camilladsp/gui-config.yml\n",
        )?;
        self.hooks(stage, &["postinst", "prerm", "postrm"])?;
        let shlibs = self.shlibs(&[library.join("camilladsp")])?;
        // The GUI is a Python runtime that brings its libraries (OpenSSL, zlib, libffi, readline
        // and the rest sit in its `_internal` directory), so only glibc is the system's; its
        // floor is higher than CamillaDSP's, which is all `dpkg-shlibdeps` was asked about.
        let mut programs = vec![library.join("camilladsp")];
        programs.extend(elf_files(&regular_files(&library.join("gui"))?));
        let shlibs = match glibc_ceiling_of(&programs) {
            Some(floor) => libc_floor(&shlibs, floor),
            None => shlibs,
        };
        self.finish(
            stage,
            "tessera-dsp",
            &format!(
                "{shlibs}, tessera (= {version}), tessera-alsa (= {version}), systemd (>= {}), init-system-helpers (>= 1.62), alsa-utils",
                env_or("SYSTEMD_MIN", "253"),
                version = self.version
            ),
            Some(("Recommends", "tessera-web")),
        )
    }
    fn shlibs(&self, binaries: &[PathBuf]) -> Result<String> {
        if !self.native {
            let mut needed = Vec::new();
            for binary in binaries {
                if let Ok(output) = Command::new("readelf").arg("-d").arg(binary).output() {
                    needed.extend(needed_sonames(&String::from_utf8_lossy(&output.stdout)));
                }
            }
            return Ok(cross_libraries(&needed, glibc_ceiling_of(binaries)));
        }
        let work = TempDir::new("tessera-shlibdeps")?;
        fs::create_dir(work.path().join("debian"))?;
        fs::write(work.path().join("debian/control"), "")?;
        let mut command = Command::new("dpkg-shlibdeps");
        command.current_dir(work.path()).arg("-O");
        for binary in binaries {
            command.arg(format!("-e{}", binary.display()));
        }
        let dependencies = text(&mut command)?;
        dependencies
            .lines()
            .find_map(|line| line.strip_prefix("shlibs:Depends="))
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| Failure::new("dpkg-shlibdeps found no dependencies"))
    }
    fn man(
        &self,
        stage: &Path,
        name: &str,
        section: &str,
        summary: &str,
        subcommands: bool,
    ) -> Result<()> {
        let binary = self.rel.join(name);
        let help = text(Command::new(&binary).arg("--help"))?;
        let roff = |text: &str| {
            text.lines()
                .map(|line| {
                    let line = line.replace('\\', "\\e").replace('-', "\\-");
                    if line.starts_with(['.', '\'']) {
                        format!("\\&{line}\n")
                    } else {
                        format!("{line}\n")
                    }
                })
                .collect::<String>()
        };
        let mut page = format!(
            ".TH {} {section} \"\" \"tessera {}\" \"tessera\"\n.SH NAME\n{name} \\- {summary}\n.SH DESCRIPTION\nThis page is generated from the program's own help text.\nThe user guide (/usr/share/doc/tessera/user\\-guide.md) has the long form.\n.SH OPTIONS AND USAGE\n.nf\n{}.fi\n",
            name.to_ascii_uppercase(),
            self.upstream,
            roff(&help)
        );
        if subcommands {
            let mut commands = false;
            for line in help.lines() {
                if line == "Commands:" {
                    commands = true;
                    continue;
                }
                if line.is_empty() {
                    commands = false;
                }
                if commands {
                    if let Some(sub) = line.split_whitespace().next().filter(|sub| *sub != "help") {
                        let help = text(Command::new(&binary).args([sub, "--help"]))?;
                        page.push_str(&format!(
                            ".SH COMMAND: {}\n.nf\n{}.fi\n",
                            sub.to_ascii_uppercase(),
                            roff(&help)
                        ));
                    }
                }
            }
        }
        page.push_str(".SH SEE ALSO\ntesserad(8), tesseractl(1), tessera\\-pw(1)\n");
        gzip(
            &page,
            &stage.join(format!("usr/share/man/man{section}/{name}.{section}.gz")),
        )
    }
    fn finish(
        &self,
        stage: &Path,
        package: &str,
        depends: &str,
        optional: Option<(&str, &str)>,
    ) -> Result<()> {
        let doc = stage.join("usr/share/doc").join(package);
        fs::create_dir_all(&doc)?;
        // The vendored programs carry their own licence notice.
        let copyright = if package == "tessera-dsp" {
            "packaging/dsp/copyright"
        } else {
            "packaging/copyright"
        };
        self.put(
            stage,
            copyright,
            &format!("usr/share/doc/{package}/copyright"),
            false,
        )?;
        let overrides = self
            .root
            .join(format!("packaging/lintian-overrides/{package}"));
        if overrides.is_file() {
            copy(
                &overrides,
                &stage.join(format!("usr/share/lintian/overrides/{package}")),
                false,
            )?;
        }
        for name in match package {
            "tessera" => &[
                "tessera-controller-start",
                "tessera-controller-command",
                "tessera-package",
            ][..],
            "tessera-web" => &["tessera-network"][..],
            _ => &[],
        } {
            gzip(
                &fs::read_to_string(self.root.join(format!("packaging/man/{name}.8")))?,
                &stage.join(format!("usr/share/man/man8/{name}.8.gz")),
            )?;
        }
        let commit = git(self.root, &["rev-parse", "--short=12", "HEAD"])
            .unwrap_or_else(|_| "unknown".into());
        let date = text(Command::new("date").env("LC_ALL", "C").args([
            "-u",
            "-R",
            "-d",
            &format!("@{}", self.epoch),
        ]))?;
        let mut changelog = format!(
            "{package} ({}) unstable; urgency=medium\n\n  * Built from commit {commit} by cargo xtask build-deb.\n\n -- {MAINTAINER}  {date}\n\n",
            self.version
        );
        if package == "tessera" {
            changelog.push_str(&crate::news::changelog_history(
                &self.root.join("packaging/NEWS.Debian"),
                package,
                &self.version,
                MAINTAINER,
            )?);
        }
        gzip(&changelog, &doc.join("changelog.Debian.gz"))?;
        fs::create_dir_all(stage.join("DEBIAN"))?;
        let size = text(
            Command::new("du")
                .args(["-sk", "--apparent-size", "--exclude=DEBIAN"])
                .arg(stage),
        )?
        .split_whitespace()
        .next()
        .unwrap_or("0")
        .to_owned();
        let description = fs::read_to_string(
            self.root
                .join(format!("packaging/descriptions/{package}.txt")),
        )?;
        let extra = optional
            .map(|(key, value)| format!("{key}: {value}\n"))
            .unwrap_or_default();
        fs::write(
            stage.join("DEBIAN/control"),
            format!(
                "Package: {package}\nVersion: {}\nArchitecture: {}\nMaintainer: {MAINTAINER}\nInstalled-Size: {size}\nDepends: {depends}\n{extra}Section: sound\nPriority: optional\n{description}",
                self.version, self.arch
            ),
        )?;
        let mut sums = String::new();
        for file in regular_files(stage)? {
            let relative = file
                .strip_prefix(stage)
                .map_err(|error| Failure::new(error.to_string()))?;
            if relative.starts_with("DEBIAN") {
                continue;
            }
            let digest = text(
                Command::new("md5sum")
                    .current_dir(stage)
                    .arg("--")
                    .arg(relative),
            )?;
            sums.push_str(&digest);
            sums.push('\n');
        }
        fs::write(stage.join("DEBIAN/md5sums"), sums)?;
        normalize(stage, &self.epoch)?;
        let destination = self
            .target
            .join("deb")
            .join(format!("{package}_{}_{}.deb", self.version, self.arch));
        run(Command::new("dpkg-deb")
            .env("SOURCE_DATE_EPOCH", &self.epoch)
            .args(["--root-owner-group", "-Zxz", "-z6", "--build"])
            .arg(stage)
            .arg(&destination))?;
        if field(&destination, "Version")? != self.version {
            return Err(Failure::new("assembled package version mismatch"));
        }
        eprintln!(
            "build-deb: built {}",
            destination
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        );
        Ok(())
    }
}

fn gzip(text: &str, path: &Path) -> Result<()> {
    fs::create_dir_all(
        path.parent()
            .ok_or_else(|| Failure::new("missing gzip parent"))?,
    )?;
    let mut child = Command::new("gzip")
        .arg("-9n")
        .stdin(Stdio::piped())
        .stdout(File::create(path)?)
        .spawn()?;
    child
        .stdin
        .take()
        .expect("piped gzip stdin")
        .write_all(text.as_bytes())?;
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(Failure::new("gzip failed"))
    }
}

fn normalize(directory: &Path, epoch: &str) -> Result<()> {
    fn walk(path: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.is_dir() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
            for entry in fs::read_dir(path)? {
                walk(&entry?.path(), paths)?;
            }
        } else if metadata.is_file() && metadata.permissions().mode() & 0o100 == 0 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o644))?;
        }
        paths.push(path.into());
        Ok(())
    }
    let mut paths = Vec::new();
    walk(directory, &mut paths)?;
    for chunk in paths.chunks(100) {
        run(Command::new("touch")
            .args(["-h", "-d", &format!("@{epoch}"), "--"])
            .args(chunk))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_between_the_documents_follow_the_files_to_the_packages_that_ship_them() {
        let dsp = "([web-portal.md](web-portal.md#appliance-mode)) and [the speaker](raspberry-pi-speaker.md), [fixes](troubleshooting.md#network-speaker), [x](https://example.org/a.md#b), [adr](adr/0015.md), [self](dsp.md)";
        assert_eq!(
            package_links(dsp, "tessera-dsp"),
            "([web-portal.md](../tessera-web/web-portal.md#appliance-mode)) and [the speaker](raspberry-pi-speaker.md), [fixes](../tessera/troubleshooting.md#network-speaker), [x](https://example.org/a.md#b), [adr](adr/0015.md), [self](dsp.md)"
        );
        // From the portal's manual, the speaker guides are in the DSP package.
        assert_eq!(
            package_links("[dsp](dsp.md) [net](network-vlans.md)", "tessera-web"),
            "[dsp](../tessera-dsp/dsp.md) [net](../tessera/network-vlans.md)"
        );
        assert_eq!(package_links("no links ](", "tessera"), "no links ](");
        assert_eq!(package_links("", "tessera"), "");
        // Every document the repository links to from the three that are rewritten here is either
        // shipped or left as it is; none is guessed.
        assert_eq!(doc_package("unknown"), None);
    }

    #[test]
    fn a_cross_build_names_the_packages_a_native_build_of_the_distribution_names() {
        let dynamic = "Dynamic section at offset 0x1 contains 27 entries:\n  Tag        Type                         Name/Value\n 0x0000000000000001 (NEEDED)             Shared library: [libasound.so.2]\n 0x0000000000000001 (NEEDED)             Shared library: [libgcc_s.so.1]\n 0x0000000000000001 (NEEDED)             Shared library: [libc.so.6]\n 0x000000000000000e (SONAME)             Library soname: [libasound_module_pcm_tessera.so]\n";
        let needed = needed_sonames(dynamic);
        assert_eq!(needed, ["libasound.so.2", "libgcc_s.so.1", "libc.so.6"]);
        // The names dpkg-shlibdeps gives on trixie amd64 for the same program (versions aside):
        // libasound2t64, libc6, libgcc-s1, in that order. libasound2 is not among them.
        assert_eq!(
            cross_libraries(&needed, None),
            "libasound2t64, libc6, libgcc-s1"
        );
        assert_eq!(cross_libraries(&[], None), "libc6, libgcc-s1");
        assert_eq!(
            cross_libraries(&["libc.so.6".to_owned()], None),
            "libc6, libgcc-s1"
        );
        // The glibc floor of the programs is stated, as a native build states it.
        assert_eq!(
            cross_libraries(&needed, Some((2, 34))),
            "libasound2t64, libc6 (>= 2.34), libgcc-s1"
        );
        // A program that links a library the table names gets that library's package.
        assert_eq!(
            cross_libraries(&["libstdc++.so.6".to_owned()], None),
            "libc6, libgcc-s1, libstdc++6"
        );
        assert!(needed_sonames("").is_empty());
    }

    #[test]
    fn the_glibc_floor_is_the_highest_symbol_version_and_raises_the_dependency() {
        let symbols = "     1: 0000000000000000     0 FUNC    GLOBAL DEFAULT  UND memcpy@GLIBC_2.14 (3)\n     2: 0000000000000000     0 FUNC    GLOBAL DEFAULT  UND pthread_cond_clockwait@GLIBC_2.34 (4)\n     3: 0000000000000000     0 FUNC    GLOBAL DEFAULT  UND __libc_start_main@GLIBC_2.2.5 (2)\n     4: 0000000000000000     0 FUNC    GLOBAL DEFAULT  UND x@GLIBC_PRIVATE (5)\n     5: 0000000000000000     0 FUNC    GLOBAL DEFAULT  UND y@@GLIBC_2.35 (6)\n";
        assert_eq!(glibc_ceiling(symbols), Some((2, 35)));
        assert_eq!(glibc_ceiling("puts@GLIBC_2.2.5"), Some((2, 2)));
        assert_eq!(glibc_ceiling("x@GLIBC_PRIVATE\nno symbols"), None);
        assert_eq!(glibc_ceiling(""), None);
        // A floor that is lower is raised, one that is high enough is kept, a missing entry added.
        let depends = "libasound2t64 (>= 1.0.18), libc6 (>= 2.34), libgcc-s1 (>= 4.2)";
        assert_eq!(
            libc_floor(depends, (2, 35)),
            "libasound2t64 (>= 1.0.18), libc6 (>= 2.35), libgcc-s1 (>= 4.2)"
        );
        assert_eq!(libc_floor(depends, (2, 34)), depends);
        assert_eq!(libc_floor(depends, (2, 12)), depends);
        assert_eq!(
            libc_floor("libc6, libgcc-s1", (2, 35)),
            "libc6 (>= 2.35), libgcc-s1"
        );
        assert_eq!(
            libc_floor("libgcc-s1", (2, 35)),
            "libgcc-s1, libc6 (>= 2.35)"
        );
        // Another package whose name starts alike is not libc6.
        assert_eq!(
            libc_floor("libc6-dev (>= 1), libc6 (>= 2.1)", (2, 3)),
            "libc6-dev (>= 1), libc6 (>= 2.3)"
        );
        assert_eq!(dependency_floor("libc6 (>= 2.39-1)"), Some((2, 39)));
        assert_eq!(dependency_floor("libc6"), None);
    }

    fn commit(root: &Path, message: &str) {
        run(Command::new("git").current_dir(root).args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@invalid",
            "commit",
            "--allow-empty",
            "-qm",
            message,
        ]))
        .unwrap();
    }
    #[test]
    fn versions_preserve_preview_first_parent_and_source_archive_rules() {
        let work = TempDir::new("tessera-version-test").unwrap();
        let repo = work.path().join("repo");
        fs::create_dir(&repo).unwrap();
        fs::write(
            repo.join("Cargo.toml"),
            "[workspace.package]\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        run(Command::new("git")
            .current_dir(&repo)
            .args(["init", "-qb", "main"]))
        .unwrap();
        run(Command::new("git").current_dir(&repo).args(["add", "."])).unwrap();
        commit(&repo, "base");
        run(Command::new("git")
            .current_dir(&repo)
            .args(["remote", "add", "origin"])
            .arg(&repo))
        .unwrap();
        run(Command::new("git").current_dir(&repo).args([
            "update-ref",
            "refs/remotes/origin/main",
            "HEAD",
        ]))
        .unwrap();
        assert_eq!(version(&repo, None, true).unwrap(), "0.1.0-1");
        run(Command::new("git")
            .current_dir(&repo)
            .args(["checkout", "-qb", "feature"]))
        .unwrap();
        commit(&repo, "one");
        commit(&repo, "two");
        let sha = git(&repo, &["rev-parse", "--short=7", "HEAD"]).unwrap();
        let preview = version(&repo, None, false).unwrap();
        assert_eq!(preview, format!("0.1.0-1~g{sha}"));
        assert!(version(&repo, None, true).is_err());
        run(Command::new("dpkg").args(["--compare-versions", &preview, "lt", "0.1.0-1"])).unwrap();
        run(Command::new("git")
            .current_dir(&repo)
            .args(["checkout", "-q", "main"]))
        .unwrap();
        run(Command::new("git")
            .current_dir(&repo)
            .args(["merge", "--squash", "feature"]))
        .unwrap();
        commit(&repo, "squash");
        run(Command::new("git").current_dir(&repo).args([
            "update-ref",
            "refs/remotes/origin/main",
            "HEAD",
        ]))
        .unwrap();
        assert_eq!(version(&repo, None, true).unwrap(), "0.1.0-2");
        let shallow = work.path().join("shallow");
        run(Command::new("git")
            .args(["clone", "-q", "--depth", "1"])
            .arg(format!("file://{}", repo.display()))
            .arg(&shallow))
        .unwrap();
        assert!(
            version(&shallow, None, false)
                .unwrap_err()
                .message
                .contains("shallow")
        );
        assert_eq!(version(&shallow, Some("7"), false).unwrap(), "0.1.0-7");
        let source = work.path().join("tarball");
        fs::create_dir(&source).unwrap();
        fs::write(
            source.join("Cargo.toml"),
            "[workspace.package]\nversion = \"0.2.0-beta\"\n",
        )
        .unwrap();
        assert_eq!(version(&source, None, false).unwrap_err().code, 2);
        assert_eq!(version(&source, Some("9"), false).unwrap(), "0.2.0~beta-9");
        run(Command::new("git")
            .current_dir(&repo)
            .args(["remote", "remove", "origin"]))
        .unwrap();
        assert_eq!(version(&repo, None, false).unwrap_err().code, 2);
    }
}
