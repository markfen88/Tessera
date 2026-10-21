# Add-on bundle

The tessera add-on archive installs the daemon and ALSA plugin on Debian 13
Trixie, systemd >= 253, native amd64 or arm64. The daemon includes the ALSA
bridge. Obtain bundles from a trusted GitHub release and verify the archive
against its trusted release SHA256SUMS. Checksums give integrity, not authenticity:
they do not authenticate code downloaded with a substituted checksum file.
Before invoking install.sh as root, extract the trusted bundle into a
root-owned 0700 directory, inaccessible to ordinary users. The NDIMon-R install
hook uses this same boundary; never execute root installer code from a directory
that the daemon or another user can modify.

Ubuntu 24.04 Noble native arm64 is an **experimental**, separate build. Build
on that OS with `packaging/build-bundle.sh --allow-ubuntu-24.04`; its archive
ends in `_arm64_ubuntu-noble-experimental.tar.gz`. Installation requires
`install.sh --allow-ubuntu-24.04` in addition to the normal options. The
installer checks both host and bundle OS; this flag cannot install Debian
binaries on Ubuntu or Ubuntu binaries on Debian. Other Ubuntu versions,
derivatives and Ubuntu amd64 are refused. Dependencies come from the native
Ubuntu packages, rather than the Debian dependency list. This experimental
artifact is excluded by the normal release publisher. The uninstall command
is unchanged and uses its installed record; no opt-in is needed to remove it.
Ubuntu live audio interoperability and hardware bridge behavior are unverified.
The ndimon profile configures Tessera only; it does not upgrade or enable
NDIMon-R integration (which requires NDIMon-R >= 1.3.0).

```sh
sha256sum -c SHA256SUMS
sudo install -d -o root -g root -m 0700 /root/tessera-addon
sudo install -o root -g root -m 0600 tessera-bundle_<version>_<arch>.tar.gz /root/tessera-addon/bundle.tar.gz
sudo tar --no-same-owner --strip-components=1 -xzf /root/tessera-addon/bundle.tar.gz -C /root/tessera-addon
sudo /root/tessera-addon/install.sh --profile speaker --audio-card hw:CARD=MyDAC --yes
sudo /root/tessera-addon/uninstall.sh --purge --yes
```

The bundle has its own SHA256SUMS over every payload file. Installation rejects
corruption before apt. It includes `tessera` and `tessera-alsa` only. Dependencies
are derived from their Debian Depends, plus `avahi-daemon` and `alsa-utils`;
Trixie's systemd and init-system-helpers are platform prerequisites.

`install.sh [--profile speaker|ndimon|none] [--admin-user USER]..
[--audio-card CARD] [--restart] [--reinstall] [--offline] [--dry-run] [--yes]`:

- `speaker`: main device, two receive channels, no transmit channels, 48 kHz,
  playback bridge, and audio supplementary group for the daemon. Prefer an
  explicit `--audio-card hw:CARD=...` selector; the fallback `default` may select
  an unintended ALSA route. A missing card warns, leaves the bridge reopening,
  and succeeds.
- `ndimon`: main device, two receive and two transmit channels, 48 kHz, 24-bit,
  device advertisement off (the default). NDIMon-R >= 1.3.0 writes its own
  `61-ndimon-flows.toml`; the bundle supplies no flows.
- `none` (default): installs without adding profile files.
- `--admin-user`: grants tessera and tessera-admin memberships; log in again.
- `--offline`: all manifest dependencies must already be installed at sufficient
  versions. No downloads are allowed, including during local package installation.
  Online mode never runs `apt-get update`; refresh package lists separately when needed.
- `--dry-run`: performs platform and checksum validation, prints the plan, changes
  nothing, and needs no root. As an ordinary user, install and uninstall skip the
  protected record and print `installed state unknown (run as root to read the record)`;
  run dry-run as root to include recorded ownership and recovery state. All real
  operations enforce the protected record's ownership checks. Actual installation needs root. `--yes` suppresses
  the terminal confirmation; unattended runs must supply it.

Profiles and service drop-ins are written only when absent. Profiles do not set
device names: an operator's earlier name remains effective, or the daemon derives
its hostname-based default. Repeated installation
of the same version/profile verifies status without replacing the record.
Use the same `--profile` on reruns. A change of profile requires removal first.
The record stores the bundle's source commit. A same-version installation with
different or unknown recorded provenance requires `--reinstall`; this also
replaces plain packages with the bundle's bridge-enabled build. A running daemon
still needs a deliberate `--restart` to use the replacement binary.
Upgrades preserve edited drop-ins and do not restart a running daemon. They print
`restart pending`; pass `--restart` at a chosen time to apply the new binary.
Restarting interrupts audio. Downgrades are refused. Status verification waits
up to ten seconds; a running old daemon must still understand the new CLI.

Both install apt calls prohibit removals. If exact-version `tessera-web` or
`tessera-pw` dependencies prevent an upgrade, upgrade them together with tessera
first, or explicitly remove them beforehand. Uninstall simulates removal, lists
extra dependent packages in its plan, and requires `--remove-dependents` to remove
them. `--yes` only suppresses the general confirmation; it does not authorize
dependent removals. Review the plan before choosing the dedicated flag.

`/var/lib/tessera-bundle/record.json` is the authoritative record, in a root-owned
0700 directory outside package purge targets. Writes use a unique temporary file
in that directory and atomic replacement; symlink directories/files are refused.
Legacy records under the daemon-owned `/var/lib/tessera` are never imported.
The record contains version, profile, completion,
restart pending, files, packages, created users/groups, newly added memberships
(`user:group` strings), and UTC timestamp. Reruns preserve ownership entries.
The JSON uses a canonical line format parsed by bash, with restricted paths and
account names; do not edit or reformat it. Installation failures after package
installation begins exit 1 with an incomplete record and the failed step.

`uninstall.sh [--purge] [--keep-users] [--remove-dependents] [--dry-run] [--yes]` stops/disables the
daemon and removes installed packages and recorded profile files. Plain removal
retains a minimal protected record of created accounts and added memberships;
a later `--purge` uses it to finish account cleanup. `--purge` deletes that record,
and also deletes `/etc/tessera` (including plugin-created flows)
and `/var/lib/tessera`. It reverses only recorded memberships and created accounts;
`--keep-users` preserves them. Without a record it reports conservative package
removal and preserves accounts whose ownership is unknown. Removal without purge
leaves accounts/config/state and their ownership record. Repeat removal succeeds
even when packages are already gone.
The final residue scan is informational: unrelated files are listed, not deleted.
Dependencies such as avahi-daemon and
alsa-utils remain installed. The protected record tracks dependencies absent
before installation; purge marks only those packages automatic with `apt-mark
auto`, including after a plain remove. Pre-existing dependencies keep their marks.
Purge never runs autoremove; the operator chooses when to remove unused packages.
A purge marks the protected record `"purge_pending": true` before reversal and
keeps it until account cleanup and all other reversal steps succeed. A failure
names the step and exits 1; rerunning uninstall resumes that purge from the record
(even without repeating `--purge`). The record survives package purge scripts.
The record also saves `keep_users`: a retry preserves the original `--keep-users`
policy without repeating that flag. A retry may add preservation, never remove it.
Successful purge deletes regular `record.json.*` temporary files and the record
directory. Symlinks and unrelated leftovers are not deleted: a blocked directory
removal exits 1, reports the remaining directory in the residue scan, and restores
the record for retry after the obstruction is resolved.
A real step failure exits 1. Successful install/uninstall exits 0; unknown flags
and validation failures exit 1.

## Building

Build natively on the target OS with the dependencies in the main README.
Rust source version 0.2.0-beta becomes Debian upstream 0.2.0~beta. Set a
package revision explicitly when building from a source archive:

```sh
SKIP_PW=1 TESSERA_DAEMON_FEATURES=alsa-bridge TESSERA_DEB_REVISION=1 packaging/build-deb.sh
TESSERA_DEB_REVISION=1 packaging/build-bundle.sh
# Ubuntu 24.04 arm64 only:
TESSERA_DEB_REVISION=1 packaging/build-bundle.sh --allow-ubuntu-24.04
```

Package files appear in target/deb; bundles appear in target/bundle. A build
produces artifacts and does not install them or restart services. Build the
optional PipeWire package with its native development libraries and SKIP_PW
unset. Set SOURCE_DATE_EPOCH to the release commit time for repeatable timestamps.

BUNDLE_DEB_DIR assembles previously built matching native core/ALSA packages.
A bundle records its source commit, version, OS, architecture and checksums.
Do not substitute files from another OS/architecture or claim the same bundle
identity after changing its contents. Plugin archives pin the complete bundle.

## Updates and release ownership

Source, packages and runtime bundles are maintained in Tessera. NDIMon host
adapters, platform catalogs and installable plugin archives are maintained in
[NDIMon-Plugins](https://github.com/markfen88/NDIMon-Plugins). Updating source does
not automatically replace an installed runtime. Select the matching release
and restart deliberately at a suitable interruption window.
