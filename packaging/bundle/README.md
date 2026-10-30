# tessera add-on bundle

For Debian Trixie, native amd64 or arm64, systemd >= 253. Includes tessera
and tessera-alsa, with the ALSA bridge enabled, Controller compatibility,
and the web portal. No PipeWire client. The initial portal password is
`tessera`; existing passwords are preserved. The portal includes a Services page.
An archive marked `ubuntu-noble-experimental` instead requires Ubuntu 24.04
Noble arm64 and explicit `--allow-ubuntu-24.04` on install. It cannot be used
on Debian. Live audio interoperability and hardware bridge behavior on Ubuntu
are unverified. Normal Debian bundles cannot be installed on Ubuntu.
Authenticate the release SHA256SUMS with minisign before executing a downloaded
installer, then check the archive hash. The internal manifest is signed too.
Before running as root, extract into a root-owned 0700 directory (docs/bundle.md).
This archive holds one installer program, `tessera-bundle`; the `install.sh` and
`uninstall.sh` scripts of 0.2.x are gone and the options are the same.

    sudo ./tessera-bundle verify
    sudo ./tessera-bundle install --profile speaker --audio-card hw:CARD=MyDAC --yes
    sudo ./tessera-bundle uninstall --purge --yes

Use `--profile ndimon` for NDIMon-R >= 1.3.0 (two receive and two transmit
channels at 48 kHz, 24-bit, device advertisement enabled by default). The plugin owns
`61-ndimon-flows.toml`. Speaker has two receive channels and no transmit channels.
It includes the CamillaDSP stage (`tessera-dsp`: CamillaDSP, its GUI and the portal's
Speaker page; docs/dsp.md) by default; `--no-dsp` installs tessera only, with a
playback bridge instead. A missing card warns and retries. The default audio
card is `default`; prefer an explicit `hw:CARD=...` selector for the intended DAC.
`--profile none` installs without profile configuration. Online mode never runs
`apt-get update`; refresh lists separately if needed.

Use `--admin-user USER` repeatedly to grant socket/admin access. Log in again
for membership changes. Existing profile files and unit drop-ins are kept.
Re-run with the same profile. To change profiles remove the old bundle first.
Upgrades keep a running daemon on its current process; `--restart` deliberately
restarts it and interrupts audio. `--dry-run` validates and prints the plan;
`--offline` requires all dependencies already installed, and prohibits downloads.

The record `/var/lib/tessera-bundle/record.json` preserves files and account
changes across reruns. A failed install after package installation begins leaves
an incomplete record and installed packages for diagnosis/retry. Uninstall
removes recorded files, keeps package config/state unless `--purge`, and removes
only recorded account changes on purge. `--keep-users` retains those accounts
and memberships. A missing record causes conservative package-only removal.
Removing additional dependent packages requires `--remove-dependents` after
reviewing the simulated plan; `--yes` alone never grants that consent.

`verify` authenticates `SHA256SUMS.minisig` (prehashed minisign ED), checks its
trusted comment for product `tessera`, the exact bundle version and `SHA256SUMS`,
then checks both manifests and every payload file. Unsafe paths, links, duplicate
checksum records, missing/unlisted files and mismatches are refused before apt.
It changes no host state. `install` requires the same verification by default.

The installer carries the project's release key (`packaging/tessera-release.pub`,
key id `4E330826A26C2812`, from 0.5.2). `/etc/tessera/trusted-release-keys` can add bare
public keys, one per line; it must be a root-owned regular file, exactly 0644,
under root-owned parents without group/world write access or symlinks. A bundle
that no trusted key vouches for is refused with `untrusted_key` and nothing is
changed. For unsigned **development builds only**,
explicitly use `install --allow-unsigned`; integrity and lifecycle checks still
apply. `verify` deliberately refuses an unsigned/untrusted bundle.

Manual verification, using a public key obtained through a trusted channel:

```sh
minisign -Vm SHA256SUMS -p /trusted/tessera-release.pub -x SHA256SUMS.minisig
sha256sum --check --strict SHA256SUMS
```

Run the hash check only after signature verification succeeds, and check the
trusted comment names tessera, the intended version and SHA256SUMS. Verify the
outer archive before extraction/execution, then its internal manifest. The
Rust verifier enforces the authenticated comment automatically. Extract only
into a root-owned 0700 directory and keep it private throughout installation.

Release tooling: `cargo xtask release --version V --bundle EXTRACTED_BUNDLE`
signs the internal list; re-archive it, then use `cargo xtask release --version V
--out NEW_DIRECTORY ARTIFACT...` for the outer bundle/packages. Signing requires
`TESSERA_RELEASE_KEY_FILE` and `TESSERA_RELEASE_KEY_PASSWORD` in the environment.
`cargo xtask verify-release --version V --dir DIRECTORY --public-key FILE`
checks either layer without a secret. `cargo xtask release-keygen --out DIR` is
an owner-only procedure requiring a nonempty environment password: it creates
an encrypted 0600 secret key and 0644 public key under a fresh 0700 directory,
and prints only the public key. No project key is built in.

Rotate by shipping the new public key alongside the old in a release signed
by the old key, confirming device adoption, then changing the signer and only
later retiring the old key. Override keys are additive and retire separately.
The owner decides offline key custody, the CI secret location and access to its
password and the signing job. See packaging/README.md in the source ("Signed
releases") for the full owner procedure. There is no automated publication step.
