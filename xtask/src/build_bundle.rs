use crate::files::{TempDir, regular_files};
use crate::{Failure, Result, env_or, output, run};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use tessera_bundle::Native;
use tessera_bundle::manifest::{Deb, Manifest, Platform, dependency, hash};

fn bundle<T>(result: tessera_bundle::Result<T>) -> Result<T> {
    result.map_err(|error| Failure::new(error.to_string()))
}

pub(crate) fn text(command: &mut Command) -> Result<String> {
    Ok(String::from_utf8_lossy(&output(command)?.stdout)
        .trim_end()
        .into())
}

pub(crate) fn target_dir(root: &Path) -> PathBuf {
    let value = PathBuf::from(env_or(
        "CARGO_TARGET_DIR",
        &root.join("target").display().to_string(),
    ));
    if value.is_absolute() {
        value
    } else {
        root.join(value)
    }
}

pub(crate) fn field(deb: &Path, name: &str) -> Result<String> {
    text(Command::new("dpkg-deb").arg("-f").arg(deb).arg(name))
}

pub(crate) fn copy(source: &Path, destination: &Path, executable: bool) -> Result<()> {
    fs::create_dir_all(
        destination
            .parent()
            .ok_or_else(|| Failure::new("missing destination parent"))?,
    )?;
    fs::copy(source, destination)?;
    fs::set_permissions(
        destination,
        fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
    )?;
    Ok(())
}

pub(crate) fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
        } else if entry.file_type()?.is_file() {
            copy(&entry.path(), &destination.join(entry.file_name()), false)?;
        } else {
            return Err(Failure::new("unexpected symlink in bundle sources"));
        }
    }
    Ok(())
}

fn combined_dependencies(debs: &[PathBuf]) -> Result<Vec<String>> {
    let mut selected: BTreeMap<String, String> = BTreeMap::new();
    let mut dependencies = vec!["avahi-daemon".into(), "alsa-utils".into()];
    for deb in debs {
        dependencies.extend(
            field(deb, "Depends")?
                .split(',')
                .map(str::trim)
                .filter(|dep| !dep.is_empty())
                .map(str::to_owned),
        );
    }
    for dep in dependencies {
        let (name, constraint) = bundle(dependency(&dep))?;
        // The bundle's own packages are installed from the bundle, not by apt.
        if matches!(
            name,
            "tessera"
                | "tessera-alsa"
                | "tessera-web"
                | "tessera-dsp"
                | "systemd"
                | "init-system-helpers"
        ) {
            continue;
        }
        if let Some(old) = selected.get(name) {
            let (_, previous) = bundle(dependency(old))?;
            match (previous, constraint) {
                (None, _) => {}
                (_, None) => continue,
                _ if old == &dep => continue,
                (Some((">=", before)), Some((">=", after))) => {
                    if !Command::new("dpkg")
                        .args(["--compare-versions", after, "gt", before])
                        .status()?
                        .success()
                    {
                        continue;
                    }
                }
                _ => {
                    return Err(Failure::new(format!(
                        "cannot combine dependency constraints for {name}"
                    )));
                }
            }
        }
        selected.insert(name.into(), dep);
    }
    let mut result: Vec<_> = selected.into_values().collect();
    result.sort();
    Ok(result)
}

pub(crate) fn write_manifests(directory: &Path, manifest: &Manifest) -> Result<()> {
    fs::write(
        directory.join("manifest.json"),
        serde_json::to_vec(manifest).map_err(|error| Failure::new(error.to_string()))?,
    )?;
    // Legacy feed metadata remains data. Single quotes preserve constraints;
    // scalar identifiers are restricted by their callers and need no quoting.
    let mut text = format!(
        "BUNDLE_NAME=tessera\nBUNDLE_VERSION={}\nBUNDLE_ARCH={}\nBUNDLE_BUILT={}\nBUNDLE_COMMIT={}\nBUNDLE_OS_ID={}\nBUNDLE_OS_CODENAME={}\nBUNDLE_OS_VERSION_ID={}\nBUNDLE_SYSTEMD_MIN=253\nBUNDLE_MIN_NDIMON=1.3.0\nBUNDLE_PROFILES=\"speaker ndimon\"\nBUNDLE_DSP={}\nBUNDLE_DEBS=\"{}\"\n",
        manifest.version,
        manifest.arch,
        manifest.built,
        manifest.commit,
        manifest.os.id,
        manifest.os.codename,
        manifest.os.version_id,
        u8::from(manifest.dsp),
        manifest
            .debs
            .iter()
            .map(|deb| deb.file.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    );
    for (key, values) in [
        (
            "BUNDLE_DEB_PACKAGES",
            manifest
                .debs
                .iter()
                .map(|deb| deb.package.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            "BUNDLE_DEB_SHA256",
            manifest
                .debs
                .iter()
                .map(|deb| deb.sha256.as_str())
                .collect(),
        ),
        (
            "BUNDLE_APT_DEPENDS",
            manifest.apt_depends.iter().map(String::as_str).collect(),
        ),
    ] {
        text.push_str(&format!("{key}=("));
        for value in values {
            if value.contains(['\'', '\n', '\r']) {
                return Err(Failure::new("unsupported manifest value"));
            }
            text.push_str(&format!(" '{value}'"));
        }
        text.push_str(" )\n");
    }
    fs::write(directory.join("manifest.env"), text)?;
    checksums(directory)
}

pub(crate) fn checksums(directory: &Path) -> Result<()> {
    let mut text = String::new();
    for path in regular_files(directory)? {
        if path
            .file_name()
            .is_some_and(|name| name == "SHA256SUMS" || name == "SHA256SUMS.minisig")
        {
            continue;
        }
        let relative = path
            .strip_prefix(directory)
            .map_err(|error| Failure::new(error.to_string()))?;
        text.push_str(&format!(
            "{}  ./{}\n",
            bundle(hash(&path, &mut Native))?,
            relative.display()
        ));
    }
    fs::write(directory.join("SHA256SUMS"), text)?;
    Ok(())
}

pub(crate) fn build(root: &Path, ubuntu: bool) -> Result<PathBuf> {
    // The package check at the end fails on this; say so before a long build.
    crate::news::check_dates(root)?;
    let target = target_dir(root);
    let prebuilt = env_or("BUNDLE_DEB_DIR", "");
    let mut os = Platform {
        id: "debian".into(),
        codename: "trixie".into(),
        version_id: "13".into(),
        systemd_min: 253,
    };
    if ubuntu || prebuilt.is_empty() {
        let release = fs::read_to_string(env_or("BUNDLE_OS_RELEASE", "/etc/os-release"))?;
        let values = bundle(tessera_bundle::manifest::assignments(&release))?;
        let get = |key: &str| {
            values
                .get(key)
                .and_then(|v| v.first())
                .map(String::as_str)
                .unwrap_or("")
        };
        if ubuntu {
            if get("ID") != "ubuntu"
                || get("VERSION_ID") != "24.04"
                || get("VERSION_CODENAME") != "noble"
                || text(Command::new("dpkg").arg("--print-architecture"))? != "arm64"
            {
                return Err(Failure::new(
                    "experimental build requires native Ubuntu 24.04 noble arm64",
                ));
            }
            os = Platform {
                id: "ubuntu".into(),
                codename: "noble".into(),
                version_id: "24.04".into(),
                systemd_min: 253,
            };
        } else if get("ID") != "debian" || get("VERSION_CODENAME") != "trixie" {
            return Err(Failure::new(
                "requires Debian trixie; Ubuntu 24.04 arm64 needs --allow-ubuntu-24.04",
            ));
        }
    }
    if prebuilt.is_empty() {
        let host = text(Command::new("rustc").arg("-vV"))?
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .unwrap_or("")
            .to_owned();
        if env_or("TARGET", &host) != host {
            return Err(Failure::new("bundle builds must be native"));
        }
        run(Command::new(std::env::current_exe()?)
            .arg("build-deb")
            .current_dir(root)
            .env("TESSERA_XTASK_ROOT", root)
            .env("TESSERA_DAEMON_FEATURES", "alsa-bridge")
            .env("SKIP_PW", "1"))?;
    }
    if env_or("NO_BUILD", "0") != "1" {
        run(Command::new("cargo").current_dir(root).args([
            "build",
            "--release",
            "--locked",
            "-p",
            "tessera-bundle",
        ]))?;
    }
    let directory = if prebuilt.is_empty() {
        target.join("deb")
    } else {
        let directory = PathBuf::from(prebuilt);
        if directory.is_absolute() {
            directory
        } else {
            root.join(directory)
        }
    };
    let mut debs = Vec::new();
    for package in ["tessera", "tessera-alsa", "tessera-web"] {
        for entry in fs::read_dir(&directory)? {
            let path = entry?.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with(&format!("{package}_")) && name.ends_with(".deb") {
                debs.push(path);
            }
        }
    }
    if debs.len() != 3 {
        return Err(Failure::new(
            "expected exactly tessera, tessera-alsa and tessera-web debs",
        ));
    }
    // The CamillaDSP stage rides along when it was built (amd64 and arm64 builds
    // without SKIP_DSP=1): the speaker profile installs it by default and
    // `tessera-bundle install --no-dsp` leaves it out.
    let mut dsp = Vec::new();
    for entry in fs::read_dir(&directory)? {
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with("tessera-dsp_") && name.ends_with(".deb") {
            dsp.push(path.clone());
        }
    }
    if dsp.len() > 1 {
        return Err(Failure::new("expected at most one tessera-dsp deb"));
    }
    debs.extend(dsp);
    let checker = env_or("BUNDLE_CHECK_DEB", "");
    if checker.is_empty() {
        crate::check_deb::check(root, &debs)?;
    } else {
        run(Command::new(checker).args(&debs))?;
    }
    assemble(
        root,
        &target,
        &debs,
        os,
        &target.join("release/tessera-bundle"),
    )
}

fn assemble(
    root: &Path,
    target: &Path,
    debs: &[PathBuf],
    os: Platform,
    binary: &Path,
) -> Result<PathBuf> {
    let version = field(&debs[0], "Version")?;
    let arch = field(&debs[0], "Architecture")?;
    if !tessera_bundle::manifest::token(&version)
        || !matches!(arch.as_str(), "amd64" | "arm64")
        || (os.id == "ubuntu" && arch != "arm64")
    {
        return Err(Failure::new("invalid bundle version or architecture"));
    }
    for deb in debs {
        if field(deb, "Version")? != version || field(deb, "Architecture")? != arch {
            return Err(Failure::new("bundle package versions/architectures differ"));
        }
    }
    let epoch = env_or(
        "SOURCE_DATE_EPOCH",
        &text(
            Command::new("git")
                .current_dir(root)
                .args(["log", "-1", "--format=%ct"]),
        )?,
    );
    let built = text(Command::new("date").args(["-u", "-d", &format!("@{epoch}"), "+%FT%TZ"]))?;
    let commit = text(
        Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "HEAD"]),
    )?;
    let name = format!(
        "tessera-bundle_{version}_{arch}{}",
        if os.id == "ubuntu" {
            "_ubuntu-noble-experimental"
        } else {
            ""
        }
    );
    let work = TempDir::new("tessera-build-bundle")?;
    let stage = work.path().join(&name);
    copy_tree(&root.join("packaging/bundle"), &stage)?;
    let _ = fs::remove_file(stage.join("manifest.schema.md"));
    copy(
        &root.join("docs/integration-contract.json"),
        &stage.join("integration-contract.json"),
        false,
    )?;
    copy(binary, &stage.join("tessera-bundle"), true)?;
    let mut entries = Vec::new();
    let mut dsp = false;
    for deb in debs {
        let file = format!(
            "debs/{}",
            deb.file_name().unwrap_or_default().to_string_lossy()
        );
        copy(deb, &stage.join(&file), false)?;
        let package = field(deb, "Package")?;
        dsp |= package == "tessera-dsp";
        entries.push(Deb {
            file,
            package,
            sha256: bundle(hash(deb, &mut Native))?,
        });
    }
    if dsp {
        // The flat starting configuration the speaker profile seeds for the card.
        copy(
            &root.join("packaging/dsp/dist/Speaker.yml.in"),
            &stage.join("profiles/speaker/camilladsp-Speaker.yml.in"),
            false,
        )?;
    }
    let manifest = Manifest {
        name: "tessera".into(),
        version,
        arch,
        built,
        commit,
        os,
        apt_depends: combined_dependencies(debs)?,
        debs: entries,
        dsp,
        profiles: vec!["speaker".into(), "ndimon".into()],
        min_ndimon: "1.3.0".into(),
    };
    write_manifests(&stage, &manifest)?;
    let verified = bundle(tessera_bundle::manifest::load(&stage, &mut Native))?;
    bundle(tessera_bundle::manifest::verify(
        &stage,
        &verified,
        &mut Native,
    ))?;
    // Temporary trees are private; archive directories themselves retain 0755.
    fn directories(path: &Path) -> Result<()> {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                directories(&entry.path())?;
            }
        }
        Ok(())
    }
    directories(&stage)?;
    fs::create_dir_all(target.join("bundle"))?;
    let archive = target.join("bundle").join(format!("{name}.tar.gz"));
    run(Command::new("tar")
        .args([
            "--sort=name",
            &format!("--mtime=@{epoch}"),
            "--owner=0",
            "--group=0",
            "--numeric-owner",
        ])
        .arg("-C")
        .arg(work.path())
        .arg("-czf")
        .arg(&archive)
        .arg(&name))?;
    println!("{}", archive.display());
    Ok(archive)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixture_archive_is_reproducible_and_has_only_the_binary_entrypoint() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let work = TempDir::new("tessera-bundle-archive-test").unwrap();
        let mut debs = Vec::new();
        for package in ["tessera", "tessera-alsa", "tessera-web"] {
            let stage = work.path().join(package);
            fs::create_dir_all(stage.join("DEBIAN")).unwrap();
            fs::write(stage.join("DEBIAN/control"), format!("Package: {package}\nVersion: 0.2.1-1\nArchitecture: amd64\nMaintainer: Fixture <fixture@invalid>\nDescription: fixture\nDepends: libc6 (>= {}), fixture-data\n", if package == "tessera-alsa" { "2.39" } else { "2.34" })).unwrap();
            let deb = work.path().join(format!("{package}_0.2.1-1_amd64.deb"));
            run(Command::new("dpkg-deb").arg("--build").arg(stage).arg(&deb)).unwrap();
            debs.push(deb);
        }
        let platform = || Platform {
            id: "debian".into(),
            codename: "trixie".into(),
            version_id: "13".into(),
            systemd_min: 253,
        };
        let binary = std::env::current_exe().unwrap();
        let first = assemble(root, work.path(), &debs, platform(), &binary).unwrap();
        let bytes = fs::read(&first).unwrap();
        let listing = text(Command::new("tar").arg("tzf").arg(&first)).unwrap();
        assert!(
            listing
                .lines()
                .any(|line| line == "tessera-bundle_0.2.1-1_amd64/tessera-bundle")
        );
        assert!(!listing.lines().any(|line| line.ends_with(".sh")));
        let second = assemble(root, work.path(), &debs, platform(), &binary).unwrap();
        assert_eq!(bytes, fs::read(second).unwrap());
        assert_eq!(
            combined_dependencies(&debs).unwrap(),
            [
                "alsa-utils",
                "avahi-daemon",
                "fixture-data",
                "libc6 (>= 2.39)"
            ]
        );
        assert!(
            !listing
                .lines()
                .any(|line| line.ends_with("camilladsp-Speaker.yml.in")),
            "no seed configuration without the CamillaDSP stage"
        );
        let manifest = text(
            Command::new("tar")
                .arg("xzOf")
                .arg(&first)
                .arg("tessera-bundle_0.2.1-1_amd64/manifest.json"),
        )
        .unwrap();
        assert!(manifest.contains("\"dsp\":false"), "{manifest}");
    }

    #[test]
    fn the_camilladsp_stage_is_part_of_the_bundle_when_it_was_built() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let work = TempDir::new("tessera-bundle-dsp-test").unwrap();
        let mut debs = Vec::new();
        for package in ["tessera", "tessera-alsa", "tessera-dsp", "tessera-web"] {
            let stage = work.path().join(package);
            fs::create_dir_all(stage.join("DEBIAN")).unwrap();
            let depends = match package {
                "tessera-dsp" => {
                    "libc6 (>= 2.34), tessera (= 0.2.1-1), tessera-alsa (= 0.2.1-1), alsa-utils"
                }
                "tessera-alsa" | "tessera-web" => "libc6 (>= 2.34), tessera (= 0.2.1-1)",
                _ => "libc6 (>= 2.34)",
            };
            fs::write(stage.join("DEBIAN/control"), format!("Package: {package}\nVersion: 0.2.1-1\nArchitecture: amd64\nMaintainer: Fixture <fixture@invalid>\nDescription: fixture\nDepends: {depends}\n")).unwrap();
            let deb = work.path().join(format!("{package}_0.2.1-1_amd64.deb"));
            run(Command::new("dpkg-deb").arg("--build").arg(stage).arg(&deb)).unwrap();
            debs.push(deb);
        }
        // The bundle's own packages never become apt dependencies of the bundle.
        assert_eq!(
            combined_dependencies(&debs).unwrap(),
            ["alsa-utils", "avahi-daemon", "libc6 (>= 2.34)"]
        );
        let platform = Platform {
            id: "debian".into(),
            codename: "trixie".into(),
            version_id: "13".into(),
            systemd_min: 253,
        };
        let archive = assemble(
            root,
            work.path(),
            &debs,
            platform,
            &std::env::current_exe().unwrap(),
        )
        .unwrap();
        let listing = text(Command::new("tar").arg("tzf").arg(&archive)).unwrap();
        for entry in [
            "debs/tessera-dsp_0.2.1-1_amd64.deb",
            "profiles/speaker/camilladsp-Speaker.yml.in",
            "profiles/speaker/60-speaker-dsp.toml.in",
            "profiles/speaker/web.d/60-speaker.toml.in",
        ] {
            assert!(
                listing.lines().any(|line| line.ends_with(entry)),
                "{entry}\n{listing}"
            );
        }
        let read = |name: &str| {
            text(
                Command::new("tar")
                    .arg("xzOf")
                    .arg(&archive)
                    .arg(format!("tessera-bundle_0.2.1-1_amd64/{name}")),
            )
            .unwrap()
        };
        assert!(read("manifest.json").contains("\"dsp\":true"));
        assert!(read("manifest.env").contains("\nBUNDLE_DSP=1\n"));
        let extracted = work.path().join("extracted");
        fs::create_dir(&extracted).unwrap();
        run(Command::new("tar")
            .arg("xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&extracted))
        .unwrap();
        let directory = extracted.join("tessera-bundle_0.2.1-1_amd64");
        let manifest = bundle(tessera_bundle::manifest::load(&directory, &mut Native)).unwrap();
        assert!(manifest.dsp);
        bundle(tessera_bundle::manifest::verify(
            &directory,
            &manifest,
            &mut Native,
        ))
        .unwrap();
    }
}
