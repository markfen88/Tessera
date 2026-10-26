use crate::{Failure, Result};
use minisign::{KeyPair, PublicKey, SecretKey};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Cursor, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use tessera_bundle::release as verify;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Sign,
    Verify,
    Keygen,
}

#[derive(Debug, Default)]
struct Arguments {
    version: Option<String>,
    out: Option<PathBuf>,
    bundle: Option<PathBuf>,
    directory: Option<PathBuf>,
    public_key: Option<PathBuf>,
    artifacts: Vec<PathBuf>,
}

fn parse(action: &Action, args: &[String], root: &Path) -> Result<Arguments> {
    let mut parsed = Arguments::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let slot = match (action, arg.as_str()) {
            (Action::Sign | Action::Verify, "--version") => {
                if parsed.version.is_some() {
                    return Err(Failure::new("duplicate --version"));
                }
                let value = args
                    .next()
                    .filter(|v| verify::valid_version(v))
                    .ok_or_else(|| Failure::new("--version requires a valid release version"))?;
                parsed.version = Some(value.clone());
                continue;
            }
            (Action::Sign | Action::Keygen, "--out") => &mut parsed.out,
            (Action::Sign, "--bundle") => &mut parsed.bundle,
            (Action::Verify, "--dir") => &mut parsed.directory,
            (Action::Verify, "--public-key") => &mut parsed.public_key,
            (Action::Sign, name) if !name.starts_with('-') => {
                parsed.artifacts.push(root.join(name));
                continue;
            }
            _ => return Err(Failure::new("unexpected release argument")),
        };
        if slot.is_some() {
            return Err(Failure::new("duplicate release option"));
        }
        let value = args
            .next()
            .filter(|value| !value.is_empty() && !value.starts_with('-'))
            .ok_or_else(|| Failure::new("release option requires a path"))?;
        *slot = Some(root.join(value));
    }
    let valid = match action {
        Action::Keygen => parsed.out.is_some(),
        Action::Verify => parsed.directory.is_some() && parsed.version.is_some(),
        Action::Sign => {
            parsed.version.is_some()
                && match (&parsed.out, &parsed.bundle) {
                    (Some(_), None) => !parsed.artifacts.is_empty(),
                    (None, Some(_)) => parsed.artifacts.is_empty(),
                    _ => false,
                }
        }
    };
    if !valid {
        return Err(Failure::new(
            "usage: release --version V --out DIR ARTIFACT... | release --version V --bundle DIR | verify-release --dir DIR --version V [--public-key FILE] | release-keygen --out DIR",
        ));
    }
    Ok(parsed)
}

fn password() -> Result<String> {
    std::env::var("TESSERA_RELEASE_KEY_PASSWORD")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Failure::new("TESSERA_RELEASE_KEY_PASSWORD must be set and nonempty"))
}

fn public_key(path: &Path) -> Result<String> {
    // Public data only. Accept the normal minisign .pub file or a bare key.
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > 16 * 1024 {
        return Err(Failure::new("invalid public-key file"));
    }
    let text = fs::read_to_string(path)?;
    let lines: Vec<_> = text.lines().collect();
    let key = match lines.as_slice() {
        [key] => *key,
        [comment, key] if comment.starts_with("untrusted comment:") => *key,
        _ => return Err(Failure::new("invalid public-key file")),
    };
    minisign_verify_key(key)?;
    Ok(key.into())
}

fn minisign_verify_key(key: &str) -> Result<()> {
    PublicKey::from_base64(key)
        .map(|_| ())
        .map_err(|_| Failure::new("invalid public key"))
}

fn new_file(path: &Path, mode: u32) -> Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    Ok(file)
}

fn keygen(out: &Path, password: String) -> Result<String> {
    if password.is_empty() {
        return Err(Failure::new("release-keygen requires a nonempty password"));
    }
    DirBuilder::new().mode(0o700).create(out)?;
    let mut secret = new_file(&out.join("tessera-release.key"), 0o600)?;
    let mut public = new_file(&out.join("tessera-release.pub"), 0o644)?;
    let key = KeyPair::generate_and_write_encrypted_keypair(
        &mut public,
        &mut secret,
        None,
        Some(password),
    )
    .map_err(|_| Failure::new("release key generation failed"))?;
    secret.sync_all()?;
    public.sync_all()?;
    Ok(key.pk.to_base64())
}

fn bundle_version(directory: &Path, version: &str) -> Result<()> {
    let path = directory.join("manifest.json");
    if path.exists() {
        let bytes = verify::read_small(directory, "manifest.json", verify::MAX_SUMS)
            .map_err(|error| Failure::new(error.to_string()))?;
        let manifest: tessera_bundle::manifest::Manifest = serde_json::from_slice(&bytes)
            .map_err(|_| Failure::new("malformed bundle manifest"))?;
        if manifest.name != "tessera" {
            return Err(Failure::new("wrong_product"));
        }
        if manifest.version != version {
            return Err(Failure::new("wrong_version"));
        }
    }
    Ok(())
}

fn reject_key_artifact(path: &Path, key: &fs::Metadata) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || (metadata.dev(), metadata.ino()) == (key.dev(), key.ino()) {
        return Err(Failure::new(
            "release input must be a regular artifact, never the signing key",
        ));
    }
    Ok(())
}

fn sign_directory(
    directory: &Path,
    version: &str,
    secret: &SecretKey,
    key_metadata: &fs::Metadata,
) -> Result<()> {
    bundle_version(directory, version)?;
    for name in verify::bundle_files(directory).map_err(|error| Failure::new(error.to_string()))? {
        reject_key_artifact(&directory.join(name), key_metadata)?;
    }
    let sums =
        verify::make_checksums(directory).map_err(|error| Failure::new(error.to_string()))?;
    let public = PublicKey::from_secret_key(secret)
        .map_err(|_| Failure::new("cannot derive release public key"))?;
    let comment =
        verify::trusted_comment(version).map_err(|error| Failure::new(error.to_string()))?;
    let signature = minisign::sign(
        Some(&public),
        secret,
        Cursor::new(&sums),
        Some(&comment),
        None,
    )
    .map_err(|_| Failure::new("release signing failed"))?;
    // Existing metadata must be regular too; never follow a signing output link.
    for name in ["SHA256SUMS", "SHA256SUMS.minisig"] {
        match fs::symlink_metadata(directory.join(name)) {
            Ok(metadata) if metadata.is_file() => {
                reject_key_artifact(&directory.join(name), key_metadata)?;
                fs::remove_file(directory.join(name))?;
            }
            Ok(_) => return Err(Failure::new("release metadata is not a regular file")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    new_file(&directory.join("SHA256SUMS"), 0o644)?.write_all(&sums)?;
    new_file(&directory.join("SHA256SUMS.minisig"), 0o644)?
        .write_all(signature.to_string().as_bytes())?;
    verify::verify_directory(directory, &[public.to_base64()], Some(version))
        .map_err(|error| Failure::new(error.to_string()))?;
    Ok(())
}

fn stage_artifacts(out: &Path, artifacts: &[PathBuf], key: &fs::Metadata) -> Result<()> {
    // Fresh output and explicit assets prevent accidental signing/publication of
    // checkout internals or leftover files from an earlier release.
    DirBuilder::new().mode(0o700).create(out)?;
    for artifact in artifacts {
        reject_key_artifact(artifact, key)?;
        let name = artifact
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|name| {
                verify::checksum_path(name) == Some(*name)
                    && (name.ends_with(".deb") || name.ends_with(".tar.gz"))
            })
            .ok_or_else(|| Failure::new("expected a flat .deb or .tar.gz release artifact"))?;
        let mut input = File::open(artifact)?;
        std::io::copy(&mut input, &mut new_file(&out.join(name), 0o644)?)?;
    }
    let compiler = crate::output(Command::new("rustc").arg("-vV"))?;
    new_file(&out.join("rustc-version.txt"), 0o644)?.write_all(&compiler.stdout)?;
    Ok(())
}

pub(crate) fn execute(action: Action, args: &[String], root: &Path) -> Result<()> {
    let args = parse(&action, args, root)?;
    match action {
        Action::Keygen => println!("{}", keygen(args.out.as_deref().unwrap(), password()?)?),
        Action::Verify => {
            let keys = if let Some(path) = args.public_key {
                vec![public_key(&path)?]
            } else {
                verify::load_keys(Path::new(verify::OVERRIDE_FILE))
                    .map_err(|error| Failure::new(error.to_string()))?
            };
            let directory = args.directory.as_deref().unwrap();
            let version = args.version.as_deref().unwrap();
            verify::verify_directory(directory, &keys, Some(version))
                .map_err(|error| Failure::new(error.to_string()))?;
            bundle_version(directory, version)?;
            println!("verified tessera {version}");
        }
        Action::Sign => {
            let key_file = std::env::var_os("TESSERA_RELEASE_KEY_FILE")
                .filter(|value| !value.is_empty())
                .ok_or_else(|| Failure::new("TESSERA_RELEASE_KEY_FILE must be set"))?;
            let password = password()?;
            // Diagnostics deliberately omit parser details, paths and env values.
            let key_metadata =
                fs::metadata(&key_file).map_err(|_| Failure::new("cannot inspect release key"))?;
            if !key_metadata.is_file()
                || key_metadata.len() > 16 * 1024
                || key_metadata.mode() & 0o077 != 0
            {
                return Err(Failure::new("release key must be a private regular file"));
            }
            let secret = SecretKey::from_file(&key_file, Some(password))
                .map_err(|_| Failure::new("cannot decrypt release key"))?;
            let directory = if let Some(bundle) = &args.bundle {
                if !bundle.join("manifest.json").is_file() {
                    return Err(Failure::new("missing_file: manifest.json"));
                }
                bundle.as_path()
            } else {
                let out = args.out.as_deref().unwrap();
                stage_artifacts(out, &args.artifacts, &key_metadata)?;
                out
            };
            sign_directory(
                directory,
                args.version.as_deref().unwrap(),
                &secret,
                &key_metadata,
            )?;
            println!("signed tessera {}", args.version.as_deref().unwrap());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::TempDir;

    #[test]
    fn arguments_are_explicit_and_reject_conflicting_modes() {
        for (action, args) in [
            (Action::Sign, vec![]),
            (Action::Sign, vec!["--version", "1", "--out", "out"]),
            (
                Action::Sign,
                vec![
                    "--version",
                    "1",
                    "--out",
                    "out",
                    "--bundle",
                    "bundle",
                    "a.deb",
                ],
            ),
            (
                Action::Sign,
                vec!["--version", "../bad", "--bundle", "bundle"],
            ),
            (Action::Verify, vec!["--dir", "out"]),
            (Action::Keygen, vec!["--out", "one", "--out", "two"]),
        ] {
            assert!(
                parse(
                    &action,
                    &args.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                    Path::new("/")
                )
                .is_err()
            );
        }
        assert!(
            parse(
                &Action::Sign,
                &["--version", "0.2.1-2", "--bundle", "bundle"].map(str::to_owned),
                Path::new("/")
            )
            .is_ok()
        );
    }

    #[test]
    fn owner_keygen_modes_and_ephemeral_signing_round_trip() {
        let temp = TempDir::new("tessera-release-fixture").unwrap();
        let dir = temp.path().join("test-key");
        let public = keygen(&dir, "ephemeral test password".into()).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o7777, 0o700);
        assert_eq!(
            fs::metadata(dir.join("tessera-release.key"))
                .unwrap()
                .mode()
                & 0o7777,
            0o600
        );
        assert_eq!(
            fs::metadata(dir.join("tessera-release.pub"))
                .unwrap()
                .mode()
                & 0o7777,
            0o644
        );
        assert_eq!(
            public_key(&dir.join("tessera-release.pub")).unwrap(),
            public
        );
        assert!(keygen(&dir, "ephemeral test password".into()).is_err());
        assert!(keygen(&temp.path().join("empty-password"), String::new()).is_err());
        let key_file = dir.join("tessera-release.key");
        let secret =
            SecretKey::from_file(&key_file, Some("ephemeral test password".into())).unwrap();
        let key_metadata = fs::metadata(&key_file).unwrap();
        let package = temp.path().join("package.deb");
        fs::write(&package, "ephemeral artifact").unwrap();
        let out = temp.path().join("signed");
        stage_artifacts(&out, &[package], &key_metadata).unwrap();
        sign_directory(&out, "0.2.1-2", &secret, &key_metadata).unwrap();
        verify::verify_directory(&out, std::slice::from_ref(&public), Some("0.2.1-2")).unwrap();
        assert!(out.join("rustc-version.txt").is_file());
        let bytes = fs::read(out.join("SHA256SUMS")).unwrap();
        let sig = minisign::SignatureBox::from_file(out.join("SHA256SUMS.minisig")).unwrap();
        assert!(sig.is_prehashed());
        minisign::verify(
            &PublicKey::from_base64(&public).unwrap(),
            &sig,
            Cursor::new(bytes),
            true,
            false,
            false,
        )
        .unwrap();
        assert!(stage_artifacts(&out, &[], &key_metadata).is_err());
        let leaked = out.join("leaked.deb");
        fs::hard_link(&key_file, &leaked).unwrap();
        assert!(sign_directory(&out, "0.2.1-2", &secret, &key_metadata).is_err());
        fs::remove_file(leaked).unwrap();
        fs::write(
            out.join("manifest.json"),
            r#"{"name":"tessera","version":"other"}"#,
        )
        .unwrap();
        assert!(bundle_version(&out, "0.2.1-2").is_err());
    }

    #[test]
    fn signed_bundle_round_trip_binds_the_actual_manifest_version() {
        let temp = TempDir::new("tessera-signed-bundle").unwrap();
        let bundle = temp.path().join("bundle");
        fs::create_dir_all(bundle.join("debs")).unwrap();
        let mut manifest: tessera_bundle::manifest::Manifest = serde_json::from_str(include_str!(
            "../../crates/tessera-bundle/tests/golden/manifest.json"
        ))
        .unwrap();
        for deb in &mut manifest.debs {
            let path = bundle.join(&deb.file);
            fs::write(&path, &deb.package).unwrap();
            deb.sha256 = verify::digest(&mut File::open(path).unwrap()).unwrap();
        }
        crate::build_bundle::write_manifests(&bundle, &manifest).unwrap();
        // Ephemeral private key stays in memory. Metadata is a separate dummy
        // file used only to exercise the signing-input exclusion boundary.
        let key = KeyPair::generate_unencrypted_keypair().unwrap();
        let marker = temp.path().join("dummy-key-metadata");
        fs::write(&marker, "fixture").unwrap();
        let metadata = fs::metadata(marker).unwrap();
        sign_directory(&bundle, &manifest.version, &key.sk, &metadata).unwrap();
        let paths = tessera_bundle::Paths {
            root: "/".into(),
            bundle: bundle.clone(),
            os_release: "unused".into(),
            uid: 1000,
        };
        let options = tessera_bundle::parse(&["verify".into()]).unwrap();
        tessera_bundle::run_with_keys(
            &options,
            &paths,
            &mut tessera_bundle::Native,
            &[key.pk.to_base64()],
        )
        .unwrap();
        assert_eq!(
            bundle_version(&bundle, "0.2.1-9").unwrap_err().message,
            "wrong_version"
        );
        assert!(sign_directory(&bundle, "0.2.1-9", &key.sk, &metadata).is_err());
    }
}
