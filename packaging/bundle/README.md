# tessera add-on bundle

For Debian Trixie, native amd64 or arm64, systemd >= 253. Includes tessera
and tessera-alsa, with the ALSA bridge enabled. No PipeWire client or portal.
An archive marked `ubuntu-noble-experimental` instead requires Ubuntu 24.04
Noble arm64 and explicit `--allow-ubuntu-24.04` on install. It cannot be used
on Debian. Live audio interoperability and hardware bridge behavior on Ubuntu
are unverified. Normal Debian bundles cannot be installed on Ubuntu.
Obtain this archive from a trusted release; checksums give integrity, not authenticity.
Before running as root, extract into a root-owned 0700 directory (docs/bundle.md).

    sudo ./install.sh --profile speaker --audio-card hw:CARD=MyDAC --yes
    sudo ./uninstall.sh --purge --yes

Use `--profile ndimon` for NDIMon-R >= 1.3.0 (two receive and two transmit
channels at 48 kHz, 24-bit, device advertisement off (the default)). The plugin owns
`61-ndimon-flows.toml`. Speaker has two receive channels, no transmit channels,
and a playback bridge; a missing card warns and retries. The default audio
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
