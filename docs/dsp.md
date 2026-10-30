# CamillaDSP speaker DSP (`tessera-dsp`)

A tessera speaker normally includes a DSP stage: **CamillaDSP** reads the network audio from the
ALSA PCM `tessera`, applies the equaliser and gain filters of the active configuration, and plays
to the DAC. The `tessera-dsp` package installs CamillaDSP and its web GUI, and the speaker
profile of the tessera bundle sets the whole chain up. It is **on by default** for the `speaker`
profile and can be left out with `--no-dsp` for a speaker that should only run tessera.

```text
 network ──► tesserad ──RX ring──► pcm.tessera ──► CamillaDSP [AsyncSinc, rate adjust] ──► DAC
            (media clock)          (tessera-alsa)   (EQ, signal boost)                    (crystal)
```

CamillaDSP's asynchronous resampler is the only clock crossing between the network media clock
and the DAC. Local sources cannot share this path: CamillaDSP reads only the network audio.

## What is installed

| Part | Where |
|------|-------|
| CamillaDSP (upstream release, unmodified) | `/usr/lib/tessera-dsp/camilladsp` |
| CamillaGUI backend (upstream release, unmodified) | `/usr/lib/tessera-dsp/gui/camillagui_backend/` |
| Units | `camilladsp.service` (engine, websocket on `127.0.0.1:1234`), `camillagui.service` (GUI, port 5005, on loopback until the bundle installer or you set an address) |
| GUI configuration (kept across upgrades) | `/etc/camilladsp/camillagui.yml`, `/etc/camilladsp/gui-config.yml` |
| Your configurations and filter files | `/opt/camilladsp/configs`, `/opt/camilladsp/coeffs`, `/opt/camilladsp/statefile.yml` |
| Service account | user `camilla` (groups `audio`, `tessera`) |
| Provenance | `/usr/share/doc/tessera-dsp/dsp.lock`, `vendored.txt` |

Both programs are the official upstream releases, pinned by SHA-256 in `packaging/dsp/dsp.lock`;
the build refuses any archive that does not match. They are GPL-3.0-or-later; the corresponding
source is at the upstream tags named in `vendored.txt`.

The programs live in a root-owned directory, so the account the unauthenticated GUI runs as cannot
replace them; only your data is under `/opt/camilladsp`. An upgrade never replaces your configurations,
filter files or state file. `apt remove` keeps them. **A purge is a clean uninstall**: `apt purge
tessera-dsp`, or `tessera-bundle uninstall --purge`, removes what the package and the bundle made: the
GUI settings (conffiles), the seed configuration the bundle created, the state file when it names no
configuration that exists, and the directories once they are empty (the bundle also removes the `camilla`
account it created). **It does not delete anything else in `/opt/camilladsp`**: an engine program of your
own, configurations and filter files you made or activated stay, and the bundle lists them. A plain
`tessera-bundle uninstall` removes the packages and keeps configuration and data, like tessera itself.
The package never deletes the account on its own; it masks the two units on `apt remove` so no enable
link outlives them.

## Installing

With the bundle (recommended): the speaker profile includes the DSP stage.

```sh
sudo ./tessera-bundle install --profile speaker --audio-card hw:CARD=MyDAC --yes          # tessera + CamillaDSP
sudo ./tessera-bundle install --profile speaker --audio-card hw:CARD=MyDAC --no-dsp --yes # tessera only
```

The installer then:

1. installs `tessera`, `tessera-alsa`, `tessera-web` and `tessera-dsp` at the bundle's version;
2. writes `/etc/tessera/config.d/60-speaker.toml` **without** the in-daemon bridge (CamillaDSP owns
   the card; with `--no-dsp` the bridge is configured as before);
3. creates `/opt/camilladsp/configs/Speaker.yml` from the template, if no configuration exists yet:
   `tessera` in, your card out, and the `dashboard_signal_boost` gain filter the portal's boost
   control edits. The chain starts **flat**: add your equaliser filters with the GUI;
4. writes `/etc/tessera/web.d/60-speaker.toml`, which turns on the portal's Speaker page with
   service switches, the amplifier volume and the signal boost (see
   [web-portal.md](web-portal.md#appliance-mode)); if you have a service list of your own in
   `web.toml` or `web.d` the file carries none, so yours is used as it is;
5. enables and starts `camilladsp` and `camillagui`, the GUI listening on the speaker's management address
   (see **GUI** below). On a speaker that already has an engine of its own or a switched-off engine or GUI,
   the installer keeps what it had (see the upgrade notes).

**Upgrading a speaker.** Run the same command with the new bundle.

- A tessera-only speaker (0.2.1, or 0.2.2 installed with `--no-dsp`; it has the profile's own
  `[[bridge]]`) becomes the CamillaDSP speaker in place: the speaker profile's `[[bridge]]` table is
  removed from `/etc/tessera/config.d/60-speaker.toml` (everything else in the file stays, comments
  included), the running daemon is told to reload (`tesseractl --yes config reload`) so it lets go of
  the card, and CamillaDSP starts. The installer does not restart `tesserad`. A bridge that is not
  the profile's own makes the install refuse, before anything changes. Add `--no-dsp` to keep the
  speaker as it is.
- A speaker with an engine of its own (a `camilladsp.service` you wrote, usually with a program in
  `/opt/camilladsp`) keeps its program, configurations, state file and portal service list. Your unit
  file would shadow the packaged one (the old program, no `PartOf=`, no start with the daemon), so
  the installer moves it to `camilladsp.service.pre-tessera-dsp` (drop-ins that sat next to it to
  `camilladsp.service.d.pre-tessera-dsp`), reloads systemd and says so; the packaged unit runs the
  vendored engine, and a running engine keeps its old program until `systemctl restart camilladsp`.
  The GUI is not added to a speaker that did not have one, and `--purge` never deletes your files.
- A CamillaDSP speaker (0.2.2) keeps every file it has: the device configuration, the portal's
  `web.d/60-speaker.toml` and unit drop-in, the active CamillaDSP configuration and the state file are
  never rewritten. The packages move to the new version, a running engine keeps playing (restart it
  when convenient), and the portal restarts to pick up the new files. The portal's unit drop-in keeps
  the single `DeviceAllow=/dev/snd/controlC<n>` line it was written with; to let the Volume pane follow
  the card across a change of index, replace it with the device lines of `appliance-paths.conf`
  ([web-portal.md](web-portal.md#appliance-mode)). A copy of that example in
  `tessera-web.service.d/appliance-paths.conf` is removed by the installer when every setting in it
  is in the profile's `60-speaker.conf` or is `ReadWritePaths=-/var/lib/alsa`, which earlier
  builds needed for `alsactl store` and the portal no longer does (it keeps the volume in its own
  state directory); a file with a setting of yours is kept. The line is not copied into the
  profile's drop-in: nothing would use the permission.
- Going back from CamillaDSP to tessera only needs `tessera-bundle uninstall` first; installing with
  `--no-dsp` over a CamillaDSP speaker is refused with that hint.

With plain packages, install `tessera tessera-alsa tessera-web tessera-dsp`, copy
`/usr/share/doc/tessera-dsp/examples/Speaker.yml.in` to `/opt/camilladsp/configs/Speaker.yml`
(replace the `@...@` markers: `@PLAYBACK_FORMAT@` is a sample format the card plays, in the EQ
engine's spelling: `S32_LE`, `S24_3_LE`, `S24_4_LE` or `S16_LE`), activate it in the GUI, and start the two services.

## Operating it

- **Portal.** The Speaker page lists `EQ engine` among the services and, with
  `[appliance.camilladsp]`, shows the signal boost and the configuration files. Its **Links**
  card opens the GUI when `gui_url` is set (the speaker profile sets it to the management address).
- **GUI.** `http://<management address>:5005/gui/index.html`: filters, mixers, pipeline, devices, and
  activating a configuration. **The GUI has no login**, so it does not listen on every interface.
  The package ships `bind_address: "127.0.0.1"` in `/etc/camilladsp/camillagui.yml` (an SSH tunnel
  reaches it: `ssh -L 5005:127.0.0.1:5005 user@speaker`). The bundle installer sets the speaker's
  **management address**, so the GUI and the portal's link work from the network the speaker is
  managed on and from no other: the address of the port you named with `[network] interface = "eth0"`
  in `/etc/tessera/web.toml` or a `web.d` file, else of the only port that has a default route. A
  speaker with several default routes (Ethernet and Wi-Fi) gets neither guess: the GUI stays on loopback
  and the installer says to name the port and install again (a plain `install` of the version that is
  installed applies it: no `--reinstall`, no package is installed), or to set `bind_address` yourself. The same
  holds when the port has no IPv4 address. The address is a fixed number in the file: when it changes
  (DHCP), edit `bind_address` (and `gui_url` in `web.d/60-speaker.toml`), or give the speaker a fixed
  address. `camillagui.service` retries until the address exists. A GUI that listened on every
  interface (`0.0.0.0`, the first 0.3.0 candidates) is narrowed the same way on upgrade. A running
  GUI keeps the socket it opened, so when a run leaves `bind_address` other than it was before the
  packages ran (the installer set the address, or dpkg replaced an unmodified file), the installer
  restarts `camillagui` and says so; the GUI has no audio, and the engine and the daemon are not
  touched. A GUI that is switched off is not started. The installer
  sets the address only in a file that is still the package's own (dpkg's md5 of it still matches),
  so naming the port and installing again works after the first run left the GUI on loopback; a file
  you edited is never rewritten, except that `0.0.0.0` is narrowed, and an address you chose is kept.
  The same run adds `gui_url` to a `web.d/60-speaker.toml` that was written before the address was
  known (one line, at the end of its `[appliance.camilladsp]` table); a `gui_url` of yours, or a
  drop-in without that table, is left alone and the installer prints the line to set. The network
  helper's firewall also rejects TCP port 5005 on every link but the
  management one and loopback ([network-vlans.md](network-vlans.md)).
- **Upgrades keep the state.** An upgrade does not switch the GUI or the engine on if you had them
  off: `systemctl is-enabled` is asked before the packages change anything.
- **Restart.** `systemctl restart camilladsp camillagui` (a few seconds of silence). The unit
  is `PartOf` `tesserad`: restarting the daemon restarts CamillaDSP with it (its capture PCM fails with `-ENODEV` and the engine would otherwise sit idle). Stopping the daemon stops the engine too, and
  starting the daemon again starts it: the unit is also `WantedBy=tesserad.service`, so enabling the engine links it into
  `tesserad.service.wants`. That is what keeps a speaker playing after a network change (`tessera-network` stops the daemon, changes the
  network and starts it again; with `PartOf` alone the engine stayed stopped and the speaker was silent). An engine that is switched off
  (`systemctl disable camilladsp`, the Speaker page's switch) removes both links and stays off when the daemon starts
  (the portal's helper lets systemd reload after the change; without the reload the daemon's next start brought it back). A speaker
  upgraded from 0.2.2 gets the second link when the upgrade enables the engine again (the packages and the bundle both do, for an engine that was on); to add it
  by hand, `sudo systemctl enable camilladsp`, then `systemctl show -p WantedBy camilladsp` lists `tesserad.service`.
- **Volume.** The portal's Volume slider is the amplifier's hardware mixer control, not
  CamillaDSP's volume. The portal saves it in `/var/lib/tessera-web/volume.json`, sets it again
  at start (also for a card that appears later), follows the card if its index changes and offers
  **Unmute** when the card's playback switch is off ([web-portal.md](web-portal.md#eq-engine)).
- **Upgrades** never restart a running engine; the package prints a reminder.
- **Rate.** The PCM `tessera` accepts only the tessera device's own rate (`tesseractl status`), so the
  engine's capture rate (`capture_samplerate`, else `samplerate`) must equal it. The seeded
  configuration does; if they ever differ the engine cannot start and the speaker stays silent. The
  Speaker page then shows an *EQ engine input rate mismatch* notice with both rates and, when the
  configuration has a resampler, a button that sets only `capture_samplerate` and reloads the engine
  ([raspberry-pi-speaker.md](raspberry-pi-speaker.md)). With the portal's rate follower
  (`[appliance.rate_follow]`, off by default) tessera dictates the rate: the engine's `samplerate`
  and a set `capture_samplerate` are overwritten with the device's rate, in the active configuration
  and in the running engine, whenever they differ (a configuration restored or applied in the GUI
  included), and the notice offers no button
  ([raspberry-pi-speaker.md](raspberry-pi-speaker.md#shared-sources-and-one-sample-rate)).

- **Format.** The seeded configuration plays in the first of `S32_LE`, `S24_3_LE` (ALSA's
  `S24_3LE`), `S24_4_LE` (ALSA's `S24_LE`) and `S16_LE` that the sound card accepts; the names are
  CamillaDSP's, which reads no other spelling for an ALSA device (4.1.3 refuses `S24_3LE` and `S24_LE`). The installer asks the kernel's ALSA PCM interface when it
  can open the card (a `plughw:` selector converts every format and is not asked; a card that is
  absent or in use cannot be asked) and keeps `S32_LE` otherwise, saying so. Many USB DACs offer
  only `S16_LE` and `S24_3_LE`. A card that refuses the configured format stops the engine with
  `snd_pcm_hw_params_set_format … Invalid argument`; the Speaker page then shows an *EQ engine
  output format not supported* notice with the device, the format and the engine's message. Change
  `devices.playback.format` in the GUI to one the card plays; the capture side keeps `S32_LE`.

Symptoms and fixes (silent speaker, rate or format notices, the Volume pane, the installer's
messages): [troubleshooting.md](troubleshooting.md#network-speaker-camilladsp-and-the-portal).

## Leaving it out

`--no-dsp` at install time, or `SKIP_DSP=1 cargo xtask build-deb` when building, gives a speaker
with only tessera (`tessera`, `tessera-alsa`, `tessera-web`). To remove it from an installed
speaker: `sudo apt remove tessera-dsp`, delete `/etc/tessera/web.d/60-speaker.toml`, and put the
`[[bridge]]` back in `/etc/tessera/config.d/60-speaker.toml`
([raspberry-pi-speaker.md](raspberry-pi-speaker.md)).

## Building

`cargo xtask build-deb` (and `build-bundle`, which runs it) packages `tessera-dsp` on amd64 and arm64
when the ALSA plugin is built. It reads `packaging/dsp/dsp.lock`, downloads each pinned archive over
HTTPS from `github.com` into `~/.cache/tessera-dsp` (`TESSERA_DSP_CACHE` overrides it), and uses an
archive only when its SHA-256 equals the pin: a cached file that differs is discarded, a download that
differs is refused, and nothing is extracted first. The check is Rust code (`xtask/src/dsp.rs`, with
tests); the tools it calls are `curl` and `tar`. To move to a new upstream release, change the lock
file's version, URL and digest together. `SKIP_DSP=1` leaves the package out; the bundle then
records `"dsp": false` and the speaker profile installs without it.

## Platforms

Debian 13 (trixie) amd64 and arm64, the platforms of the bundles. There are no armhf packages.
