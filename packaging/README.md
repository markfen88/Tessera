# Packaging

`cargo xtask build-deb` builds the Debian packages from the tree with Rust and
`dpkg-deb`, without debhelper, fakeroot or cargo-deb.

| Package | Contents |
|---|---|
| `tessera` | `tesserad`, `tesseractl`, the `tesserad.service` unit, sysusers and tmpfiles entries, the sysctl socket-buffer ceilings (`/usr/lib/sysctl.d/60-tessera.conf`, applied by the postinst with `systemd-sysctl`), the reference `/etc/tessera/config.toml` (the only conffile; every key commented out, see below), its copy `/usr/share/doc/tessera/examples/config.toml`, docs, and (native builds only) shell completions and man pages |
| `tessera-alsa` | the ALSA plugin (`libasound_module_pcm_tessera.so`) and the `pcm.tessera` / `pcm.tessera_plug` snippet |
| `tessera-web` | the web portal, default Services page, initial `tessera` password, generated service-control policy and its unit; the helper units, and the sound-card refresh (`tessera-web-sound.service`, started by `60-tessera-web-sound.rules`: a card that appears after the portal started restarts the portal, see `docs/web-portal.md`) |
| `tessera-dsp` | CamillaDSP and the CamillaGUI backend, the unmodified upstream releases pinned by SHA-256 in `packaging/dsp/dsp.lock` (amd64 and arm64 only, built when the ALSA plugin is): the programs in `/usr/lib/tessera-dsp`, the `camilladsp` and `camillagui` units (enabled, not started), the `camilla` account and its data directories (sysusers and tmpfiles), the two GUI conffiles in `/etc/camilladsp`; see `docs/dsp.md` |
| `tessera-pw` | the PipeWire client and its user unit (native builds only: `libspa-sys` runs bindgen on host headers) |

```sh
cargo xtask build-deb                                    # native build
TARGET=aarch64-unknown-linux-gnu cargo xtask build-deb   # cross build
```

Cross builds need the target linker (`CARGO_TARGET_<TRIPLE>_LINKER`) and, when the daemon is built with the sound-card bridge or the plugin is packaged, the target's `libasound2-dev` with `PKG_CONFIG_ALLOW_CROSS=1` and `PKG_CONFIG_PATH`. `Depends` of a cross build is written without `dpkg-shlibdeps` (there is no installed target library to ask): `libc6, libgcc-s1` plus, for every file whose ELF dynamic section needs a library listed in `LIBRARY_PACKAGES` (`xtask/src/build_deb.rs`), the package a native build of Debian 13 or Ubuntu 24.04 names for it (`libasound2t64`; `libasound2` is only a virtual package there, which `dpkg-query` does not see), with no minimum version except the glibc floor of the programs (`libc6 (>= 2.39)`, read from their `GLIBC_x.y` symbols). `check-deb` reads the `NEEDED` libraries of every ELF object in a package: one that a shared object beside the program provides is satisfied (the `tessera-dsp` GUI is a PyInstaller tree with OpenSSL, zlib, libffi, readline and the rest in its `_internal` directory, so they need no `Depends`); any other must be in `LIBRARY_PACKAGES` and its package in `Depends`, or the check fails and says to name it there. It also requires `libc6 (>= v)` to cover the highest glibc symbol of the package, fails a package that names a virtual package, and fails packages of one name for two architectures whose dependency names differ. The build raises the `libc6` floor of `tessera-dsp` over the vendored GUI tree, which `dpkg-shlibdeps` is not asked about.

Environment (all optional):

* `TARGET` Rust target triple; `SKIP_PW=1` no `tessera-pw`; `SKIP_ALSA=1` no plugin / `tessera-alsa`; `SKIP_DSP=1` no `tessera-dsp`; `NO_BUILD=1` assemble from existing binaries;
* `TESSERA_DSP_CACHE` where the pinned CamillaDSP archives are cached and verified (default `~/.cache/tessera-dsp`); the build downloads them over HTTPS from `github.com` and stops unless each SHA-256 equals its pin in `packaging/dsp/dsp.lock`;
* `TESSERA_DAEMON_FEATURES` comma-separated cargo features of `tesserad`, for example `alsa-bridge` (the headless sound-card bridge, see `docs/user-guide.md`);
* `TESSERA_DEB_REVISION` Debian revision (default: commit count); `SOURCE_DATE_EPOCH` timestamp for reproducible output; `SYSTEMD_MIN` minimum systemd in `Depends` (default 253).

Output: `target/deb/*.deb` and `SHA256SUMS`. Check: `cargo xtask check-deb` (lint, unit verification, sysusers parse).

The service does not get `SupplementaryGroups=audio`; a bridge that opens `/dev/snd` needs it as a drop-in (user guide).

Tmpfiles alone creates the daemon and Controller state/runtime directories.
With `ProtectSystem=strict`, `ReadWritePaths` permits daemon writes only to
`/var/lib/tessera` and `/run/tessera`, and Controller writes to
`/var/lib/tessera-controller` and socket access through `/run/tessera`.
The Controller keeps daemon state read-only and waits in the launcher for its
first persisted identity. Custom daemon state/runtime paths need corresponding
administrator-created directories and writable exceptions in a unit drop-in.
The portal already permits both control-socket runtime paths; its optional
appliance file writers require their documented per-feature drop-ins. The
unitfile helper writes through PID 1 over D-Bus and needs no writable path.
Network/recovery units have no read-only filesystem sandbox; migration units
and the PipeWire user unit likewise need no such exception.

## The reference configuration

`/etc/tessera/config.toml` is the full reference: every key of the configuration schema, commented out at its default, each with a short what-and-why, its unit, range, apply class and restart scope. Nothing in it is active, so the daemon behaves as if the file were empty. The header tells the operator to put changes in `/etc/tessera/config.d/*.toml` and to leave this file alone.

To read the reference of the version that is installed, run `tesserad --print-default-config`: it prints the same text, reads no file and needs no privileges (the header of the file names it). A copy is installed as `/usr/share/doc/tessera/examples/config.toml`, next to the prose reference `/usr/share/doc/tessera/configuration.md` (`docs/configuration.md` in the sources), but minimised images (containers, some cloud images) exclude `/usr/share/doc` with a dpkg `path-exclude` rule, and then neither is installed. `/etc/tessera/config.toml` itself is a conffile and is always there (as the package shipped it, or as the administrator changed it); `dist/etc/tessera/config.toml` in the sources is the same file.

The file is **generated** from the schema in `crates/tessera-state/src/schema.rs`, not written by hand. `dist/etc/tessera/config.toml` is the one committed output; `cargo xtask build-deb` installs that same file twice, as `/etc/tessera/config.toml` and as `/usr/share/doc/tessera/examples/config.toml` (there is no second copy in the tree), so a build does not depend on a generator run. These keep it equal to the schema:

* `cargo xtask build-deb` regenerates the file from the workspace (`cargo run --release --locked -p tessera-state --example reference_config`: a host build, also in a cross build, that needs no network once the dependencies are in the cargo cache, as after the builds before it; `CARGO_NET_OFFLINE=true` enforces that) and stops with the same message if `dist/` differs (skipped with `NO_BUILD=1`);
* `cargo xtask check-deb` checks the built package: the conffile exists (a missing one is a failure), the examples copy is identical and the file contains only comments.

After a change to the schema (a key, a default, a range or a comment), regenerate both generated files from the workspace root and commit them:

```sh
cargo run -q -p tessera-state --example reference_config > dist/etc/tessera/config.toml
# the tables in docs/configuration.md between the schema:begin / schema:end markers:
cargo run -q -p tessera-state --example reference_config -- --tables   # paste over the block
```

### Upgrading: the conffile

`/etc/tessera/config.toml` is a dpkg conffile, so dpkg decides on every upgrade that ships a different file:

* **The file was never edited** (the intended case: edits belong in `config.d/`): dpkg replaces it with the new reference without asking, whatever `--force-conf*` option is given.
* **The file was edited** and the package changes it: dpkg asks (interactive), or follows the option given. **Before you take the new version, move your settings into a file in `config.d/` (for example `50-site.toml`)**; then the answer is harmless: take the new reference, or keep your file. The options: `--force-confold` keeps the administrator's file (dpkg normally leaves the packaged one next to it as `config.toml.dpkg-dist`), `--force-confnew` takes the new reference (the old file is kept as `config.toml.dpkg-old`), `--force-confdef` takes dpkg's default action where it has one and otherwise asks, unless `--force-confnew` or `--force-confold` is also given, which then decides; `--force-confask` always asks (it does not answer silently). For unattended upgrades use `--force-confdef --force-confold`: the administrator's edits keep working because no key is ever renamed. A kept, outdated file is harmless, since the reference is only comments; to see what is new, run `tesserad --print-default-config` (or compare with `/usr/share/doc/tessera/examples/config.toml`, where that directory is installed).
* `config.d/` is not a conffile of the package: dpkg never touches it.

**From a package with the old, active sample.** Earlier packages shipped an active `[[device]] id = "main"` (and other keys at their defaults) in `config.toml`. Every key in it was a default, so an unmodified file upgraded to the reference changes nothing, with one exception: the implicit device `main` (the device the daemon creates when no file defines one) exists only while no file defines a `[[device]]`. A host whose drop-in adds another device (say `id = "b"`) and relied on the active `main` of the old sample has only `b` after the upgrade, and the default name of the remaining device changes from `<host>-b` to `<host>` (a device name is visible on the network). Such a drop-in should define `main` too: add a `[[device]] id = "main"` entry (the entry may be just that line). The package announces this in `NEWS.Debian` (`/usr/share/doc/tessera/NEWS.Debian.gz`, source `packaging/NEWS.Debian`; `apt-listchanges` shows it before the upgrade) and the user guide (section 8) repeats it. The file holds one complete entry per release that needs one, newest first, each with its own version and date, copied as they are (an entry stamped with each build's version would be shown again on every upgrade). The first line of an entry names the package version that introduces the change (builds use the workspace version plus `TESSERA_DEB_REVISION`, default: commit count). `cargo xtask build-deb` stops if the newest entry's version is higher than the one being built. `apt-listchanges` shows only entries newer than the version installed on the host (a host on `0.1.0-4` sees nothing from an entry for `0.2.0~beta-1`), so a release adds its entry above the older ones, with the release version, above what the hosts run and not above the version built, and a date no later than the release commit.

Unknown keys are errors: a daemon older than a key refuses a configuration that uses it, so upgrade the packages before using a new key (a downgrade with a new-key drop-in fails to start the same way).

## Versioning

Controller compatibility is a required core runtime. The package builds the
existing compatibility runtime from this source tree without changing its
protocol implementation, installs it under `/usr/lib/tessera`, and enables
`tessera-controller@main` with the daemon. A host can instead use the daemon's
opt-in native device role (docs/interoperability.md, "Native device role"); the
packages do not switch a host and leave a disabled runtime unit disabled.
The Rust launcher reads the daemon's current wired interface,
IPv4 address, device name and persisted identity at startup; no operator
username or installation-machine address is compiled in. Device advertisement
is enabled by default. Keep the runtime source when assembling source releases.
The existing runtime serves only the first logical device's base ports;
additional logical devices' offset control ports are not yet supported.
The default `@main` instance follows the first device if there is no `main` key.
Existing operator-disabled units are not re-enabled on upgrade. A pre-existing
enabled device-key compatibility instance is retained rather than starting a
duplicate. Older manual templates used a Unix username as the instance; their
enabled instances migrate to `@main` for the next deliberate daemon restart.
The running legacy helper is left running until that restart.
If systemd also restarts a retired instance through `PartOf=`, the launcher
exits with status 78 for its unknown device key and the unit does not retry.
A template in `/etc/systemd/system` overrides the package; installation warns
and preserves it without enabling a duplicate. Review/remove that local
override before switching to the packaged `@main` service. Enabled legacy
instances are detected in every system target's wants directory.

The compatibility unit uses an unprivileged command adapter in its private
PATH. Its historical `sudo tee` argv now submits `SetProtocolFlows` over
`control.sock` with protocol origin; `sudo systemctl reload tesserad` succeeds
as a deprecated no-op. Inspect effective runtime flows with `tesseractl flows
show` and use `flows set|clear` for explicit changes. Flows persist in
`state.toml`. The runtime's empty report at its start (it restarts with the daemon) does
not replace the stored set: the adapter applies an empty report only from a run that
has applied one (`/var/lib/tessera-controller/flows-run`, docs/control-interface.md).
Existing `90-probe-flows.toml` is a deprecated fallback with a warning (still read
in 0.5.0; removal is deferred), and any stored set (including an explicit clear)
supersedes it. Packages neither create that file nor grant it a writable unit
path. Local `tesseractl` commands still run as the service user. No sudoers grant
or privilege elevation is installed. The unit retains its daemon-admin group;
the daemon's persisted state is mounted read-only to the compatibility process.

The web package initializes a password hash for `tessera` only when the password
file is absent, enables/starts a fresh portal, and installs the generated polkit
allowlist for the Services page. Existing passwords and explicit service lists
are retained. Bundles include the web package as well as core and ALSA packages.
The generated polkit file is package-managed and replaced on configure;
edit the service list and regenerate policy rather than editing that file.
`/etc/polkit-1/rules.d` is polkitd's (0750 root:polkitd) and no file of ours declares
it. The one exception: 0.2.x packages declared it in their tmpfiles file and left it
0755, so configure (and the bundle installer) put a directory found at exactly 0755 back
to 0750 with group `polkitd`, where that group exists; a mode an administrator chose,
and a host without the group, are left as found.
Invalid portal configuration or password initialization errors fail package
configuration explicitly rather than starting a portal with incomplete setup.

Debian release versions are `<workspace version>-<first-parent count>`.
Commits in origin/main history count `git rev-list --count --first-parent HEAD`;
unmerged previews count their merge base with origin/main and append `~g<sha7>`
so a squash merge cannot make the release older than its preview.
`TESSERA_DEB_REVISION` overrides the counter; shallow checkouts require that
override or full history. `TESSERA_RELEASE=1` requires HEAD in origin/main history.
Without the origin remote or origin/main ref, the build exits 2 and asks for
`TESSERA_DEB_REVISION` explicitly. A source tarball has no commit identity and
uses that explicit revision without a Git suffix.
`cargo xtask build-deb --print-version` prints the version without building.
Release tags use `v<version>`, for example `v0.3.0`. The reference configuration contains no version.

## Script policy

`cargo xtask check-no-scripts` fails on tracked or untracked, non-ignored
`.sh`, `.py` and handwritten `.js`/`.cjs`/`.mjs` files, and on files whose first line selects a shell, including
`env -S` and BusyBox shell invocations. Generated build outputs remain ignored
by Git. The one committed browser asset `crates/tessera-web/assets/generated/live.js`
is generated from TypeScript and checked for drift by `cargo xtask web-assets
--check`; Rust packaging embeds it without Node. The shell allowlist in
`xtask/src/no_scripts.rs` is **empty**. Any
future exception must identify an exact path and the external requirement
that makes shell unavoidable; migration work is not an exception.

## Native Debian lifecycle hooks

The package payload declares accounts in `sysusers.d`, directories in
`tmpfiles.d`, services in units and default enablement in `system-preset`.
Debian's `deb-systemd-helper` retains administrator-disabled state and the
Controller template's `DefaultInstance=main`; configure never starts or restarts
the core daemon. Fresh portal startup and upgrade restart behavior are retained.

Non-declarative work is implemented in `crates/tessera-package`. The installed
`tessera-package configure-core|configure-web` commands are Rust one-shot
migration entry points, also exposed by the static `Type=oneshot` units
`tessera-package-migrate.service` and `tessera-web-package-migrate.service`.
Configure hooks call the shared Rust implementation directly; these units are
not enabled at boot. The same compiled executable is copied into the dpkg
control archive as the lifecycle hooks, with dispatch through dpkg's package,
hook-name and action arguments:

| Package | Native control hooks | Why they remain |
|---|---|---|
| `tessera` | `preinst`, `postinst`, `prerm`, `postrm` | Capture legacy enabled instances before unpack, apply sysusers/tmpfiles and helper enablement, stop only on removal, purge configuration/state after payload removal. |
| `tessera-web` | `postinst`, `prerm`, `postrm` | Initialize an absent password, generate the service policy, retain portal startup/restart semantics, recover networking before removal, and purge the password/policy. |
| `tessera-alsa`, `tessera-pw` | none | Payload and unit/configuration files only. |

These hooks are native ELF binaries, with no shebang or interpreter. No
handwritten maintainer scripts are shipped. Neither debhelper nor cargo-deb is
invoked, so neither emits shell hooks. Retaining native preinst/postrm hooks
preserves behavior that a post-unpack service alone cannot provide. Hook copies
increase control-archive size and need native-architecture lifecycle validation.
The sysusers accounts remain on package purge; bundle purge reverses only the
account changes recorded as bundle-owned. NEWS stays the unchanged, versioned
`NEWS.Debian.gz` payload for apt-listchanges.

Core purge removes generated `/var/lib/tessera`, `/var/lib/tessera-controller`,
`/var/lib/tessera-package` and `/run/tessera`, plus core configuration. Web
purge recovers networking after stopping its helper/recovery units, then
removes `/var/lib/tessera-network` and `/run/tessera-network` along with portal
configuration/policy. Removal and purge also unload what the packages leave in the
running system: `tessera-web` deletes the nft table `inet tessera_network` and stops
`tesseraaudio.slice` and the slice of the service-switch helper jobs
(`system-tessera\x2dweb\x2dunitfile.slice`); `tessera` stops `tesseraaudio.slice` and the
controller template's slice
and writes the socket-buffer ceilings back to the values `sysctl-previous` in
`/var/lib/tessera-package` recorded when the first configure raised them (only a
ceiling that still has the raised value). The configure hook runs
`systemd-sysctl` by its absolute path (`/usr/lib/systemd/systemd-sysctl`, else
`/lib/systemd/systemd-sysctl`): it is on no `bin` directory of dpkg's `PATH`, and by its bare name the
call silently did nothing, so no record was written and a purge left the raised values. The network files the portal committed
(Netplan or networkd definitions, VLAN links, `/etc/modules-load.d/tessera-8021q.conf`)
stay on purpose; they are the host's addressing. Bundle `uninstall --purge` also removes these generated
trees left by older package hooks; a separately retained portal keeps its
network journal/runtime. Ordinary removal retains persistent state.

## Signed releases

A release carries the packages/bundle archives, `SHA256SUMS` and
`SHA256SUMS.minisig`. Signatures use minisign's prehashed Ed25519 **ED** variant.
The authenticated trusted comment is exactly
`product:tessera<TAB>version:VERSION<TAB>file:SHA256SUMS`, with literal tabs.
Versions include the Debian revision where the bundle/package version does.
NDIMon-R independently implements the same comment vocabulary and task names;
the implementations share no code.

The compiled `tessera_bundle::release::TRUSTED_KEYS` list is **empty**: no
project signing key is built in. First trust requires the project owner to
provision public keys through a trusted administrative channel, or to compile
them into a later release. An optional `/etc/tessera/trusted-release-keys`
adds keys, one bare minisign public key per line (no `.pub` comment header).
It must be a root-owned regular file with mode **0644**. All parent directories
must be root-owned and not group/world-writable; symlinks are refused throughout.
The override cannot revoke a compiled key. Never trust a key merely because it
arrived with the download it claims to authenticate.

The project owner decides **who holds the offline encrypted backup, where the CI
secret is held, who knows its password, and who may change signing jobs**. The
commands below are owner procedures for a trusted machine, never for an unattended
or automated run. Keep signing material outside the checkout and release inputs.
Do not enable shell tracing or print environment values. On the owner's chosen
private machine:

```sh
read -rsp 'New release-key password: ' TESSERA_RELEASE_KEY_PASSWORD
export TESSERA_RELEASE_KEY_PASSWORD
cargo xtask release-keygen --out /owner/private/tessera-key
unset TESSERA_RELEASE_KEY_PASSWORD
```

The destination must not exist. Key generation requires a nonempty environment
password and writes encrypted `tessera-release.key` **0600** and
`tessera-release.pub` **0644** in a new **0700** directory, printing only the bare
public key. Existing files/directories are never overwritten. A failed operation
can leave a private partial directory for the owner to inspect/remove. Provision
only that bare public key on devices as the root-owned override file; keep the
normal two-line `.pub` file for operator verification.

On the trusted signing machine or protected private CI job, supply
`TESSERA_RELEASE_KEY_FILE` (path to the private encrypted key) and
`TESSERA_RELEASE_KEY_PASSWORD` through the environment. Signing refuses either
missing/empty input, uses no interactive prompt and never prints their values.
A CI secret should supply the encrypted key through a temporary 0600 file,
removed after the job; the job must never generate a replacement key. Restrict
access to the signing job and release tags under the owner's custody policy.

For explicit, already built `.deb` and `.tar.gz` artifacts:

```sh
export TESSERA_RELEASE_KEY_FILE=/owner/private/tessera-key/tessera-release.key
read -rsp 'Release-key password: ' TESSERA_RELEASE_KEY_PASSWORD
export TESSERA_RELEASE_KEY_PASSWORD
cargo xtask release --version 0.3.0-1 --out /staging/signed /staging/artifacts/*.deb /staging/artifacts/*.tar.gz
unset TESSERA_RELEASE_KEY_FILE TESSERA_RELEASE_KEY_PASSWORD
cargo xtask verify-release --dir /staging/signed --version 0.3.0-1 --public-key /owner/private/tessera-key/tessera-release.pub
```

`--out` must not exist. The task copies only explicit artifacts, records the
exact `rustc -vV` output as `rustc-version.txt`, writes canonical GNU SHA-256
records and signs them. Duplicate/unsafe names, links, existing outputs and
signing-key inputs (including hard links) are refused. Signing never publishes.
`verify-release` requires an expected version and verifies every listed file
and the complete output directory. `--public-key` accepts a normal `.pub` file
or a bare key; without it, compiled/root-owned device trust is used.

An extracted bundle also needs its **internal** checksums signed before it is
archived, because `tessera-bundle install` authenticates that internal manifest:

```sh
# With the two signing environment variables supplied as above:
cargo xtask release --version 0.3.0-1 --bundle /staging/tessera-bundle_0.3.0-1_amd64
cargo xtask verify-release --version 0.3.0-1 --dir /staging/tessera-bundle_0.3.0-1_amd64 --public-key /owner/private/tessera-key/tessera-release.pub
```

Use the extracted tree from `cargo xtask build-bundle`, with no extra files.
The version must match `manifest.json`. Re-archive that signed tree, preserving
its executable modes and root directory name, then pass the archive to the
outer `release --out` command alongside the packages. Both checksum layers
exclude only their own `SHA256SUMS` and `SHA256SUMS.minisig`. Outer assets have
flat basenames; internal bundles retain safe `debs/` and `profiles/` paths.
Files, links or payloads absent from the signed list are refused. Extract into
a root-owned private directory and prevent changes between checking and use.

For independent manual verification, obtain the public key through a trusted
channel and run **before executing a newly downloaded installer**:

```sh
minisign -Vm SHA256SUMS -p /trusted/tessera-release.pub -x SHA256SUMS.minisig
sha256sum --check --strict SHA256SUMS
```

Run the checksum command only after minisign succeeds. Check the authenticated
trusted comment names **tessera**, the exact intended version and **SHA256SUMS**;
the CLI crypto check does not enforce those fields itself. Repeat inside the
extracted bundle for its internal manifest. The Rust verifier enforces the
comment and hashes automatically and reports typed `untrusted_key`,
`bad_signature`, `wrong_product`, `wrong_version`, `missing_file`,
`checksum_mismatch` or `malformed` failures. Empty trust always fails closed.
`tessera-bundle install --allow-unsigned` is a **development-only** opt-in for
today's unsigned builds; it bypasses authentication while retaining checksum,
path, manifest, platform and lifecycle checks. `verify` has no unsigned bypass.

Key rotation: add the new public key alongside the old,
ship that change **signed by the old key**, confirm devices have installed it,
then switch the signer to the new key. Remove the old compiled key only in a
later release; remove old override entries separately. Never sign the only
trust transition solely with a key devices do not yet trust.

None of these tasks uploads or publishes anything: signing and verification
read and write local directories only.
