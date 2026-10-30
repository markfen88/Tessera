//! Bounded, side-effect-free probes of pinned OWN executable images. No
//! compatibility-runtime image is read or executed by this check.
use super::*;
use std::os::fd::AsRawFd;
use std::process::Stdio;

const LIMIT: u64 = 4096;

pub(super) async fn check(roles: &launch::RoleTable) -> Result<(), CommandError> {
    let source = tessera_control_types::RUNTIME_BUILD_ID
        .filter(|id| valid_source(id))
        .ok_or_else(|| unavailable("daemon has no runtime integration build identity"))?;
    let images = roles
        .own_build_images()
        .ok_or_else(|| unavailable("own runtime integration executables are missing"))?;
    for (role, image) in images {
        let pinned = image
            .try_clone()
            .map_err(|_| unavailable("cannot retain own build image"))?;
        tokio::task::spawn_blocking(move || supports_probe(&pinned))
            .await
            .map_err(|_| unavailable("own build image inspection stopped"))??;
        // exec resolves this inherited O_PATH descriptor before CLOEXEC closes
        // it. Renaming/replacing the installed path cannot redirect this probe.
        let mut command =
            tokio::process::Command::new(format!("/proc/self/fd/{}", image.as_raw_fd()));
        command
            .arg("--runtime-build-id")
            .env_clear()
            .env("LC_ALL", "C");
        let bytes = probe(command).await?;
        matching(&bytes, role, tessera_control_types::BUILD_VERSION, source)?;
    }
    Ok(())
}

/// Inspect only our own pinned ELF images for the probe contract marker. This
/// prevents an older launcher interpreting an unknown flag as a device key.
/// The finite file walk checks time around reads; it cannot interrupt a stalled
/// filesystem syscall (the same limitation as the role metadata collector).
fn supports_probe(image: &std::fs::File) -> Result<(), CommandError> {
    let start = Instant::now();
    let mut file = std::fs::File::open(format!("/proc/self/fd/{}", image.as_raw_fd()))
        .map_err(|_| unavailable("cannot inspect own build image"))?;
    let marker = tessera_control_types::RUNTIME_BUILD_PROBE_MARKER.as_bytes();
    let mut buffer = [0_u8; 64 * 1024];
    let mut tail = Vec::new();
    let mut first = true;
    loop {
        if start.elapsed() > Duration::from_secs(2) {
            return Err(unavailable("own build image inspection deadline"));
        }
        let size = std::io::Read::read(&mut file, &mut buffer)
            .map_err(|_| unavailable("own build image read failed"))?;
        if start.elapsed() > Duration::from_secs(2) {
            return Err(unavailable("own build image inspection deadline"));
        }
        if size == 0 || (first && !buffer[..size].starts_with(b"\x7fELF")) {
            return Err(unavailable(
                "own executable lacks the safe runtime build probe; update the package",
            ));
        }
        first = false;
        tail.extend_from_slice(&buffer[..size]);
        if tail.windows(marker.len()).any(|part| part == marker) {
            return Ok(());
        }
        let keep = tail.len().saturating_sub(marker.len() - 1);
        tail.drain(..keep);
    }
}

fn valid_source(source: &str) -> bool {
    source.len() == 64
        && source
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn matching(bytes: &[u8], role: &str, version: &str, source: &str) -> Result<(), CommandError> {
    let expected = serde_json::json!({ "contract": 1, "probe": tessera_control_types::RUNTIME_BUILD_PROBE_MARKER,
            "role": role, "version": version, "source": source });
    if !valid_source(source)
        || serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .as_ref()
            != Some(&expected)
    {
        return Err(unavailable(
            "runtime integration executables have different or unqualified builds",
        ));
    }
    Ok(())
}

async fn probe(mut command: tokio::process::Command) -> Result<Vec<u8>, CommandError> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| unavailable("cannot probe own runtime integration executable"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| unavailable("runtime build probe output unavailable"))?
        .take(LIMIT + 1);
    let mut bytes = Vec::new();
    let status = tokio::time::timeout(Duration::from_secs(2), async {
        stdout.read_to_end(&mut bytes).await?;
        child.wait().await
    })
    .await
    .map_err(|_| unavailable("runtime build probe deadline"))?
    .map_err(|_| unavailable("runtime build probe failed"))?;
    if !status.success() || bytes.len() as u64 > LIMIT {
        return Err(unavailable("runtime build probe refused"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires freshly built own CLI roles; run by paired integration validation"]
    async fn pinned_own_builds_match_and_replacement_requires_new_registration() {
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory = PathBuf::from(std::env::var_os("TESSERA_TEST_RUNTIME_BINS").unwrap());
        let scratch = Scratch(
            std::env::temp_dir().join(format!("tessera-build-images-{}", std::process::id())),
        );
        std::fs::create_dir(&scratch.0).unwrap();
        // Model installed permissions, not the build directory's umask. The
        // production lookup must continue refusing group-writable images.
        let [launcher, adapter, cli] = [
            "tessera-controller-start",
            "tessera-controller-command",
            "tesseractl",
        ]
        .map(|name| {
            let image = scratch.0.join(name);
            std::fs::copy(directory.join(name), &image).unwrap();
            std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o755)).unwrap();
            image
        });
        let runtime = scratch.0.join("metadata-only-runtime-fixture");
        std::fs::write(
            &runtime,
            "own metadata fixture, deliberately not an executable image\n",
        )
        .unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
        let roles =
            launch::RoleTable::fixture([&launcher, &runtime, &adapter, &cli], false).unwrap();
        check(&roles).await.unwrap();
        // The pinned descriptors still execute the original own CLI after an
        // installed path is replaced. A new callback table cannot join it.
        std::fs::remove_file(&cli).unwrap();
        std::fs::copy(&adapter, &cli).unwrap();
        check(&roles).await.unwrap();
        let replaced =
            launch::RoleTable::fixture([&launcher, &runtime, &adapter, &cli], false).unwrap();
        assert!(!roles.same_images(&replaced));
        assert!(check(&replaced).await.is_err());
        // A legacy launcher/script with no probe contract is not executed.
        assert!(supports_probe(&std::fs::File::open(&runtime).unwrap()).is_err());
    }

    #[test]
    fn mixed_builds_roles_and_unqualified_versions_refuse() {
        let source = "a".repeat(64);
        let good = serde_json::json!({ "contract": 1, "probe": tessera_control_types::RUNTIME_BUILD_PROBE_MARKER,
            "role": "cli", "version": "dev-snapshot", "source": source });
        assert!(
            matching(
                &serde_json::to_vec(&good).unwrap(),
                "cli",
                "dev-snapshot",
                &source
            )
            .is_ok()
        );
        for (key, value) in [
            ("role", serde_json::json!("report-adapter")),
            ("version", serde_json::json!("another-build")),
            ("source", serde_json::json!("b".repeat(64))),
            ("contract", serde_json::json!(0)),
            ("probe", serde_json::json!("old-or-unknown-probe")),
        ] {
            let mut bad = good.clone();
            bad[key] = value;
            assert!(
                matching(
                    &serde_json::to_vec(&bad).unwrap(),
                    "cli",
                    "dev-snapshot",
                    &source
                )
                .is_err()
            );
        }
        for bad in [b"0.4.1".as_slice(), b"{}", b"", b"{\"source\":null}"] {
            assert!(matching(bad, "cli", "dev-snapshot", &source).is_err());
        }
        assert!(!valid_source("0.4.1"));
        assert!(!valid_source(&"A".repeat(64)));
    }

    #[tokio::test]
    async fn own_probe_failure_size_and_deadline_are_bounded() {
        for script in ["exit 1", "head -c 4097 /dev/zero", "exec sleep 10"] {
            let mut command = tokio::process::Command::new("/bin/sh");
            command.args(["-c", script]);
            assert!(probe(command).await.is_err());
        }
    }
}
