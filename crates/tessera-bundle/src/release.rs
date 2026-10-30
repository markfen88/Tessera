//! Counterpart contract §10. Authenticate checksum records before using them.
use minisign_verify::{PublicKey, Signature};
use rustix::fs::{Mode, OFlags, open, openat};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

/// The project's release key (`packaging/tessera-release.pub`, key id
/// 4E330826A26C2812), from 0.5.2. Releases are signed by the public release
/// workflow in its approval-gated `release` environment
/// (`packaging/README.md`, "Signed releases").
pub const TRUSTED_KEYS: &[&str] = &["RWQSKGyiJggzTsm26Ka9OgO4I+RhXqXDTrVl2LXP1ZHFBCJ2lsE/TlgE"];
pub const OVERRIDE_FILE: &str = "/etc/tessera/trusted-release-keys";
pub const MAX_SUMS: usize = 1024 * 1024;
pub const MAX_SIGNATURE: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Error {
    #[error("untrusted_key")]
    UntrustedKey,
    #[error("bad_signature")]
    BadSignature,
    #[error("wrong_product")]
    WrongProduct,
    #[error("wrong_version")]
    WrongVersion,
    #[error("missing_file: {name}")]
    MissingFile { name: String },
    #[error("checksum_mismatch: {name}")]
    ChecksumMismatch { name: String },
    #[error("malformed: {reason}")]
    Malformed { reason: String },
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UntrustedKey => "untrusted_key",
            Self::BadSignature => "bad_signature",
            Self::WrongProduct => "wrong_product",
            Self::WrongVersion => "wrong_version",
            Self::MissingFile { .. } => "missing_file",
            Self::ChecksumMismatch { .. } => "checksum_mismatch",
            Self::Malformed { .. } => "malformed",
        }
    }
}

fn malformed(reason: &str) -> Error {
    Error::Malformed {
        reason: reason.into(),
    }
}

type Result<T> = std::result::Result<T, Error>;

pub fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 128
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".+:~-".contains(&b))
}

pub fn trusted_comment(version: &str) -> Result<String> {
    if !valid_version(version) {
        return Err(malformed("invalid release version"));
    }
    Ok(format!(
        "product:tessera\tversion:{version}\tfile:SHA256SUMS"
    ))
}

/// Release assets are flat; extracted bundles also contain debs/ and profiles/.
/// A single leading ./ is accepted for the existing GNU bundle format.
pub fn checksum_path(name: &str) -> Option<&str> {
    let name = name.strip_prefix("./").unwrap_or(name);
    if name.is_empty()
        || name.len() > 4096
        || matches!(name, "SHA256SUMS" | "SHA256SUMS.minisig")
        || !name.split('/').all(|part| {
            !part.is_empty()
                && part.len() <= 255
                && part != "."
                && part != ".."
                && !part.starts_with(['.', '-', ' '])
                && !part.ends_with(' ')
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._+-~ ".contains(&b))
        })
    {
        None
    } else {
        Some(name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checksums {
    entries: BTreeMap<String, String>,
}

impl Checksums {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > MAX_SUMS || !bytes.ends_with(b"\n") {
            return Err(malformed("empty, oversized or unterminated SHA256SUMS"));
        }
        let text = std::str::from_utf8(bytes).map_err(|_| malformed("SHA256SUMS is not UTF-8"))?;
        let mut entries = BTreeMap::new();
        for line in text.split_terminator('\n') {
            let record = line.as_bytes();
            if record.len() < 67
                || !record[..64].iter().all(u8::is_ascii_hexdigit)
                || record[64] != b' '
                || !matches!(record[65], b' ' | b'*')
            {
                return Err(malformed("invalid GNU SHA256SUMS record"));
            }
            let name =
                checksum_path(&line[66..]).ok_or_else(|| malformed("unsafe checksum path"))?;
            if entries
                .insert(name.into(), line[..64].to_ascii_lowercase())
                .is_some()
            {
                return Err(malformed("duplicate checksum path"));
            }
        }
        Ok(Self { entries })
    }

    pub fn entries(&self) -> &BTreeMap<String, String> {
        &self.entries
    }

    pub fn verify_files(&self, directory: &Path) -> Result<()> {
        for (name, expected) in &self.entries {
            if digest(&mut regular_file(directory, name)?)? != *expected {
                return Err(Error::ChecksumMismatch { name: name.clone() });
            }
        }
        // No unlisted profiles, executables or package payloads can be consumed
        // by installation after verification of a smaller checksum set.
        for name in bundle_files(directory)? {
            if !self.entries.contains_key(&name) {
                return Err(Error::MissingFile { name });
            }
        }
        Ok(())
    }
}

/// All path traversal is relative to open directory handles, with no symlinks
/// or blocking special files at any component. Extract into a private directory
/// before verification and keep it private until installation finishes.
fn regular_file(directory: &Path, name: &str) -> Result<File> {
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let mut parent = File::from(
        open(directory, flags | OFlags::DIRECTORY, Mode::empty())
            .map_err(|_| malformed("cannot open bundle directory"))?,
    );
    let mut parts = Path::new(name).components().peekable();
    while let Some(Component::Normal(part)) = parts.next() {
        let last = parts.peek().is_none();
        let fd = openat(
            &parent,
            part,
            if last {
                flags
            } else {
                flags | OFlags::DIRECTORY
            },
            Mode::empty(),
        )
        .map_err(|error| {
            if error == rustix::io::Errno::NOENT {
                Error::MissingFile { name: name.into() }
            } else {
                malformed("cannot open regular bundle file")
            }
        })?;
        let file = File::from(fd);
        if last {
            if !file
                .metadata()
                .map_err(|_| malformed("cannot inspect bundle file"))?
                .is_file()
            {
                return Err(malformed("bundle file is not regular"));
            }
            return Ok(file);
        }
        parent = file;
    }
    Err(malformed("invalid bundle path"))
}

pub fn digest(reader: &mut impl Read) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buffer)
            .map_err(|_| malformed("cannot read bundle file"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn read_small(directory: &Path, name: &str, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    regular_file(directory, name)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| malformed("cannot read release metadata"))?;
    if bytes.len() > limit {
        return Err(malformed("release metadata exceeds size limit"));
    }
    Ok(bytes)
}

pub fn bundle_files(directory: &Path) -> Result<Vec<String>> {
    fn walk(base: &Path, dir: &Path, names: &mut Vec<String>) -> Result<()> {
        for entry in fs::read_dir(dir).map_err(|_| malformed("cannot list bundle"))? {
            let entry = entry.map_err(|_| malformed("cannot list bundle entry"))?;
            let path = entry.path();
            let name = path
                .strip_prefix(base)
                .ok()
                .and_then(Path::to_str)
                .ok_or_else(|| malformed("invalid bundle filename"))?;
            let kind = entry
                .file_type()
                .map_err(|_| malformed("cannot inspect bundle entry"))?;
            if matches!(name, "SHA256SUMS" | "SHA256SUMS.minisig") {
                if !kind.is_file() {
                    return Err(malformed("release metadata is not regular"));
                }
                continue;
            }
            if checksum_path(name) != Some(name) {
                return Err(malformed("unsafe bundle filename"));
            }
            if kind.is_dir() {
                walk(base, &path, names)?;
            } else if kind.is_file() {
                names.push(name.into());
            } else {
                return Err(malformed("bundle contains a link or special file"));
            }
        }
        Ok(())
    }
    let mut names = Vec::new();
    walk(directory, directory, &mut names)?;
    names.sort();
    Ok(names)
}

pub fn make_checksums(directory: &Path) -> Result<Vec<u8>> {
    let mut text = String::new();
    for name in bundle_files(directory)? {
        text.push_str(&format!(
            "{}  {name}\n",
            digest(&mut regular_file(directory, &name)?)?
        ));
    }
    Checksums::parse(text.as_bytes())?;
    Ok(text.into_bytes())
}

#[derive(Debug)]
pub struct Verified {
    pub version: String,
    pub checksums: Checksums,
}

pub fn verify_manifest(
    bytes: &[u8],
    envelope: &str,
    keys: &[String],
    expected: Option<&str>,
) -> Result<Verified> {
    if keys.is_empty() {
        return Err(Error::UntrustedKey);
    }
    if bytes.len() > MAX_SUMS || envelope.len() > MAX_SIGNATURE || envelope.lines().count() != 4 {
        return Err(malformed("invalid minisign envelope"));
    }
    let signature =
        Signature::decode(envelope).map_err(|_| malformed("invalid minisign signature"))?;
    let mut matching = false;
    let mut authenticated = false;
    for encoded in keys {
        let key =
            PublicKey::from_base64(encoded).map_err(|_| malformed("invalid trusted public key"))?;
        match key.verify(bytes, &signature, false) {
            Ok(()) => {
                authenticated = true;
                break;
            }
            Err(minisign_verify::Error::UnexpectedKeyId) => (),
            Err(minisign_verify::Error::UnexpectedAlgorithm) => {
                return Err(malformed("release requires prehashed ED minisign"));
            }
            Err(_) => matching = true,
        }
    }
    if !authenticated {
        return Err(if matching {
            Error::BadSignature
        } else {
            Error::UntrustedKey
        });
    }
    let fields: Vec<_> = signature.trusted_comment().split('\t').collect();
    if fields.len() != 3 {
        return Err(malformed(
            "expected product, version and file in trusted comment",
        ));
    }
    if fields[0] != "product:tessera" {
        return Err(Error::WrongProduct);
    }
    let version = fields[1]
        .strip_prefix("version:")
        .filter(|v| valid_version(v))
        .ok_or_else(|| malformed("invalid trusted version"))?;
    if expected.is_some_and(|expected| expected != version) {
        return Err(Error::WrongVersion);
    }
    if fields[2] != "file:SHA256SUMS" {
        return Err(malformed("trusted comment must name SHA256SUMS"));
    }
    Ok(Verified {
        version: version.into(),
        checksums: Checksums::parse(bytes)?,
    })
}

pub fn verify_directory(
    directory: &Path,
    keys: &[String],
    expected: Option<&str>,
) -> Result<Verified> {
    if keys.is_empty() {
        return Err(Error::UntrustedKey);
    }
    let bytes = read_small(directory, "SHA256SUMS", MAX_SUMS)?;
    let sig = read_small(directory, "SHA256SUMS.minisig", MAX_SIGNATURE)?;
    let sig = std::str::from_utf8(&sig).map_err(|_| malformed("signature is not UTF-8"))?;
    let verified = verify_manifest(&bytes, sig, keys, expected)?;
    verified.checksums.verify_files(directory)?;
    Ok(verified)
}

pub fn validate_key_metadata(uid: u32, mode: u32, regular: bool) -> Result<()> {
    if uid != 0 || mode & 0o7777 != 0o644 || !regular {
        return Err(malformed(
            "trusted keys require a root-owned regular file with mode 0644",
        ));
    }
    Ok(())
}

/// Optional additive trust, anchored at root. Pin ancestors as well as the
/// final file so a writable parent cannot replace a safe-looking key file.
pub fn load_keys(path: &Path) -> Result<Vec<String>> {
    if !path.is_absolute() {
        return Err(malformed("trusted-key path must be absolute"));
    }
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let mut parent = File::from(
        open("/", flags | OFlags::DIRECTORY, Mode::empty())
            .map_err(|_| malformed("cannot open trust root"))?,
    );
    let mut keys: Vec<String> = TRUSTED_KEYS.iter().map(|key| (*key).into()).collect();
    let mut parts = path.components().skip(1).peekable();
    while let Some(part) = parts.next() {
        let Component::Normal(part) = part else {
            return Err(malformed("invalid trusted-key path"));
        };
        let metadata = parent
            .metadata()
            .map_err(|_| malformed("cannot inspect trust parent"))?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(malformed(
                "trusted-key parent must be root-owned and not group/world-writable",
            ));
        }
        let last = parts.peek().is_none();
        let fd = match openat(
            &parent,
            part,
            if last {
                flags
            } else {
                flags | OFlags::DIRECTORY
            },
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(keys),
            Err(_) => return Err(malformed("cannot open trusted keys without symlinks")),
        };
        let file = File::from(fd);
        if !last {
            parent = file;
            continue;
        }
        let metadata = file
            .metadata()
            .map_err(|_| malformed("cannot inspect trusted keys"))?;
        validate_key_metadata(metadata.uid(), metadata.mode(), metadata.is_file())?;
        let mut text = String::new();
        file.take(16 * 1024 + 1)
            .read_to_string(&mut text)
            .map_err(|_| malformed("cannot read trusted keys"))?;
        if text.len() > 16 * 1024 {
            return Err(malformed("trusted keys exceed size limit"));
        }
        for line in text.lines().filter(|line| !line.is_empty()) {
            PublicKey::from_base64(line).map_err(|_| malformed("invalid override public key"))?;
            if !keys.iter().any(|key| key == line) {
                keys.push(line.into());
            }
        }
        return Ok(keys);
    }
    Err(malformed("trusted-key path must name a file"))
}
