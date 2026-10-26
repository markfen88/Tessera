//! The pinned upstream programs of the `tessera-dsp` package: CamillaDSP and the
//! CamillaGUI backend, unmodified official release archives named in
//! `packaging/dsp/dsp.lock` with the SHA-256 of each archive. An archive is used
//! only when its digest equals the pin: a cached file that differs is discarded,
//! a download that differs is refused, and nothing is extracted before that.

use crate::files::TempDir;
use crate::{Failure, Result, env_or, run};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One line of `dsp.lock`: `component version arch url sha256`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pin {
    pub(crate) component: String,
    pub(crate) version: String,
    pub(crate) arch: String,
    pub(crate) url: String,
    pub(crate) sha256: String,
}

/// Where official release archives may come from.
const ORIGIN: &str = "https://github.com/";

fn lowercase_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

/// Parse `dsp.lock`: blank lines and `#` comments are skipped; every other line has
/// five fields, a known component and architecture, a lowercase 64-digit digest and
/// an `https://github.com/` URL.
pub(crate) fn parse_lock(text: &str) -> Result<Vec<Pin>> {
    let mut pins: Vec<Pin> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = line.split_whitespace().collect();
        let [component, version, arch, url, sha256] = fields[..] else {
            return Err(Failure::new(format!("dsp.lock: malformed line: {line}")));
        };
        if !matches!(component, "camilladsp" | "camillagui") {
            return Err(Failure::new(format!(
                "dsp.lock: unknown component {component}"
            )));
        }
        if !matches!(arch, "amd64" | "arm64") {
            return Err(Failure::new(format!(
                "dsp.lock: no CamillaDSP artifacts for architecture {arch}"
            )));
        }
        if !tessera_bundle::manifest::token(version) {
            return Err(Failure::new(format!(
                "dsp.lock: bad version for {component}"
            )));
        }
        if !lowercase_hex(sha256) {
            return Err(Failure::new(format!(
                "bad checksum for {component} in dsp.lock"
            )));
        }
        if !url.starts_with(ORIGIN) || url.contains(char::is_control) {
            return Err(Failure::new(format!(
                "dsp.lock: unexpected URL for {component}: {url}"
            )));
        }
        if pins
            .iter()
            .any(|pin| pin.component == component && pin.arch == arch)
        {
            return Err(Failure::new(format!(
                "dsp.lock: {component} is pinned twice for {arch}"
            )));
        }
        pins.push(Pin {
            component: component.into(),
            version: version.into(),
            arch: arch.into(),
            url: url.into(),
            sha256: sha256.into(),
        });
    }
    Ok(pins)
}

/// The SHA-256 of a file, streamed.
pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// The verified archive of a pin in `cache`, downloading it with `download` (given the
/// URL and a destination) when it is not there. The cached name carries the digest, so
/// a changed pin never finds an old file.
fn obtain(
    cache: &Path,
    pin: &Pin,
    download: &mut dyn FnMut(&str, &Path) -> Result<()>,
) -> Result<PathBuf> {
    let name = pin.url.rsplit('/').next().unwrap_or("archive");
    let file = cache.join(format!("{name}-{}", pin.sha256));
    if file.is_file() && sha256_file(&file)? != pin.sha256 {
        eprintln!(
            "dsp: cached {} archive does not match its pin; discarding it",
            pin.component
        );
        fs::remove_file(&file)?;
    }
    if !file.is_file() {
        eprintln!(
            "dsp: downloading {} {} ({})",
            pin.component, pin.version, pin.arch
        );
        let part = cache.join(format!("{name}-{}.part", pin.sha256));
        download(&pin.url, &part)?;
        fs::rename(&part, &file)?;
    }
    if sha256_file(&file)? != pin.sha256 {
        let _ = fs::remove_file(&file);
        return Err(Failure::new(format!(
            "{} archive does not match the pinned SHA-256; refusing it",
            pin.component
        )));
    }
    Ok(file)
}

fn curl(url: &str, destination: &Path) -> Result<()> {
    run(Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--retry",
            "3",
            "--max-time",
            "600",
            "-o",
        ])
        .arg(destination)
        .arg(url))
}

fn extract(archive: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    run(Command::new("tar")
        .args(["--no-same-owner", "--no-same-permissions", "-xzf"])
        .arg(archive)
        .arg("-C")
        .arg(destination))
}

/// Copy a program directory the way `cp -a --no-preserve=ownership` would, with the
/// modes the package wants: a regular file keeps only whether it is executable
/// (0755 or 0644), directories are 0755, symlinks stay symlinks.
pub(crate) fn copy_program_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o755))?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if kind.is_dir() {
            copy_program_tree(&entry.path(), &target)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target)?;
            let executable = fs::metadata(entry.path())?.permissions().mode() & 0o111 != 0;
            fs::set_permissions(
                &target,
                fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
            )?;
        } else {
            return Err(Failure::new(format!(
                "unexpected file type in {}",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

/// The verified, extracted trees for one architecture.
#[derive(Debug)]
pub(crate) struct Fetched {
    /// `<tree>/camilladsp/camilladsp` and `<tree>/camillagui/camillagui_backend/...`;
    /// removed with the value.
    pub(crate) tree: TempDir,
    pub(crate) camilladsp: String,
    pub(crate) camillagui: String,
}

impl Fetched {
    pub(crate) fn camilladsp_binary(&self) -> PathBuf {
        self.tree.path().join("camilladsp/camilladsp")
    }

    pub(crate) fn gui_directory(&self) -> PathBuf {
        self.tree.path().join("camillagui/camillagui_backend")
    }
}

fn executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn unpack(
    cache: &Path,
    pins: &[Pin],
    arch: &str,
    download: &mut dyn FnMut(&str, &Path) -> Result<()>,
) -> Result<Fetched> {
    fs::create_dir_all(cache)?;
    let tree = TempDir::in_directory(cache, "work")?;
    let mut versions = (String::new(), String::new());
    for pin in pins.iter().filter(|pin| pin.arch == arch) {
        let archive = obtain(cache, pin, download)?;
        extract(&archive, &tree.path().join(&pin.component))?;
        match pin.component.as_str() {
            "camilladsp" => versions.0.clone_from(&pin.version),
            _ => versions.1.clone_from(&pin.version),
        }
    }
    if versions.0.is_empty() || versions.1.is_empty() {
        return Err(Failure::new(format!(
            "dsp.lock pins no CamillaDSP and GUI for {arch}"
        )));
    }
    let fetched = Fetched {
        tree,
        camilladsp: versions.0,
        camillagui: versions.1,
    };
    if !executable(&fetched.camilladsp_binary()) {
        return Err(Failure::new("camilladsp binary missing from the archive"));
    }
    if !executable(&fetched.gui_directory().join("camillagui_backend")) {
        return Err(Failure::new("GUI backend missing from the archive"));
    }
    Ok(fetched)
}

/// Fetch and verify the pinned archives for `arch` (`amd64` or `arm64`) into
/// `$TESSERA_DSP_CACHE` (default `~/.cache/tessera-dsp`) and extract them.
pub(crate) fn fetch(root: &Path, arch: &str) -> Result<Fetched> {
    if !matches!(arch, "amd64" | "arm64") {
        return Err(Failure::new(format!(
            "no CamillaDSP artifacts for architecture {arch}"
        )));
    }
    let pins = parse_lock(&fs::read_to_string(root.join("packaging/dsp/dsp.lock"))?)?;
    let home = env_or("HOME", "");
    let cache = match env_or("TESSERA_DSP_CACHE", "") {
        value if !value.is_empty() => PathBuf::from(value),
        _ if !home.is_empty() => PathBuf::from(home).join(".cache/tessera-dsp"),
        _ => {
            return Err(Failure::new(
                "set TESSERA_DSP_CACHE or HOME for the CamillaDSP cache",
            ));
        }
    };
    unpack(&cache, &pins, arch, &mut curl)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const SHA: &str = "55f5ec2ed80fcc79a543672f9f89ace4557d290d80584ef31ee0442111bd0b11";

    fn line(component: &str, arch: &str) -> String {
        format!(
            "{component} 4.1.3 {arch} https://github.com/HEnquist/{component}/releases/download/v4.1.3/{component}-{arch}.tar.gz {SHA}\n"
        )
    }

    fn pin(sha256: &str) -> Pin {
        Pin {
            component: "camilladsp".into(),
            version: "4.1.3".into(),
            arch: "amd64".into(),
            url: "https://github.com/HEnquist/camilladsp/releases/download/v4.1.3/camilladsp-linux-amd64.tar.gz".into(),
            sha256: sha256.into(),
        }
    }

    fn digest(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[test]
    fn the_repository_lock_pins_both_programs_for_both_architectures() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let pins =
            parse_lock(&fs::read_to_string(root.join("packaging/dsp/dsp.lock")).unwrap()).unwrap();
        assert_eq!(pins.len(), 4);
        for component in ["camilladsp", "camillagui"] {
            for arch in ["amd64", "arm64"] {
                assert_eq!(
                    pins.iter()
                        .filter(|pin| pin.component == component && pin.arch == arch)
                        .count(),
                    1,
                    "{component} {arch}"
                );
            }
        }
    }

    #[test]
    fn lock_lines_are_validated_strictly() {
        let good = format!(
            "# comment\n\n{}{}",
            line("camilladsp", "amd64"),
            line("camillagui", "arm64")
        );
        let pins = parse_lock(&good).unwrap();
        assert_eq!(pins.len(), 2);
        assert_eq!(pins[0].sha256, SHA);
        for (bad, why) in [
            (
                line("camilladsp", "amd64").replace(SHA, &SHA[1..]),
                "short digest",
            ),
            (
                line("camilladsp", "amd64").replace(SHA, &SHA.to_uppercase()),
                "uppercase digest",
            ),
            (
                line("camilladsp", "amd64").replace("https://github.com/", "http://github.com/"),
                "plain http",
            ),
            (
                line("camilladsp", "amd64").replace("https://github.com/", "https://example.com/"),
                "other host",
            ),
            (
                line("camilladsp", "amd64").replace("amd64 ", "armhf "),
                "architecture",
            ),
            (
                line("camilladsp", "amd64").replace("camilladsp 4", "othertool 4"),
                "component",
            ),
            (
                line("camilladsp", "amd64").replace(" 4.1.3 ", " 4.1;3 "),
                "version",
            ),
            (
                format!(
                    "{}{}",
                    line("camilladsp", "amd64"),
                    line("camilladsp", "amd64")
                ),
                "pinned twice",
            ),
            (
                "camilladsp 4.1.3 amd64 https://github.com/x".into(),
                "missing field",
            ),
            (
                format!("{} extra", line("camilladsp", "amd64").trim_end()),
                "extra field",
            ),
        ] {
            assert!(parse_lock(&bad).is_err(), "{why}");
        }
    }

    #[test]
    fn sha256_matches_known_vectors() {
        let temp = TempDir::new("tessera-dsp-sha").unwrap();
        let file = temp.path().join("f");
        fs::write(&file, "abc").unwrap();
        assert_eq!(
            sha256_file(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        fs::write(&file, "").unwrap();
        assert_eq!(
            sha256_file(&file).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn a_cached_archive_with_the_pinned_digest_is_not_downloaded_again() {
        let cache = TempDir::new("tessera-dsp-cache").unwrap();
        let sha = digest(b"archive");
        let pin = pin(&sha);
        let name = format!("camilladsp-linux-amd64.tar.gz-{sha}");
        fs::write(cache.path().join(&name), "archive").unwrap();
        let calls = Cell::new(0);
        let file = obtain(cache.path(), &pin, &mut |_, _| {
            calls.set(calls.get() + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(file, cache.path().join(name));
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn a_tampered_cache_entry_is_discarded_and_fetched_again() {
        let cache = TempDir::new("tessera-dsp-cache").unwrap();
        let sha = digest(b"archive");
        let pin = pin(&sha);
        let name = format!("camilladsp-linux-amd64.tar.gz-{sha}");
        fs::write(cache.path().join(&name), "tampered").unwrap();
        let mut urls = Vec::new();
        let file = obtain(cache.path(), &pin, &mut |url, destination| {
            urls.push(url.to_owned());
            fs::write(destination, "archive")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(urls, std::slice::from_ref(&pin.url));
        assert_eq!(fs::read_to_string(file).unwrap(), "archive");
        assert!(!cache.path().join(format!("{name}.part")).exists());
    }

    #[test]
    fn a_download_that_differs_from_the_pin_is_refused_and_removed() {
        let cache = TempDir::new("tessera-dsp-cache").unwrap();
        let sha = digest(b"archive");
        let pin = pin(&sha);
        let error = obtain(cache.path(), &pin, &mut |_, destination| {
            fs::write(destination, "something else")?;
            Ok(())
        })
        .unwrap_err();
        assert!(error.message.contains("pinned SHA-256"), "{error}");
        assert!(
            !cache
                .path()
                .join(format!("camilladsp-linux-amd64.tar.gz-{sha}"))
                .exists(),
            "a refused archive is not kept"
        );
        // A failing download leaves nothing usable either.
        let error =
            obtain(cache.path(), &pin, &mut |_, _| Err(Failure::new("offline"))).unwrap_err();
        assert_eq!(error.message, "offline");
    }

    #[test]
    fn a_refused_archive_leaves_no_work_directory_behind() {
        let cache = TempDir::new("tessera-dsp-unpack").unwrap();
        let pins = parse_lock(&format!(
            "{}{}",
            line("camilladsp", "amd64"),
            line("camillagui", "amd64")
        ))
        .unwrap();
        // The fake download is not a tar archive and has the wrong digest: the
        // refusal comes first, so tar never sees it (it would fail differently).
        let error = unpack(cache.path(), &pins, "amd64", &mut |_, destination| {
            fs::write(destination, "not the pinned archive")?;
            Ok(())
        })
        .unwrap_err();
        assert!(error.message.contains("pinned SHA-256"), "{error}");
        let leftovers = fs::read_dir(cache.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .flat_map(|dir| {
                fs::read_dir(dir)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
            })
            .count();
        assert_eq!(leftovers, 0, "no extraction directory was populated");
        assert!(fetch(Path::new("/nonexistent"), "armhf").is_err());
    }
}
