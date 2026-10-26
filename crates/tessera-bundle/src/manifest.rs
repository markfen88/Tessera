//! Manifest and checksum data are never evaluated as shell code.
use crate::{Error, Result, Runner, checked, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Platform {
    pub id: String,
    pub codename: String,
    #[serde(default)]
    pub version_id: String,
    pub systemd_min: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Deb {
    pub file: String,
    pub package: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub arch: String,
    pub built: String,
    pub commit: String,
    pub os: Platform,
    pub apt_depends: Vec<String>,
    pub debs: Vec<Deb>,
    /// The bundle carries `tessera-dsp`, which the speaker profile installs by default.
    #[serde(default)]
    pub dsp: bool,
    pub profiles: Vec<String>,
    pub min_ndimon: String,
}

pub fn token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".+:~_-".contains(&c))
}

pub fn dependency(value: &str) -> Result<(&str, Option<(&str, &str)>)> {
    let (name, constraint) = if let Some((name, expression)) = value.split_once(" (") {
        let expression = expression
            .strip_suffix(')')
            .ok_or_else(|| Error("invalid dependency".into()))?;
        let (op, version) = expression
            .split_once(' ')
            .ok_or_else(|| Error("invalid dependency".into()))?;
        ensure(
            matches!(op, ">=" | "<=" | "=" | ">>" | "<<") && token(version),
            "invalid dependency constraint",
        )?;
        (name, Some((op, version)))
    } else {
        (value, None)
    };
    ensure(
        name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            && name
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"+.:~-".contains(&c)),
        "invalid dependency",
    )?;
    Ok((name, constraint))
}

pub fn relative(value: &str) -> bool {
    !value.is_empty()
        && !Path::new(value).is_absolute()
        && !value.contains(['\n', '\r', '\\'])
        && Path::new(value)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

pub fn sums(text: &str) -> Result<BTreeMap<String, String>> {
    Ok(crate::release::Checksums::parse(text.as_bytes())?
        .entries()
        .clone())
}

pub fn hash(path: &Path, runner: &mut impl Runner) -> Result<String> {
    let text = checked(runner, "sha256sum", &["--", &path.to_string_lossy()])?;
    let hash = text.split_whitespace().next().unwrap_or_default();
    ensure(
        hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid sha256sum output",
    )?;
    Ok(hash.to_ascii_lowercase())
}

fn check_file(
    directory: &Path,
    path: &str,
    expected: &str,
    runner: &mut impl Runner,
) -> Result<()> {
    let mut current = directory.to_owned();
    for part in Path::new(path).components() {
        current.push(part.as_os_str());
        ensure(
            !fs::symlink_metadata(&current)?.file_type().is_symlink(),
            "checksum path is a symlink",
        )?;
    }
    ensure(hash(&current, runner)? == expected, "checksum mismatch")
}

/// Parse only the data quoting emitted by printf %q and os-release. No variable,
/// command, arithmetic or wildcard expansion is performed.
pub fn assignments(text: &str) -> Result<BTreeMap<String, Vec<String>>> {
    let mut values = BTreeMap::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| Error("invalid assignment".into()))?;
        ensure(
            !key.is_empty() && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
            "invalid assignment key",
        )?;
        let value = value
            .strip_prefix('(')
            .map(|value| {
                value
                    .strip_suffix(')')
                    .ok_or_else(|| Error("invalid array".into()))
            })
            .transpose()?
            .unwrap_or(value);
        let mut words = Vec::new();
        let mut word = String::new();
        let mut quoted = None;
        let mut escape = false;
        let mut started = false;
        for c in value.chars() {
            if escape {
                word.push(c);
                escape = false;
                started = true;
            } else if c == '\\' && quoted != Some('\'') {
                escape = true;
                started = true;
            } else if quoted == Some(c) {
                quoted = None;
            } else if matches!(c, '\'' | '"') && quoted.is_none() {
                quoted = Some(c);
                started = true;
            } else if c.is_whitespace() && quoted.is_none() {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            } else {
                word.push(c);
                started = true;
            }
        }
        ensure(!escape && quoted.is_none(), "unterminated assignment")?;
        if started {
            words.push(word);
        }
        ensure(
            values.insert(key.into(), words).is_none(),
            "duplicate assignment",
        )?;
    }
    Ok(values)
}

pub fn load(directory: &Path, runner: &mut impl Runner) -> Result<Manifest> {
    ensure(
        directory.join("manifest.env").is_file(),
        "missing manifest.env",
    )?;
    let checksums = sums(&fs::read_to_string(directory.join("SHA256SUMS"))?)?;
    let expected = checksums
        .get("manifest.env")
        .ok_or_else(|| Error("missing or duplicate manifest checksum".into()))?;
    check_file(directory, "manifest.env", expected, runner)
        .map_err(|_| Error("manifest checksum mismatch".into()))?;
    let env = assignments(&fs::read_to_string(directory.join("manifest.env"))?)?;
    // JSON is the feed contract. Check it before parsing and require it to agree
    // with the legacy data manifest retained for existing feed consumers.
    let expected = checksums
        .get("manifest.json")
        .ok_or_else(|| Error("unchecked manifest.json".into()))?;
    check_file(directory, "manifest.json", expected, runner)?;
    let manifest: Manifest = serde_json::from_slice(&fs::read(directory.join("manifest.json"))?)?;
    for (key, value) in [
        ("BUNDLE_NAME", &manifest.name),
        ("BUNDLE_VERSION", &manifest.version),
        ("BUNDLE_ARCH", &manifest.arch),
        ("BUNDLE_COMMIT", &manifest.commit),
        ("BUNDLE_OS_ID", &manifest.os.id),
        ("BUNDLE_OS_CODENAME", &manifest.os.codename),
    ] {
        ensure(
            env.get(key)
                .is_some_and(|words| words.len() == 1 && words[0] == *value),
            "manifest metadata mismatch",
        )?;
    }
    ensure(
        env.get("BUNDLE_APT_DEPENDS") == Some(&manifest.apt_depends),
        "manifest dependencies mismatch",
    )?;
    // Bundles from before the CamillaDSP stage carry no BUNDLE_DSP: none included.
    ensure(
        env.get("BUNDLE_DSP").is_some_and(|words| *words == ["1"]) == manifest.dsp,
        "manifest dsp mismatch",
    )?;
    let deb_paths = manifest
        .debs
        .iter()
        .map(|deb| deb.file.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    ensure(
        env.get("BUNDLE_DEBS")
            .is_some_and(|words| words.len() == 1 && words[0] == deb_paths),
        "manifest package paths mismatch",
    )?;
    ensure(
        manifest.name == "tessera"
            && token(&manifest.version)
            && token(&manifest.commit)
            && matches!(manifest.arch.as_str(), "amd64" | "arm64"),
        "invalid manifest metadata",
    )?;
    for dep in &manifest.apt_depends {
        dependency(dep)?;
    }
    Ok(manifest)
}

/// Verify the authenticated release metadata and all bundle bytes before use.
pub fn verify_signature(
    directory: &Path,
    keys: &[String],
    version: Option<&str>,
) -> std::result::Result<crate::release::Verified, crate::release::Error> {
    crate::release::verify_directory(directory, keys, version)
}

pub fn verify(directory: &Path, manifest: &Manifest, runner: &mut impl Runner) -> Result<()> {
    let checksums = sums(&fs::read_to_string(directory.join("SHA256SUMS"))?)?;
    for (path, expected) in &checksums {
        check_file(directory, path, expected, runner)?;
    }
    let mut packages = Vec::new();
    for deb in &manifest.debs {
        ensure(
            (matches!(
                deb.package.as_str(),
                "tessera" | "tessera-alsa" | "tessera-web"
            ) || (manifest.dsp && deb.package == "tessera-dsp"))
                && deb.file
                    == format!(
                        "debs/{}_{}_{}.deb",
                        deb.package, manifest.version, manifest.arch
                    ),
            "invalid deb path",
        )?;
        ensure(
            checksums.get(&deb.file) == Some(&deb.sha256),
            &format!("unchecked deb: {}", deb.file),
        )?;
        packages.push(deb.package.as_str());
    }
    packages.sort();
    if manifest.dsp {
        ensure(
            packages == ["tessera", "tessera-alsa", "tessera-dsp", "tessera-web"],
            "expected exactly tessera, tessera-alsa, tessera-dsp and tessera-web debs",
        )
    } else {
        ensure(
            packages == ["tessera", "tessera-alsa", "tessera-web"],
            "expected exactly tessera, tessera-alsa and tessera-web debs",
        )
    }
}
