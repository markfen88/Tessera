# Add-on bundle

The tessera add-on archive installs the daemon, the ALSA plugin, the web portal and
(amd64 and arm64) the CamillaDSP stage on Debian 13 Trixie, systemd >= 253, native
amd64 or arm64. The daemon includes the ALSA bridge. The archive carries its own
installer, `tessera-bundle` (`install`, `uninstall` and `verify`). Before 0.3.0 the
archive held the shell scripts `install.sh` and `uninstall.sh`; they are gone, the
options are the same, and `verify` and `--allow-unsigned` are new.

Ubuntu 24.04 Noble native arm64 is an **experimental**, separate build. Build
on that OS with `cargo xtask build-bundle --allow-ubuntu-24.04`; its archive
ends in `_arm64_ubuntu-noble-experimental.tar.gz`. Installation requires
`tessera-bundle install --allow-ubuntu-24.04` in addition to the normal options. The
installer checks both host and bundle OS; this flag cannot install Debian
binaries on Ubuntu or Ubuntu binaries on Debian. Other Ubuntu versions,
derivatives and Ubuntu amd64 are refused. Dependencies come from the native
Ubuntu packages, rather than the Debian dependency list. This experimental
artifact is excluded by the normal release publisher. The uninstall command
is unchanged and uses its installed record; no opt-in is needed to remove it.
Ubuntu live audio interoperability and hardware bridge behavior are unverified.
The ndimon profile configures Tessera only; it does not upgrade or enable
NDIMon-R integration (which requires NDIMon-R >= 1.3.0).

## Verifying and installing

Obtain bundles from a trusted GitHub release and verify the archive against its
minisign-authenticated release `SHA256SUMS`. Obtain the public key through a
trusted administrative channel, and check the authenticated comment for tessera,
the exact intended version and SHA256SUMS. Before invoking `tessera-bundle install`
as root, extract the trusted bundle into a root-owned 0700 directory, inaccessible
to ordinary users. The NDIMon-R install hook uses this same boundary; never execute
root installer code from a directory that the daemon or another user can modify.

```sh
minisign -Vm SHA256SUMS -p /trusted/tessera-release.pub -x SHA256SUMS.minisig
sha256sum --check --strict SHA256SUMS
sudo install -d -o root -g root -m 0700 /root/tessera-addon
sudo install -o root -g root -m 0600 tessera-bundle_VERSION_ARCH.tar.gz /root/tessera-addon.tar.gz
sudo tar --no-same-owner --strip-components=1 -xzf /root/tessera-addon.tar.gz -C /root/tessera-addon
sudo /root/tessera-addon/tessera-bundle verify
sudo /root/tessera-addon/tessera-bundle install --profile speaker --audio-card hw:CARD=MyDAC --yes
```

`tessera-bundle verify` authenticates the internal `SHA256SUMS.minisig` before
checking every payload hash and both manifests. It changes no host state, and it
has no unsigned bypass. `install` requires the same verification by default.

### Trusted release keys

The program carries **no built-in key**: its compiled public-key list is empty, and
an empty list fails closed. A release is accepted only when a key listed in
`/etc/tessera/trusted-release-keys` vouches for it. The file holds bare minisign
public keys, one base64 key per line (no `.pub` comment header, no comments); it
must be a regular file owned by root with mode exactly 0644, in directories that
are owned by root and not writable by group or others, with no symlinks on the
path. A file that breaks a rule is refused as `malformed` and the message names
the rule. The file adds keys; it cannot revoke one.

```sh
# the .pub file from the project is two lines: a comment and the key
tail -n 1 /trusted/tessera-release.pub | sudo tee /etc/tessera/trusted-release-keys > /dev/null
sudo chmod 0644 /etc/tessera/trusted-release-keys
```

The Rust verifier checks, automatically, that the signature is a prehashed Ed25519
minisign signature of a listed key and that the authenticated trusted comment names
product `tessera`, the exact bundle version and `SHA256SUMS`. It then checks the
hashes and refuses unsafe paths, links, duplicate checksum records and missing,
unlisted or changed files, before apt runs. Failures are typed: `untrusted_key`,
`bad_signature`, `wrong_product`, `wrong_version`, `missing_file`,
`checksum_mismatch` or `malformed`. A release that no trusted key vouches for is
refused with `untrusted_key:` followed by the reason (no key is installed, or the
release is signed by another key), the key file's path and a reminder that
`--allow-unsigned` is for development builds only; nothing is changed. Key
generation, signing, custody and rotation are described in
[release signing and rotation](../packaging/README.md#signed-releases).

### Unsigned builds

`tessera-bundle install --allow-unsigned` is a **development-only** opt-in for a
bundle that no trusted key vouches for, for example a build you made yourself or a
release whose key you have not provisioned yet. It bypasses the authentication of
the signature and nothing else: path, manifest, per-file checksum, platform and
lifecycle checks all still run, and the bundle's own `SHA256SUMS` must still match
the files. Check the archive against the release's `SHA256SUMS` (and signature) by
hand first, as in the block above; never use the option on an archive whose origin
you cannot vouch for.

## What the bundle installs

The bundle has its own SHA256SUMS over every payload file. Installation rejects
corruption before apt. It includes `tessera`, `tessera-alsa` and `tessera-web`, and on
amd64 and arm64 `tessera-dsp` (CamillaDSP and its GUI, see [dsp.md](dsp.md)), which the
`speaker` profile installs unless `--no-dsp` is given.
Controller compatibility is included in the core package and enabled with the
daemon. The portal includes a Services page and initializes a fresh password
to `tessera`; existing passwords are preserved. Dependencies
are derived from their Debian Depends, plus `avahi-daemon` and `alsa-utils`;
Trixie's systemd and init-system-helpers are platform prerequisites.

## Profiles and options

`tessera-bundle install [--profile speaker|ndimon|none] [--admin-user USER]..
[--audio-card CARD] [--no-dsp] [--allow-unsigned] [--allow-ubuntu-24.04] [--restart]
[--reinstall] [--offline] [--dry-run] [--yes]`:

- `speaker`: main device, two receive channels, no transmit channels, 48 kHz.
  **By default it includes the CamillaDSP stage** (`tessera-dsp`): `tesserad` serves the
  PCM `tessera`, CamillaDSP plays to the card, and the portal's Speaker page,
  service switches, volume and signal boost are enabled through `/etc/tessera/web.d/60-speaker.toml`
  (see [dsp.md](dsp.md)). With **`--no-dsp`** the profile is tessera only: the daemon's
  playback bridge plays to the card, with the audio supplementary group for the daemon.
  Prefer an explicit `--audio-card hw:CARD=...` selector; the fallback `default` may
  select an unintended ALSA route. A missing card warns and succeeds (the bridge keeps
  reopening; CamillaDSP keeps restarting until the card appears). Changing from
  CamillaDSP back to tessera only needs the bundle removed first. The other way is an
  in-place upgrade: a tessera-only speaker (its `60-speaker.toml` has the profile's own
  `[[bridge]]`) is converted by a plain `install --profile speaker`, which removes just
  that table, reloads a running daemon so it lets go of the card, and starts CamillaDSP.
  A bridge that is not the profile's own makes the install refuse before anything changes
  (remove it, or pass `--no-dsp`); `--no-dsp` keeps an existing tessera-only speaker as it
  is. A CamillaDSP speaker keeps every file it has (device configuration, portal drop-ins,
  the active CamillaDSP configuration and state). A speaker that already has an engine of
  its own (a `camilladsp.service` you wrote, a program in `/opt/camilladsp`) keeps its
  program, configurations and state; the packaged unit replaces your unit file, which is
  moved aside, and the EQ GUI is not added. Details: [dsp.md](dsp.md).
- `ndimon`: main device, two receive and two transmit channels, 48 kHz, 24-bit,
  device advertisement enabled by default. NDIMon-R >= 1.3.0 writes its own
  `61-ndimon-flows.toml`; the bundle supplies no flows.
- `none` (default): installs without adding profile files.
- `--admin-user`: grants tessera and tessera-admin memberships; log in again.
- `--allow-unsigned`: see [Unsigned builds](#unsigned-builds).
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
its hostname-based default.

## Upgrades and repeated installs

Repeated installation of the same version/profile verifies status without replacing
the record. On a CamillaDSP speaker it also sets what depends on the host and not on the
packages: the EQ engine GUI's `bind_address` (the management address, once the port has been
named with `[network] interface`) and the portal's `gui_url` link to it, and it restarts
`camillagui` if the address changed and the GUI runs. No package is installed and nothing in the
audio path is restarted; `--reinstall` is not needed for this. Use the same `--profile` on reruns. A change of profile requires removal
first. The record stores the bundle's source commit. A same-version installation with
different or unknown recorded provenance requires `--reinstall`; this also
replaces plain packages with the bundle's bridge-enabled build. A running daemon
still needs a deliberate `--restart` to use the replacement binary.
Upgrades preserve edited drop-ins and do not restart a running daemon. They print
`restart pending`; pass `--restart` at a chosen time to apply the new binary.
Restarting interrupts audio. Downgrades are refused. Status verification waits
up to ten seconds; a running old daemon must still understand the new CLI.

To move from a 0.2.x bundle, extract the new archive and run the same
`tessera-bundle install` command you used before (with `--allow-unsigned` until you
have provisioned a key). The record written by the 0.2.1 and 0.2.2 shell installers
is read as it is, a CamillaDSP speaker keeps its files, and a tessera-only speaker
becomes the CamillaDSP speaker unless you add `--no-dsp`. The EQ engine is linked
into the daemon's wants (`WantedBy=tesserad.service`) on every install and upgrade,
so it starts whenever `tesserad` starts; an engine you switched off stays off.

**Dependencies.** A dependency is met by the package of that name or by an installed package
that provides it, as dpkg and apt do: on Debian 13 and Ubuntu 24.04 `libasound2t64` provides
`libasound2`, so a bundle that names either installs. A provider that states no version
meets only a dependency without a version. Before anything is written the installer asks apt
(a simulated install) whether the missing dependencies can be installed; if not, the run
stops with apt's reason, and no record, file or package has been touched.

**What an upgrade keeps.** The engine and the EQ GUI stay switched on or off as the speaker
had them (`systemctl is-enabled`, asked before the packages change anything): a speaker
whose engine or GUI you switched off keeps it off, and a speaker that had an engine of its
own and no GUI does not get the GUI. A speaker with neither is a new CamillaDSP speaker and
gets both. If you gave the portal a service list of your own (`[[appliance.service]]` in
`web.toml` or a `web.d` file), the profile writes none (a list in a drop-in replaces the
whole list, so it would drop your services and their polkit grants); add `camilladsp`
(label `EQ engine`) to your list for its switch, and run `tessera-web appliance polkit`
as the generated `web.d/60-speaker.toml` says.

**A unit of your own.** A file `/etc/systemd/system/camilladsp.service` (or
`camillagui.service`) wins over the packaged unit and would keep the engine on its old
program, without `PartOf=` and without the link that starts it with the daemon. The
installer moves it to `camilladsp.service.pre-tessera-dsp` (a second one to `….2`), moves
the `camilladsp.service.d` directory that sat next to it to
`camilladsp.service.d.pre-tessera-dsp`, reloads systemd and says so. A running engine keeps
its old program until `systemctl restart camilladsp`. The plan (`--dry-run` included) names
the unit files and drop-in directories it will move. The enable links that `systemctl enable`
made for your unit (`multi-user.target.wants/camilladsp.service` and the like) pointed at the
file that moved and would dangle, with `is-enabled` still saying `enabled`: they are removed,
each one named, and the packaged unit is enabled in their place (`tesserad.service.wants`). Drop-ins next
to the packaged unit (no unit file of yours) are your changes to it and stay. A mask (a link to
`/dev/null`) is left alone with a warning. To go back: stop and disable the engine, move the
file back, `systemctl daemon-reload`, `systemctl enable --now camilladsp`.

**Pending restart.** `restart pending` is recorded together with the run of the daemon it is
about (systemd's `InvocationID`). A later `install` run reports `restart done` and clears it
once the daemon has been restarted or the board rebooted.

Both install apt calls prohibit removals. If exact-version `tessera-web` or
`tessera-pw` dependencies prevent an upgrade, upgrade them together with tessera
first, or explicitly remove them beforehand. Uninstall simulates removal, lists
extra dependent packages in its plan, and requires `--remove-dependents` to remove
them. `--yes` only suppresses the general confirmation; it does not authorize
dependent removals. Review the plan before choosing the dedicated flag.

## The installation record

`/var/lib/tessera-bundle/record.json` is the authoritative record, in a root-owned
0700 directory outside package purge targets. Writes use a unique temporary file
in that directory and atomic replacement; symlink directories/files are refused.
Legacy records under the daemon-owned `/var/lib/tessera` are never imported.
The record contains version, profile, completion,
restart pending (with the daemon run it is about), files (the device configuration, and with
CamillaDSP the portal's `web.d/60-speaker.toml`, its unit drop-in and the seed
configuration `Speaker.yml` when the installer created it), packages (`tessera-dsp` among them), created
users/groups (`camilla` when the bundle created it), newly added memberships
(`user:group` strings), and UTC timestamp. Records written by the 0.2.1 and 0.2.2 shell
installers are read as they are. Reruns preserve ownership entries.
The JSON keeps the original canonical line format and is parsed as data by Rust,
with restricted paths and account names; do not edit it. Installation failures after package
installation begins exit 1 with an incomplete record and the failed step.

## Removal and purge

```sh
sudo /root/tessera-addon/tessera-bundle uninstall --dry-run          # the plan, nothing changed
sudo /root/tessera-addon/tessera-bundle uninstall --purge --yes      # also deletes configuration and data
```

`tessera-bundle uninstall [--purge] [--keep-users] [--remove-dependents] [--dry-run] [--yes]` stops/disables the
daemon (first CamillaDSP's GUI and engine, when the bundle installed them) and removes
installed packages and recorded profile files, the portal's `web.d` and unit drop-ins
included; a portal that stays installed gets its polkit rules regenerated for the
remaining service list. Plain removal
retains a minimal protected record of created accounts and added memberships;
a later `--purge` uses it to finish account cleanup. `--purge` deletes that record,
and also deletes `/etc/tessera` (including plugin-created flows)
and `/var/lib/tessera`, and, when the bundle installed the CamillaDSP stage, what the
bundle and its packages made there: the seed configuration it recorded, the engine's state
file when it names no configuration that exists, the GUI settings (package conffiles) and the
directories once they are empty. **Everything else in `/opt/camilladsp` is yours and stays**: an engine
program of your own, your configurations and filter files; the plan's last lines list
what was left. A plain removal keeps all of it, the seed included (the GUI edits it), and the account.
Purge reverses only recorded memberships and created accounts (the
`camilla` account among them); `--keep-users` preserves them. If you moved a unit of your own aside
on install, uninstall prints where it is and how to use it again. Without a record it reports conservative package
removal and preserves accounts whose ownership is unknown. Removal without purge
leaves accounts/config/state and their ownership record. Repeat removal succeeds
even when packages are already gone.
The final residue scan is informational: unrelated files are listed, not deleted.
Dependencies such as avahi-daemon and
alsa-utils remain installed. The protected record tracks dependencies absent
before installation; purge marks only those packages automatic with `apt-mark
auto`, including after a plain remove. Pre-existing dependencies keep their marks:
the installer does not name an installed dependency to apt (naming a package makes
apt mark it manually installed) unless it is older than the bundle's minimum, and
then puts back an automatic mark it had.
Purge never runs autoremove; the operator chooses when to remove unused packages.

Removing the packages (with or without `--purge`) also takes back what they left
loaded: the helper's firewall table `inet tessera_network`, the empty slices
`tesseraaudio.slice` and `system-tessera\x2dcontroller.slice`, and the socket-buffer
ceilings `net.core.rmem_max` / `wmem_max`, which go back to the values recorded when
the package first raised them (unless something else has changed them since). The
network configuration the portal committed stays, by design: the Netplan or
networkd files, the VLAN links they define and `/etc/modules-load.d/tessera-8021q.conf`,
so that removing Tessera never changes the host's addressing or cuts its management
connection. Edit or delete those files by hand when the VLANs are no longer wanted.
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
The package version is the workspace version plus a Debian revision: `0.3.0`
becomes `0.3.0-<revision>`, and a pre-release such as `0.4.0-beta` becomes
`0.4.0~beta-<revision>`, so that it sorts before the final release. Set a
package revision explicitly when building from a source archive:

```sh
SKIP_PW=1 TESSERA_DAEMON_FEATURES=alsa-bridge TESSERA_DEB_REVISION=1 cargo xtask build-deb
TESSERA_DEB_REVISION=1 cargo xtask build-bundle
# Ubuntu 24.04 arm64 only:
TESSERA_DEB_REVISION=1 cargo xtask build-bundle --allow-ubuntu-24.04
```

Package files appear in target/deb; bundles appear in target/bundle. A build
produces artifacts and does not install them or restart services. On amd64 and arm64
the build also packages `tessera-dsp`: it downloads the pinned CamillaDSP and GUI
archives named in `packaging/dsp/dsp.lock` into `~/.cache/tessera-dsp`
(`TESSERA_DSP_CACHE`), checks each SHA-256 and refuses any difference
([dsp.md](dsp.md#building)); `SKIP_DSP=1` leaves the package, and so the speaker
profile's default, out (`"dsp": false` in `manifest.json`). Build the
optional PipeWire package with its native development libraries and SKIP_PW
unset. Set SOURCE_DATE_EPOCH to the release commit time for repeatable timestamps.
No step of a build or of the installer runs a shell script: the tasks are
`cargo xtask` and the installer is the Rust program `tessera-bundle`.

BUNDLE_DEB_DIR assembles previously built matching native core/ALSA packages (and the
`tessera-dsp` package, when that directory has one).
A bundle records its source commit, version, OS, architecture and checksums.
Do not substitute files from another OS/architecture or claim the same bundle
identity after changing its contents. Plugin archives pin the complete bundle.
A bundle you build yourself is unsigned: install it with `--allow-unsigned`, or sign
it as described in [release signing](../packaging/README.md#signed-releases).

## Updates and release ownership

Source, packages and runtime bundles are maintained in Tessera. NDIMon host
adapters, platform catalogs and installable plugin archives are maintained in
[NDIMon-Plugins](https://github.com/markfen88/NDIMon-Plugins). Updating source does
not automatically replace an installed runtime. Select the matching release
and restart deliberately at a suitable interruption window.
