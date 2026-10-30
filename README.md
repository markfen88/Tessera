# Tessera 0.5.1

Tessera 0.5.1 tells network controllers the device latency range a change may
use, and the portal can make a speaker's whole audio chain (bridge, ALSA bus,
CamillaDSP) follow the tessera device's sample rate.

Tessera 0.5.0 added an opt-in native device role to the daemon (`--native-device`):
`tesserad` itself answers a network controller, negotiates receive flows from the
saved subscriptions and serves transmit flows, in place of the packaged
compatibility runtime on that host. The portal's **Routing** page connects,
replaces and disconnects inputs from either host, one transmit channel can feed
several inputs, and a device can be locked behind a PIN. Hosts that do not
enable the role keep the packaged compatibility runtime as their controller
interface. See [the changelog](CHANGELOG.md) for what works and what does
not yet.

Tessera is an experimental, open audio-over-IP stack for Linux, written in Rust
and licensed under MPL-2.0. It provides a PTPv1 media clock, UDP audio flows,
local audio backends, a command-line client and a web control portal.

The project targets interoperability with Dante® media networks. It is not
vendor-certified. Packages include and enable the existing Controller
compatibility runtime alongside the daemon; device advertisement is on by
default. The native device role replaces that runtime only on a host where an
administrator enables it ([how](docs/interoperability.md#native-device-role)).
The web portal lists core services and initially accepts password
`tessera` on fresh installs, preserving existing passwords on upgrades.
Matching static flows remain available. External hardware audio content,
sustained clock stability and automatic routing interoperability are not
certified by this release. Dante is a trademark of Audinate; this project is not
affiliated with or endorsed by Audinate.

[Release notes](CHANGELOG.md) · [Documentation](docs/index.md) ·
[NDIMon plugins](https://github.com/markfen88/NDIMon-Plugins)

## What it provides

- `tesserad`: multiple logical devices, RX/TX channels, static unicast/multicast
  flows and a virtual media clock. PTP never changes the host's wall clock.
- `tesseractl`: status, devices, names/channels, rate/encoding/latency, clock,
  configuration and typed control commands.
- `tessera-web`: authenticated server-rendered controls with live updates,
  light/dark appearance, metrics, events and optional speaker/service/EQ controls.
- ALSA PCM capture/playback, a PipeWire client and an optional in-daemon sound-card
  bridge with drift compensation.
- `tessera-network`: a separate privileged helper for standalone management/audio
  VLANs, recoverable apply and traffic isolation, on systemd-networkd, Netplan,
  NetworkManager or ifupdown. The audio daemon stays unprivileged.
- `tessera-dsp` (amd64 and arm64): CamillaDSP and its GUI as the equaliser stage of a
  network speaker. The `speaker` profile of the add-on bundle sets the whole chain up
  ([CamillaDSP speaker DSP](docs/dsp.md)).
- Debian packages and profile-based add-on bundles, installed by a Rust program that
  authenticates the release first. NDIMon integration is delivered through a separate
  plugin archive containing an exact precompiled runtime bundle.

## Platforms and prerequisites

| Component | Requirements |
|---|---|
| Runtime | Linux/glibc; wired Ethernet; systemd 253 or newer for shipped units |
| Primary packages/bundles | Debian 13 Trixie; native amd64/arm64 builds |
| Experimental Ubuntu bundle | Ubuntu 24.04 Noble arm64 only, with explicit opt-in |
| Standalone VLAN editor | One wired port; systemd-networkd, Netplan/networkd, NetworkManager or ifupdown |
| Build | Rust toolchain from `rust-toolchain.toml`, a native C toolchain and `dpkg-dev` |
| ALSA plugin/bridge build | `libasound2-dev`, `pkg-config` |
| Optional PipeWire build | `libpipewire-0.3-dev`, `libspa-0.2-dev`, `libclang-dev` |

Wi-Fi is refused for media unless explicitly allowed. Configure switch tagging,
multicast/IGMP and any routed discovery infrastructure separately. Other Linux
images may build from source, but a bundle cannot cross OS/architecture boundaries.

## Install from a release

Choose the asset matching the machine and verify its trusted release checksum.
Core packages contain `tesserad`/`tesseractl`; ALSA, PipeWire and web are separate
packages. Install matching versions together:

```sh
sudo apt install ./tessera_<version>_<arch>.deb \
  ./tessera-alsa_<version>_<arch>.deb ./tessera-web_<version>_<arch>.deb
```

The daemon is enabled without being started. Configure it before starting audio.
A fresh portal starts with the password `tessera`; change it with
`sudo tessera-web set-password`. The web package includes the standalone network
helper/recovery units and their dependencies. The `tessera-dsp` package belongs to
the speaker; install it with the others when you want the CamillaDSP stage.

For a profile-based speaker or NDIMon runtime, use an add-on archive. It contains
the core daemon, the ALSA plugin, the web portal and, on amd64 and arm64, the
CamillaDSP stage that the speaker profile installs by default (`--no-dsp` leaves it
out; [dsp.md](docs/dsp.md)), not the optional PipeWire package. The archive carries
its own installer, `tessera-bundle`, which authenticates the release before it
changes anything (the `install.sh` and `uninstall.sh` scripts of 0.2.x no longer
exist). Extract trusted root installer code into a private root-owned directory:

```sh
sha256sum -c SHA256SUMS
sudo install -d -o root -g root -m 0700 /root/tessera-addon
sudo install -o root -g root -m 0600 tessera-bundle_<version>_<arch>.tar.gz /root/tessera-addon/bundle.tar.gz
sudo tar --no-same-owner --strip-components=1 -xzf /root/tessera-addon/bundle.tar.gz -C /root/tessera-addon
sudo /root/tessera-addon/tessera-bundle install --profile speaker --audio-card hw:CARD=MyDAC --yes
```

**Trust.** The installer has no built-in key. It installs a bundle only when a key
listed in the root-owned `/etc/tessera/trusted-release-keys` (mode 0644, one bare
minisign public key per line) vouches for the release; otherwise it stops with
`untrusted_key` and changes nothing. Until you have provisioned the key of a signed
release, a build is installed with `tessera-bundle install --allow-unsigned`, an
option for development builds that keeps every checksum, manifest and platform
check. `tessera-bundle verify` has no such bypass. Check the release's
`SHA256SUMS` (and its `SHA256SUMS.minisig` with `minisign`) before you run
anything you downloaded; [bundle guide](docs/bundle.md#verifying-and-installing).

The experimental Ubuntu archive requires `--allow-ubuntu-24.04` on its installer.
Use `--profile ndimon` only for runtime provisioning; installing Tessera alone
does not install or activate the NDIMon host integration. The dedicated NDIMon
plugin is the normal path for an NDIMon appliance.

Full install/removal behavior: [bundle guide](docs/bundle.md) and
[user guide](docs/user-guide.md).

### Network speaker with CamillaDSP

`--profile speaker` makes the machine a network speaker: `tesserad` receives the
audio, CamillaDSP applies the equaliser and gain filters and plays to the sound
card you name, and the portal's Speaker page controls volume, signal boost, the
services and the EQ files. Prefer an explicit card selector (`aplay -L`). A speaker
installed with an earlier bundle is converted or upgraded in place; `--no-dsp`
keeps the tessera-only speaker with the in-daemon bridge. What the installer
writes, how it upgrades and what a purge removes: [CamillaDSP speaker DSP](docs/dsp.md)
and [Raspberry Pi speaker](docs/raspberry-pi-speaker.md).

## First configuration

The reference is `/etc/tessera/config.toml`. Put operator overrides in
`/etc/tessera/config.d/*.toml`; do not edit the daemon-owned `state.toml`.
For example:

```toml
[network]
interfaces = ["eth0"]

[[device]]
id = "main"
name = "stage-audio"
rx_channels = 2
tx_channels = 2
sample_rate = 48000
encoding = 24
latency_us = 10000
```

Add static `[[flow]]` entries matching the peer's direction, channels, encoding,
rate, port and packet geometry. Without the native device role no flow is
negotiated automatically; with it, saved subscriptions are requested from the
transmitters they name. Configure sound-card bridges only in a build with
`alsa-bridge`.

```sh
tesseractl config check
sudo usermod -aG tessera,tessera-admin "$USER"
# Log in again after changing group membership.
sudo systemctl start tesserad
tesseractl status
tesseractl network show
tesseractl clock show
journalctl -u tesserad -b
```

Without configured flows/bridges the clock and local sockets can run while no
audio crosses the network. See [configuration](docs/configuration.md) and
[the CLI reference](docs/cli.md) for complete syntax.

## Management and audio VLANs

The standalone portal's Network page defines named untagged/tagged networks,
DHCP/static IPv4, and independent management/audio assignments on one wired port.
VLAN IDs are 1–4094. A blank tag follows the switch port's untagged/native network.
Only management supplies the default gateway, DNS servers and up to six DNS search
domains. The helper configures systemd-networkd, Netplan with networkd,
NetworkManager (also behind Netplan) or ifupdown, whichever the host uses.

Edits stay in a draft until Apply. Once activation is ready, open the new
management address and explicitly Keep changes within 60 seconds. Status polls
and page loads do not confirm. Revert, expiry, failed apply or boot recovery
restores the previous configuration. Timers operate independently of the browser.
Management confirmation uses the actual destination socket address, not proxy headers,
unless an administrator sets `[network] confirm = "token_only"` in `web.toml`.

For direct confirmation, the portal must listen on the management address or
`0.0.0.0:<port>`; its configured port is restricted to management. Configure TLS
proxies and switch access separately. Unsupported/complex configurations are
refused rather than silently discarded.

When NDIMon owns the appliance, its network settings are authoritative. Tessera's
standalone editor becomes read-only; choose the audio VLAN inside the NDIMon
plugin. An existing standalone setup needs an explicit ownership handoff
(`tessera-network handoff`). The NetworkManager and ifupdown backends are new in
0.3.0 and not yet accepted on live hosts. See
[network configuration and recovery](docs/network-vlans.md).

## Web portal

```sh
sudo tessera-web set-password
sudo usermod -aG tessera-admin tessera-web
sudo systemctl enable --now tessera-web
```

Default listen address: `127.0.0.1:8090`. Use an SSH tunnel or a configured TLS
reverse proxy for remote administration. Every change requires a session and
CSRF/origin validation. Daemon configuration changes can require impact-token
confirmation; network confirmation is a separate scoped transaction.

The optional appliance mode adds a Speaker page, allowlisted service controls,
CamillaDSP EQ/config files, signal boost and hardware volume when configured; the
speaker profile of the bundle turns it on. Site settings go in
`/etc/tessera/web.d/*.toml`. See [portal configuration/API](docs/web-portal.md).

## Audio backends and timing

ALSA's `pcm.tessera` provides network capture/playback, with device/channel-range
selection. PipeWire exposes source/sink nodes through `tessera-pw` and its systemd
user unit. The in-daemon bridge connects hardware capture/playback directly and
requires the `alsa-bridge` feature. Match rates/channels before enabling a route.

The virtual clock follows a PTP leader, can lead, and supports holdover. Hardware
timestamping requires suitable NIC/driver support and extra capabilities; it is
not guaranteed through VLANs. Diagnose underruns, late packets, xruns and drift
with status/metrics rather than assuming configured latency equals end-to-end delay.
[Backend guide](docs/audio-backends.md) · [Timing and privileges](docs/rt-tuning.md).

## NDIMon integration and plugins

The public [NDIMon-Plugins repository](https://github.com/markfen88/NDIMon-Plugins)
contains the Tessera host adapter, user controls, plugin authoring guide,
platform catalogs and installable archives. It requires NDIMon 1.3.0 or newer.
Tessera's daemon, ALSA backend and standalone portal remain in this repository.

A plugin archive pins an exact runtime bundle and checksum. Core source changes
are delivered to NDIMon through a rebuilt plugin release; they are not silently
merged into an installed appliance. Compatibility metadata is informational;
an exact tested-build warning UI is not part of this release.

## Build from source

```sh
git clone https://github.com/markfen88/Tessera.git
cd Tessera
cargo build --release -p tesserad -p tesseractl -p tessera-web -p tessera-network \
  --features tesserad/alsa-bridge
SKIP_PW=1 TESSERA_DAEMON_FEATURES=alsa-bridge TESSERA_DEB_REVISION=1 cargo xtask build-deb
```

Build on the target OS/architecture. `TESSERA_DEB_REVISION` makes the package
revision explicit in source archives. Version `0.5.1` is packaged as `0.5.1-*`. To create an
add-on bundle, use `cargo xtask build-bundle`, or its explicit Ubuntu opt-in.
The tree contains no shell or Python scripts: packaging, bundle and test entry
points are `cargo xtask` tasks. See [build/package options](docs/bundle.md#building).

## Upgrade, rollback and removal

Back up `/etc/tessera` first. Upgrade matching core/web/ALSA/DSP/PipeWire packages
as a set. Package/bundle installation preserves configuration and does not
silently restart a running audio daemon. Restart deliberately to activate the
new binary; restarting interrupts audio. In NDIMon, update through Plugins.
Read the upgrade notes of the [release notes](CHANGELOG.md) before moving from
0.2.x: the bundle is installed with `tessera-bundle` and authenticated first.

Bundle removal uses its installed ownership record. Plain removal preserves
configuration/accounts; purge can delete configuration and recorded accounts, and
for a CamillaDSP speaker the CamillaDSP data. Dependencies are not automatically
autoremoved. Review the [bundle guide](docs/bundle.md) before using purge or
dependent-package removal.

## Known limitations

The Controller compatibility runtime and the native device role serve the first
logical device only. Additional logical devices’ offset control ports are not yet
supported. With the native device role, a vendor controller can see the device
access lock but not set it, the newer flow-list queries of current controllers
are not answered (some tools refuse some operations because of that), and
interface statistics are not reported; see
[interoperability](docs/interoperability.md). Nothing in this release is certified.
AES67, JACK, Bluetooth latency calibration, ARC/eARC capture and automatic lip-sync
are not implemented.
Static flow and packet settings remain provisional for external hardware.
Standalone VLAN changes support one wired port on systemd-networkd, Netplan/networkd,
NetworkManager and ifupdown (the last two are new and not yet accepted on live
hosts); Wi-Fi, IPv6 configuration, bonds/bridges and arbitrary policy routing are
not managed.
CamillaGUI has no login: keep it off networks you do not trust.
Source publication does not deploy to any appliance.

## License

MPL-2.0 — [LICENSE](LICENSE). No vendor runtime or vendor source is included.
