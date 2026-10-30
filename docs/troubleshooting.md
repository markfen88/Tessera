# Troubleshooting

## NetAudio reports a name contains unsupported characters

If subscription planning reports `name contains unsupported characters`, check
the transmitter's **device name** as well as its TX channel name. The request
identifies the destination RX by channel number; an RX display label alone is
not the source-name pair used to connect.

Audinate's [public naming rules](https://dev.audinate.com/GA/dante-controller/userguide/webhelp/content/device_and_channel_names.htm)
allow spaces in channel labels such as `TX 2` and `Analog L`. Device names use
ASCII letters, digits and hyphens, with no leading or trailing hyphen. Thus a
device named `Studio Laptop` is outside those rules even though a TX label
`TX 2` is valid. This makes the device name a candidate for the observed
NetAudio error; it does not isolate which field its planner rejected.

Tessera validates new rename/label commands and warns about existing names.
It preserves existing spellings and subscriptions. Do not strip spaces from
channel labels, lowercase everything or automatically rename a running
transmitter as a workaround: routing uses names as identity. A deliberate
rename must keep discovery, the compatibility runtime and receiver readback
consistent, then recheck the intended routes in NetAudio and Dante Controller.

Start with the facts:

```sh
systemctl status tesserad                       # running? the Status: line says what it last did
journalctl -u tesserad -b --no-pager | tail -50 # the daemon's log
tesseractl status                               # can you talk to it, what does it think
tesseractl config check                         # does the configuration pass validation
id                                              # your groups
```

and for the PipeWire client: `systemctl --user status tessera-pw` and
`journalctl --user -u tessera-pw -b`.

Remember what exists today (see `user-guide.md` and `interoperability.md`):
the daemon runs the PTP clock, the media engine with static `[[flow]]`
entries, receiver keepalives, mDNS advertising, the local clients
and (with `alsa-bridge`) the sound-card bridge. By default it does not
implement the vendor's control protocols itself: the separate Controller
compatibility runtime (`tessera-controller@main`) serves the first device's base
control ports and hands what a controller sets up to the daemon as typed
commands. A host with the opt-in native device role (`--native-device`) serves
them from `tesserad` instead, and the runtime is disabled there.

On a speaker, the sections "Network speaker (CamillaDSP and the portal)" and
"Add-on bundle" below cover most of what goes wrong.

## `tesseractl` fails

**`cannot connect to /run/tessera/control.sock` (exit 3)**

```
tesseractl: cannot connect to /run/tessera/control.sock: No such file or directory
is tesserad running? use --socket or TESSERA_SOCKET if it listens elsewhere
```

The daemon is not running (the package enables it but does not start it):
`sudo systemctl start tesserad`. If it is running, you may be pointing at
another socket: `--socket` / `TESSERA_SOCKET`, or the daemon was started with
another `--runtime-dir`. A `Permission denied` here instead of `No such file`
is the next case.

**Exit 4, permission denied**

* Reading status needs group `tessera`; changing settings needs
  `tessera-admin` **as well** (the socket is mode 0660 group `tessera`, so a
  member of only `tessera-admin` cannot even connect).
* `id` must list both groups. Group changes only reach **new** login
  sessions: log out and in, or `newgrp tessera`.
* If you renamed the admin group in `[daemon] admin_group`, use that name. A
  name that does not exist means only root may change settings.

**`... is locked by the configuration`** (exit 1): the key is in a `locked`
list of `config.toml` or a drop-in. Edit the file, not the daemon (the lock
exists so that a controller on the network cannot change it).

**`invalid latency: 50 us is outside the allowed range [1000, 40000]`**: the
range is `latency_bounds_us`; values below the platform timing floor also
need `allow_unsafe_latency = true`.

**Requests slow down or connections are refused**: per-user connection and rate
limits (see `man tesserad`); a script that opens many connections or loops
without pause is throttled by design.

## The daemon does not start, or reloads wrongly

Run the checker; it needs no running daemon and tells you the file and key:

```
$ tesseractl config check
tesseractl: configuration has 4 error(s)
  error: device[0].sample_rate: 12345 is not a supported rate (supported: 44100, 48000, 88200, 96000, 176400, 192000) (in /etc/tessera/config.toml)
  error: device[0].encoding: 20 is not a supported encoding (supported: 16, 24, 32) (in /etc/tessera/config.toml)
  error: device[0].latency_us: 10 is outside latency_bounds_us [1000, 40000] (in /etc/tessera/config.toml)
  error: device[0].latency_us: 10 is below this platform's timing floor of 1000; set allow_unsafe_latency = true to accept it (in /etc/tessera/config.toml)
```

A syntax error is reported the same way (and by `tesserad` itself in the
journal). At start the unit then fails; `Restart=on-failure` tries again
after a second, until systemd's default start limit (5 starts in 10 s) stops
it with `start request repeated too quickly`. After fixing the file run
`sudo systemctl reset-failed tesserad` and start it again.

**A reload that did nothing.** `systemctl reload tesserad` may return success
even when the daemon refused the new files; the old configuration keeps
running. Read the reason in `systemctl status tesserad`:

```
Status: "reload failed (still running the old configuration): invalid config: ..."
```

and check with `tesseractl config show --effective` what is in force.

**`is in use by a running daemon (refusing to start a second one)`**: only
one daemon per runtime directory (`/run/tessera/tesserad.lock`). Stop the
other one, or give the second a different `--runtime-dir`.

**`group tessera does not exist` / `group X does not exist`**: the
`tessera` group is created at install time. `sudo systemd-sysusers
tessera.conf` recreates it (and the user). A group named explicitly with
`--socket-group` that does not exist is an error; a missing default group is
only a warning, and the sockets then keep the daemon's own group, which is
not what you want (members of `tessera` could not connect).

**Unknown key or `Type=notify-reload` rejected in the unit**: your systemd
is older than 253. See "Older systemd" in `user-guide.md`.

**Unit restarted by the watchdog** (`Watchdog timeout`): the daemon pings only
while its state thread answers; a stuck state thread is restarted on purpose.
Report it with the journal around that moment.

**Device renamed after adding a second device**: with several devices the
default name becomes `<host>-<id>`. Give every device an explicit `name`.

## Real-time priority and glitches

* **`cannot enter SCHED_FIFO (...)` in the journal of `tessera-pw`** or a
  warning that the daemon runs at normal priority: the limits did not arrive.
  For the user service the usual cause is that the user manager has no
  real-time hard limit (a user manager silently clamps the unit's
  `LimitRTPRIO` to what PAM granted at login): see `rt-tuning.md`, section 3,
  and read the *effective* values from `/proc/PID/limits` of the running
  process (`systemctl show` prints only the requested value). `--rt-priority 0` silences the
  warning on purpose.
* **`cannot lock memory (RLIMIT_MEMLOCK is N KiB, not unlimited ...)` in the journal of
  `tessera-pw`**: the memory-lock limit is not unlimited (a stock user session and a `pipewire`
  group entry both give a limited `memlock`). `tessera-pw` then runs without locking its memory and
  says so; it does not lock part of it, because a partial limit would make PipeWire's own later
  allocations fail. Same place as above (`rt-tuning.md`, section 3). An older `tessera-pw` locked
  anyway and then failed when PipeWire could no longer map memory.
* **Dropouts, clicks**: look at the counters first (`tesseractl metrics`:
  `engine.tick_skips`, `engine.wake_lateness_us{...}`, and per flow `LATE`,
  `HOLES`, `UNDERRUN`, `SKIPS`), then measure the machine with
  `cyclictest` (the timing guide). Common causes in order: the CPU
  frequency governor, a USB or Wi-Fi device interrupting the audio core,
  Energy-Efficient Ethernet on a port, interrupt coalescing, a thermal or
  power throttle on a Raspberry Pi (`vcgencmd get_throttled`), and a
  configured latency below what the platform can do.
* **Latency was refused at start**: lower than the platform's timing floor;
  raise it, or set `allow_unsafe_latency = true` for that device knowing the
  consequence.

## PipeWire client (`tessera-pw`)

| Symptom | Likely cause and fix |
|---|---|
| no `tessera` nodes in `wpctl status` | service not enabled for your user (`systemctl --user enable --now tessera-pw`); or it is waiting for the daemon (`journalctl --user -u tessera-pw`) |
| `Permission denied` on `audio.sock` in its log | your user is not in group `tessera` (log in again after `usermod`) |
| connects, then nodes vanish after three queries | the device is missing from the daemon (renamed or removed); `--missing-queries` changes the tolerance |
| stops every time PipeWire restarts | by design (`BindsTo=pipewire.service`); it comes back with PipeWire |
| nodes exist but are silent | the daemon is down or has no data: the nodes stay and stay silent, and the client reconnects with back-off. Check `systemctl status tesserad` |
| port names did not change after relabelling | PipeWire fixes port names at creation; the node's properties update, the port names refresh when the node is recreated (restart the client) |
| `tessera-pw` is not installed on a cross-built package set | the PipeWire client is not cross-built; build it on the target |

`pw-top` shows whether the graph is driven by a tessera node or a hardware
card (a linked hardware card drives; the tessera nodes then follow through a
resampler).

## ALSA

The `tessera-alsa` package is missing from a build made with `SKIP_ALSA=1`.
If `aplay -L` has no `tessera` entry: the package is not installed or
`/etc/alsa/conf.d/60-tessera.conf` is missing (`dpkg -L tessera-alsa`). The
package's snippet defines `pcm.tessera` with default options and a
`tessera_plug` PCM (or use `plug:tessera`). Options: `audio-backends.md`.

| Symptom | Likely cause and fix |
|---|---|
| `aplay -D tessera` fails with `Permission denied` or "cannot connect" | the user is not in group `tessera`, or `tesserad` is not running, or it runs with an empty `[backends] enabled` (no `audio.sock`) |
| open fails, the daemon's user is refused | the plugin talks only to a daemon running as root, `tessera` or yourself (`daemon_uid`); a daemon under another user needs `daemon_uid any` |
| `Invalid argument` for a format or rate | the PCM offers `S16_LE`, `S24_LE`, `S24_3LE`, `S32_LE`, `FLOAT_LE` at the device's rate, read/write access only: open `plug:tessera` |
| an application gets `-ENODEV` | the daemon went away (restart, crash, hung for `overlay_stale` ms); reopen the PCM |
| a capture application such as CamillaDSP used a whole core | fixed: the plugin's poll descriptor is readable only when `avail_min` frames are ready; an older plugin woke the application every engine tick. Update `tessera-alsa` |
| `snd_pcm_wait` returns `-EIO` | the PCM is stopped or in an xrun: kernel-like behaviour of the new poll descriptor; recover with `snd_pcm_prepare` |

`TESSERA_ALSA_STATS=1` in the application's environment prints the plugin's
stream counters to stderr when a stream ends.

## Sound-card bridge (`[[bridge]]`)

`tesseractl bridges` shows one row per bridge, and the last error under the
table.

| Symptom | Likely cause and fix |
|---|---|
| the daemon refuses the configuration: the bridge needs the `alsa-bridge` feature | this `tesserad` was built without it (the default). Build with `--features alsa-bridge` or `TESSERA_DAEMON_FEATURES=alsa-bridge cargo xtask build-deb` |
| `reopening`, "Permission denied" | the service user cannot open `/dev/snd/*`: add `SupplementaryGroups=audio` in a drop-in (`systemctl edit tesserad`) |
| `reopening`, "Device or resource busy" | another program holds the card (or the same card is named twice with different spellings) |
| `reopening`, card not found | the card is missing (`aplay -l`); the bridge starts by itself when it appears. Each attempt also prints `ALSA lib ... Cannot get card index` to the journal (alsa-lib's own message) |
| `failed` | the card cannot run with what the bridge needs (rate, sizes), or a pinned `sample_rate` no longer matches the device; fixed by a configuration change |
| `SLIPS` grows | the card's offset is above `max_correction_ppm`, or a loop downstream fights the bridge: keep `time_constant_s` at least ten times the downstream loop's period |
| `SHORT` is not 0 | the bridge's own resampler did not keep up; check the CPU and the bridge's real-time state (`(no rt)` after the state) |

## Network

The order is the order of dependence.

1. **Interface.** `[network] interfaces` names the media interface; a Wi-Fi
   interface is refused unless `allow_wireless = true`. Without a name, the
   first wired interface with a carrier is used, which may not be the one you
   want on a multi-homed machine. `tesseractl status` shows warnings the daemon raised.
2. **Multicast.** Switch: IGMP snooping *and* an active querier; without a
   querier, snooping prunes the groups after a few minutes and audio stops.
   `tcpdump -n -i eth0 igmp` should show membership reports and queries.
3. **Name discovery.** mDNS is UDP 5353, multicast. Check that
   `avahi-daemon` is running or the built-in responder is selected
   (`[discovery] mdns`), not both responding. `avahi-browse -a -t` lists what
   is announced.
4. **Clock.** `tesseractl clock show` lists the clocks seen; `No clocks`
   means no PTP leader is heard: UDP 319 and 320 multicast must pass the
   switch and any host firewall; check with
   `sudo tcpdump -n -i eth0 udp port 319 or udp port 320`. Only one PTP
   client per interface: stop `ptp4l`, `chronyd` PTP or another PTP daemon
   on the media interface. A device that is a follower and sees no leader
   stays free-running.
5. **Firewall.** `tesserad` uses UDP 319 and 320 (PTP, multicast
   224.0.1.129), the UDP port of every `[[flow]]` (media in, keepalives out
   from the same port), and UDP 5353 when advertising. The control protocol
   ports of the interoperable devices are **not final** (the specification
   still has an open discrepancy about one of them). Until that is settled,
   do not block UDP between media peers on the audio VLAN; restrict by
   interface and VLAN rather than by port. A native receiver
   (`--native-device`) that takes a channel from a multicast flow listens on
   its group (239.255.0.0/16, UDP 4321 by default); if a firewall or the
   switch drops the group, the receiver keeps that input on unicast.
6. **Subscription stuck "in progress"**: no rx flow carries that rx channel
   (or the flow started less than 5 s ago). With the native device role
   (`--native-device`) the daemon requests that flow itself: check that
   `[discovery] browse_devices = true`, that the transmitter is listed
   (`tesseractl routing devices`) with the same sample rate and encoding, and
   the subscription's state in `tesseractl status` (`unresolved`: the
   transmitter or channel was not found; a refusal code or a rate/encoding
   mismatch otherwise). A refused request is retried every 5 s. Without the
   native role, `tesserad` does not implement the flow control that would set
   it up; audio is carried by `[[flow]]` entries (`configuration.md`, "Static
   flows") or by the flows a controller submitted (`tesseractl flows show`).
   Either way the state follows the packets of the flow that carries the
   channel: "receiving", or "no media" when none arrived for a few seconds
   (check the sender, the port and `peer`, item 7).
7. **An rx flow receives nothing**: `tesseractl metrics` shows the flow's
   `PACKETS`. If `engine.rx_filtered` grows, the datagrams come from another
   address than the flow's `peer`. Check the port and the interface
   (`sudo tcpdump -n -i eth0 udp port <port>`); with the `peer` filter a
   transmitter that sends from a second address is refused on purpose.
8. **Wrong-sounding audio on a flow**: the two ends disagree on the channel
   list or encoding. The packet does not carry the channel count
   ; configure both ends alike.
9. **QoS.** Audio and PTP in the wrong queue produce periodic dropouts and
   a wandering clock. Compare the media DSCP settings in the configuration reference with the
   switch configuration and with `tcpdump -vvv` (`tos`).
10. **Wi-Fi, USB Ethernet, VPN, bridges**: not suitable for media.
11. **A hardware transmitter stops after about 1.3 s**:
   keepalives. A unicast transmitter stops a flow whose receiver sends no
   keepalives. Check that `--rx-keepalive-ms` is not `0`, that
   `keepalives_sent` of the rx flow grows (`tesseractl metrics --json`) and
   `keepalive_failures` does not, and that they reach the transmitter
   (`sudo tcpdump -n -i eth0 udp and src port <flow port>`: one datagram
   every 250 ms to the media's source port, nothing blocking it in a host
   firewall). If a device ignores the default empty datagram, try
   `--rx-keepalive-payload 1337`. Keepalives are PROVISIONAL (from the
   provisional wire settings). A transmitter that never started sending is a
   different problem: a hardware transmitter starts a flow only on a flow
   request, which `tesserad` sends only with the native device role
   ([interoperability.md](interoperability.md#native-device-role)).

## A controller lists the device but shows it greyed out or unreachable

The controller found the device in mDNS (device advertisement is on by default)
but gets no answer on the control ports the records advertise: ARC (UDP
4440), CMC (8800) and flow control (4455), each plus 10 per further device.
Either the daemon's native device role or the packaged Controller compatibility
runtime serves them, for the first logical device only.

With the native device role (`--native-device`):

* `journalctl -u tesserad -b | grep 'native device role'` should show `serving
  on <address> (<interface>)`. `cannot bind its ports ... is the compatibility
  runtime still running?` means another process holds them: `sudo systemctl
  disable --now tessera-controller@main`. `waits for the primary interface`
  means the daemon has no primary interface with an IPv4 address yet.
* The role binds again by itself when the primary address changes.

With the compatibility runtime:

* Is it running? `systemctl status tessera-controller@main`. It starts and stops
  with `tesserad` (it is `PartOf` it); a start that fails is retried every 3 s, and
  an instance for a retired device key exits with status 78 and is not retried.
  `journalctl -u tessera-controller@main -b` says why.
* It reads the daemon's wired interface, address, device name and identity when it
  starts. After the address changed (DHCP lease, cable moved to another network),
  restart it: `sudo systemctl restart tessera-controller@main`.
* A second logical device has no listener: its records advertise offset ports on
  which nothing answers, and the daemon warns about each. Disable advertisement
  (`[discovery] advertise_devices = false`) if that matters.
* A site-local `/etc/systemd/system/tessera-controller@.service` overrides the
  packaged unit and prevents the automatic migration of an old username-based
  instance to `@main`; review or remove it.

A locked PTP clock does not imply vendor-controller compatibility. The static
flows of Tessera peers or independently configured peers work without any
controller. See [compatibility limits](interoperability.md).

## A controller cannot change the sample rate or encoding

The device offers a controller exactly the rates and encodings it accepts now,
the same lists `tesseractl --device main status` prints (`can be set to: ...`)
and the portal's Device page offers. When only the current value is left, the
reason is printed under the device table (`main: sample rate fixed at 48000 Hz:
...`): an enabled `[[bridge]]` that sets `sample_rate` pins the rate (by design:
its card runs at that rate), and a `locked` key pins its value. The device then
reports the setting as not changeable, and a controller offers nothing else.

With the native device role (`--native-device`), every rate or encoding write a
controller sends is logged at info level, applied or not:

```sh
journalctl -u tesserad -b --no-pager | grep -E '(sample-rate|encoding) write from'
# ... sample-rate write from 192.0.2.10:51234: flags 0x00000001, value 44100: applied (was 48000)
# ... sample-rate write from 192.0.2.10:51234: flags 0x00000001, value 96000: refused: invalid sample_rate: bridge ...
```

No line means the controller sent this device no write. A request the role does
not implement is logged with its bytes (`native ARC 0x.... from ... answered
unsupported: ...`, once a minute per request type). After a rate change,
receivers of this device that run at the old rate stop receiving until they
change too: that is expected.

If the vendor's controller offers only one rate, the device is pinned:
`tesseractl status` names the bridge whose `sample_rate` fixes it (leave that key
out to let the card follow the device) or the configuration lock. Otherwise the
log above shows whether a write reached the device and what was answered. Some third-party tools refuse a
sample-rate change in their own checks because the device does not answer the
newer flow-list queries; that refusal never reaches the device.

## A change is refused although you are an administrator

* **`device ... is locked against changes (device access lock)`**: the device
  is locked behind a PIN. `tesseractl status` shows `locked` in the `ACCESS`
  column. Unlock it with `tesseractl access unlock` or the portal's Device
  page; a forgotten PIN is reset on the device with `sudo tesseractl access
  reset` ([cli.md](cli.md#access)). A vendor controller shows the padlock but
  cannot unlock it.
* **`rate limited` when unlocking**: five wrong PINs from one kind of client
  pause unlocking from it for 60 s.
* **A controller's factory reset or clear configuration does nothing**: both
  are refused unless `[control] allow_factory_reset = true`
  ([configuration.md](configuration.md#control)); the refusal is logged and the
  device keeps its settings. They are also refused while the device is locked.
* **`... is locked by the configuration`**: a `locked` list of the
  configuration pins that key or domain (`tesseractl config show --provenance`
  names the file); that is not the access lock.

## Network speaker (CamillaDSP and the portal)

A speaker installed with the `speaker` profile plays through CamillaDSP
(`dsp.md`). Start with `systemctl status tesserad camilladsp camillagui
tessera-web` and `journalctl -u camilladsp -b`; the portal's Speaker page shows
the engine's state and names the usual causes in a notice.

| Symptom | Likely cause and fix |
|---|---|
| silent, the daemon receives (`tesseractl metrics` packets grow) and `camilladsp` is not running or keeps restarting | no engine can start: the card is missing (`aplay -l`; the engine keeps retrying until it appears), the card is held by another program (a leftover `[[bridge]]` of the tessera-only speaker: remove it or reinstall with `--no-dsp`), or one of the next two rows |
| Speaker page: **EQ engine input rate mismatch** | the engine's ALSA capture rate (`capture_samplerate`, else `samplerate`) differs from the tessera device's rate, and the PCM `tessera` accepts only the device's rate. Press the fix button, or set `capture_samplerate` to the device's rate in the GUI. The notice appears only when the configuration can be read; one that cannot is logged once by the portal |
| Speaker page: **EQ engine output format not supported** | the card refuses the playback `format` (`snd_pcm_hw_params_set_format … Invalid argument`). In the GUI set `devices.playback.format` to one the card plays: `S32_LE`, `S24_3_LE`, `S24_4_LE` or `S16_LE`. Many USB DACs offer only `S16_LE` and `S24_3_LE`. The installer seeds the first the card accepts |
| silent after every network apply, Revert or expiry (0.2.2 speakers) | the engine was only `PartOf` the daemon and stayed stopped when the daemon started again. 0.3.0 links it into `tesserad.service.wants`: `systemctl show -p WantedBy camilladsp` must list `tesserad.service`; if not, `sudo systemctl enable camilladsp`. An engine you switched off (`systemctl disable`, the Speaker page's switch) stays off |
| Volume pane: "The sound card `<name>` was not found" | the card is not connected, `[appliance.volume] card` is not its name (the word in brackets in `aplay -l`), or the portal's unit may not open its control device. A drop-in written by an installer before 0.3.0 names one `controlC<n>`: replace that line with the lines of `/usr/share/doc/tessera-web/examples/appliance-paths.conf` (`web-portal.md`, "Files and permissions") |
| Volume pane: "The output is muted" | the card's playback switch is off (`[off]` in `amixer sget`), so the slider moves and nothing is heard. Press **Unmute** (it runs `amixer sset <control> unmute`); the portal never unmutes by itself |
| volume back at its default after a reboot | 0.3.0 saves the value in `/var/lib/tessera-web/volume.json` and sets it again at start. Missing file after a change: the unit has no state directory (a manual install without the packaged unit), and the commit said it will not survive a restart. `journalctl -u tessera-web -b` names a card it is still waiting for (looks every 5 s for up to 10 minutes) |
| everyone is signed out when a USB DAC is plugged in | by design: a udev rule starts `tessera-web-sound.service`, which restarts the portal 5 s later so the new card is reachable. It happens only where the portal has a Volume pane; sign in again |
| no **EQ engine (CamillaDSP)** button | `[appliance.camilladsp] gui_url` is not set, or the engine is off ("engine off"); the speaker profile sets it |
| CamillaGUI (port 5005) is open to the network | it has no login. 0.3.0 listens on loopback or on the management address only; the first candidates listened on every interface (`0.0.0.0`), which the installer narrows on upgrade (in the file; the running GUI keeps listening on every interface until `systemctl restart camillagui`). Check `bind_address` in `/etc/camilladsp/camillagui.yml`; `ss -ltn sport = :5005` shows what listens (`dsp.md`) |
| the installer says the GUI listens on this machine only: "the management port is unknown" | the speaker has several default routes (Ethernet and Wi-Fi), or the port has no IPv4 address. Name the port with `[network] interface = "eth0"` in `/etc/tessera/web.toml` or a `web.d` file and install again (a plain `install` of the installed version applies the GUI's address and the portal's link, restarts `camillagui` if it runs, and installs nothing; no `--reinstall`), or set `bind_address` in `/etc/camilladsp/camillagui.yml` to the speaker's address and `systemctl restart camillagui` |
| the EQ GUI does not start after an address change | `bind_address` holds the management address that was set at install; edit it (and `gui_url` in `/etc/tessera/web.d/60-speaker.toml`) or give the speaker a fixed address. `journalctl -u camillagui` says "Cannot assign requested address" |
| the old engine unit of mine is gone from `/etc/systemd/system` | the installer moved it to `camilladsp.service.pre-tessera-dsp` (and its drop-in directory to `camilladsp.service.d.pre-tessera-dsp`) so the packaged unit takes over; `bundle.md` says how to go back |
| the portal lost the services I had configured | the profile's `web.d/60-speaker.toml` of an earlier build carried a service list, and a list in a drop-in replaces the whole list. Put your list in a `web.d` file that sorts after it (for example `70-services.toml`) and regenerate the polkit rules with `tessera-web appliance polkit`; this release writes no list over yours |
| Signal boost pane says there is no slider | the active configuration lacks the gain filter named by `gain_filter` (default `dashboard_signal_boost`); the seeded `Speaker.yml` has it |

## Add-on bundle (`tessera-bundle`)

| Message or symptom | Cause and fix |
|---|---|
| `untrusted_key: no trusted release key is installed. Trusted keys are read from /etc/tessera/trusted-release-keys …` | an installer from 0.5.1 or earlier has no built-in key and the file is absent. Use the 0.5.2 or later bundle, or provision the release's public key from a channel you trust (`bundle.md`, "Trusted release keys"), or, for a development build only, install with `--allow-unsigned`. Nothing was changed |
| `untrusted_key: the release is not signed by any trusted release key` | a key file exists but none of its keys vouches for this archive: wrong archive, wrong key or an unsigned build. Do not add a key because it came with the download |
| `bad_signature`, `wrong_product`, `wrong_version`, `missing_file`, `checksum_mismatch` | the archive is damaged or altered, or its parts come from different releases. Download it again, check it against the release's `SHA256SUMS`, and extract it into an empty directory |
| `malformed: trusted keys require a root-owned regular file with mode 0644`, or `trusted-key parent must be root-owned and not group/world-writable` | fix the owner and mode of `/etc/tessera/trusted-release-keys` and of its directories; symlinks are refused |
| `WARNING: --allow-unsigned is for development only` | expected with that option; the checksum, manifest and platform checks still ran |
| `unknown option …`, `install.sh: No such file` | the 0.2.x scripts are gone. Run `tessera-bundle install` with the same options; `verify` takes none |
| `refusing downgrade` | the installed version is newer than the bundle |
| the install refuses because of a `[[bridge]]` in `/etc/tessera/config.d/60-speaker.toml` | a bridge that is not the speaker profile's own cannot share the card with CamillaDSP. Remove it, or install with `--no-dsp` |
| `restart pending` | not an error: the new binary runs after `sudo systemctl restart tesserad` (or `install --restart`), which interrupts audio. After a restart or a reboot the next `install` prints `restart done` and clears it |
| `install: dependencies: missing dependency libasound2` (a bundle built before 0.3.0-7) | the bundle names the virtual package `libasound2`; current installers accept the package that provides it (`libasound2t64` on trixie). Use the new bundle, or install with the newer `tessera-bundle` from it |
| `apt cannot install the missing dependencies (…)` | apt could not resolve the listed dependencies (package lists out of date, no network, a name that does not exist). Nothing was changed and no record was written; run `sudo apt-get update`, or install them yourself and use `--offline` |
| a failed install after packages were installed | the record is left incomplete with the failed step named. Fix the cause and run the same command again, or `tessera-bundle uninstall` |
| the experimental Ubuntu archive is refused | it needs `--allow-ubuntu-24.04` and Ubuntu 24.04 arm64; other releases and amd64 are refused, and a Debian archive does not install on Ubuntu |

## Network page and `tessera-network`

| Symptom | Likely cause and fix |
|---|---|
| Keep answers `wrong_address` (409) | the policy `require_management_address` needs the request to arrive at the new management address. Open the address the confirmation page shows, or an administrator sets `[network] confirm = "token_only"` in `web.toml` (`network-vlans.md`) |
| `still_applying` (409) | the change is still being applied or recovered; try again in a moment. A Revert waits up to five seconds for the apply worker first |
| the Network page says the recovery is pending and Keep is unavailable; a Revert answers `apply_failed` / `recover` (409) | the previous settings could not be verified yet (the page, `journalctl -t tessera-network` and `tessera-network accept-current` give the connection, the property and both values). The speaker keeps playing and the helper retries every ten seconds; if it cannot settle, run `sudo tessera-network accept-current` to see why and `--yes` to keep the network as it is (`network-vlans.md`, "When a recovery cannot finish") |
| `token_mismatch` (403), then `rate_limited` (429) | five failed token attempts per minute and client are allowed across Pending, Keep and Revert together. Wait a minute |
| `not_pending`, `expired` (410) | the change ended (kept, reverted or expired); the previous or new configuration is in place, check the Network page |
| "Network configuration refused: …" at adoption or Apply | the host's network configuration cannot be represented without losing something. The reason names it: `ambiguous_backend` (NetworkManager and networkd both manage the port), `unknown_backend`, `wired_port_required`, `attached_interfaces`, `foreign_vlan`, `ipv6_configuration`, `mixed_dhcp_static`, `multiple_addresses`, `policy_routing`, `unsupported_dhcp_client` (ifupdown needs ISC dhclient), `ifupdown_main_stanza` / `ifupdown_include_missing` (move the parent stanza to `/etc/network/interfaces.d/tessera-<interface>` and mark it handed off), and others. `unrepresentable_configuration (file: key)` names where it found the setting. Nothing is changed. With several wired ports, set `[network] interface = "eth0"` in `web.toml` |
| search domains refused | more than six, an invalid name (a trailing dot, a `~` routing domain), or domains on a network that is not management. A `tessera-network` or NDIMon-R release that predates search domains rejects a document that carries them: upgrade both ends before configuring any |
| management address lost after Apply | do nothing for 60 seconds after activation: without a Keep the device-side timer restores the previous configuration, and boot recovery does it after a reboot. Waiting is safe; pressing Keep from an address that works confirms |
| the host kept its VLANs after removing the packages | by design: the committed Netplan/networkd files, the VLAN links and `/etc/modules-load.d/tessera-8021q.conf` stay so that removal never changes the host's addressing. Delete them by hand if unwanted |
| `handoff` refuses | `tessera-network handoff status` shows the offer; an offer is cancelled with `handoff cancel <id>` until the receiver reserves its commit. Review the impact token before `commit`; changed impact needs a new review |

## Controller flows

* **A warning names `90-probe-flows.toml`.** The file is deprecated and still read
  while no controller flow set is stored. Intentional host-owned flows can be
  stored with `tesseractl flows set FILE`. Runtime-produced carriers use the
  guarded runtime report and recovery paths; importing their bytes does not
  establish runtime ownership.
* **`flows is locked by the configuration`.** The host owns the flows
  (`[control] locked = ["flows"]`), as on an NDIMon appliance. Change the host's
  configuration, not the daemon.
* **A restart of `tesserad` must not discard host-owned flows.** The adapter submits
  complete reports, including an empty startup report. The daemon protects
  configured, host-owned and restored carriers from automatic acquisition.
  Explicit carrier recovery can authorize a reviewed scope for a new run;
  matching bytes alone cannot. `tesseractl flows show` lists effective content.
* **`runtime flows require the guarded RuntimeReport request`.** Update the
  adapter together with the daemon, launcher and CLI. A compatibility-unit
  process cannot use the intentional whole-set setter, even with administrator
  group membership or a protocol label. Genuine administrator flow writes stay
  available. Unavailable sender identity refuses the write.
* **`runtime integration executables have different or unqualified builds`.**
  Install the core binaries from one source build and restart the compatibility
  unit through the normal service procedure. Registration requires matching
  executable roles, display versions and own-source build IDs. A replacement
  during a live run also requires new registration. Do not clear state or
  change carrier ownership to bypass a package mismatch.

`cargo xtask build-deb` supplies the source build ID automatically. For a
development build on a build target, freeze the sources first and pass the same
`TESSERA_RUNTIME_BUILD_ID` from `cargo xtask runtime-build-id` to the daemon,
launcher, adapter and CLI builds. An unlabeled `cargo build` does not qualify
runtime integration. The ID describes our integration sources; it establishes
neither compatibility-runtime provenance nor routing or audio acceptance.

## Packages

* **conffile prompt on upgrade**: you changed `/etc/tessera/config.toml`. Your
  settings belong in `config.d/`: move them there (a file such as
  `config.d/50-site.toml`), then take the new reference at the prompt (or keep
  your file; either way nothing is lost). To read the reference of the installed
  version run `tesserad --print-default-config`; a copy is in
  `/usr/share/doc/tessera/examples/config.toml` unless the image strips
  `/usr/share/doc`.
* **The daemon was not restarted by the upgrade**: intended; `sudo systemctl
  restart tesserad` when audio can pause.
* **`purge` left the user or the groups**: intended (sysusers semantics),
  see `user-guide.md`, section 9.
* **`dpkg: dependency problems`**: install with `sudo apt install ./file.deb`
  so that `apt` resolves them; `tessera-pw`, `tessera-alsa`, `tessera-web` and
  `tessera-dsp` need the same version of `tessera` (`tessera-dsp` also of
  `tessera-alsa`): upgrade them in one `apt install`.
* **`Duplicate line for path "/etc/polkit-1/rules.d"` at every boot**: 0.2.x
  packages declared polkit's directory in their tmpfiles file. 0.3.0 does not;
  the message is gone after the upgrade, which also puts the directory back to
  polkitd's mode 0750 when 0.2.x had left it at 0755.
* **`iproute2 set to manually installed`** in an installer's output: fixed in the
  0.3.0 bundle, which no longer names a dependency that is already installed.
* **The socket-buffer ceilings stayed raised after a purge**: 0.2.x packages did not
  put `net.core.rmem_max` and `wmem_max` back. 0.3.0 records the values it replaces
  in `/var/lib/tessera-package` and restores them on removal.
* **Reproduce a clean state**: `sudo apt purge tessera tessera-pw`, remove the
  user and groups by hand, install again. For a bundle install, use
  `tessera-bundle uninstall --purge`.
* **`cargo xtask build-deb` fails linking the ALSA plugin** (`cannot find -lasound`):
  install `libasound2-dev`, or build with `SKIP_ALSA=1`. In a cross build the
  plugin needs the target's libasound; use `SKIP_ALSA=1` unless it is
  installed.
* **The bridge is missing from your packages**: the default build has no
  `alsa-bridge`; rebuild with `TESSERA_DAEMON_FEATURES=alsa-bridge`.

## Collecting a report

```sh
tesseractl --json status                      > status.json
tesseractl metrics                            > metrics.txt
tesseractl --json metrics                     > metrics.json
tesseractl bridges                            > bridges.txt
tesseractl config show --effective             > config.txt 2>&1
tesseractl config show --provenance            >> config.txt 2>&1
journalctl -u tesserad -b --no-pager          > tesserad.log
journalctl --user -u tessera-pw -b --no-pager > tessera-pw.log
systemctl status tesserad --no-pager          > unit.txt
uname -a; dpkg -l 'tessera*' ; ethtool -T eth0 2>&1
```

Check the files for names and addresses you do not want to publish before you
attach them.


## Recovering an uncertain routing operation

In builds that provide routing recovery, open **Routing → Recover uncertain
operation**. Review every input's current source against the saved source and
original request. Accept the observed baseline only when those selections are
what you want. The daemon reads the receiver again before saving; a changed
selection cancels acceptance. This sends no receiver routing command and does
not repeat the original request. Its original outcome remains unknown.

The same review is available with `tesseractl routing inspect` and
`GET /api/v1/routing/operation`. Accept its single-use review with
`tesseractl routing reconcile --review TOKEN`, or the typed
`ReconcileReceiverOperation` command through `POST /api/v1/commands`.
Administrator access is required. Wait for the retired worker/startup quiet
interval and resolve any routing lock before accepting. A changed receiver
identity or binding requires explicit migration rather than automatic adoption.

Carrier protection after restart is separate. Accepting selections never clears
host flow/vacancy protection or assigns restored carriers to a new runtime.
Do not edit the managed state file or delete unrelated flows to bypass this.

### Reviewing carriers after a runtime restart

In builds that provide carrier recovery, open **Routing → Review restart
recovery**. The review lists each stored carrier and its recorded runtime
origin. Only entries recorded as belonging to an earlier authenticated runtime
are eligible. Unknown origins, host-owned entries, configured flows and
protected empty slots remain protected. Matching carrier names or bytes cannot
establish ownership.

**Return eligible carriers to runtime management** changes ownership metadata
only. It sends no receiver command and changes no stored carrier bytes or saved
selections. A later complete runtime report may replace or remove those whole
carriers, so review their details and affected audio before confirming. Release
does not replay a refused report or make routing ready automatically.

The same inspection is available with `tesseractl routing inspect-carriers`
and `GET /api/v1/routing/carrier-recovery`. An administrator can accept the
single-use, 30-second review with `tesseractl routing release-carriers --review
TOKEN`, or `ReleaseRuntimeCarriers` through `POST /api/v1/commands`. The daemon
rechecks the runtime, complete reviewed state, locks and retired worker before
persisting. A changed or expired review requires another inspection. An
uncertain receiver operation must be reconciled first. A failed save keeps the
previous protection; retrying requires a fresh review.

### Authorizing the next runtime start

If a startup report was refused, returning recorded carriers to the current
run cannot produce a fresh report. The recovery page instead offers
**Authorize the next runtime start to replace these carriers** when the domain
is eligible. This is a new administrator decision, including carriers whose
origin is unknown; it does not establish their historical origin.

Review every stored carrier of the first local device, its saved receiver
selections and the configured carriers before confirming. The authorization
changes no carrier bytes or receiver selections. It does not clear intentional
host protection or protected empty slots. Configured first-device carriers,
locks, uncertainty or an unfinished receiver worker prevent authorization.

The equivalent command is `tesseractl routing grant-next-run --review TOKEN`,
using the separate `next_run_review` from a fresh `routing inspect-carriers`
response, or `GrantRuntimeCarriersToNextRun` through `POST /api/v1/commands`.
Reviews expire after 30 seconds and can be used once. Machine-peer credentials
cannot grant this permission.

Use the attended service update procedure to restart the runtime; the portal
does not restart it. Restarting can interrupt audio. Only the next
authenticated run can consume the authorization, before it executes, and only
its own accepted complete report establishes routing readiness. Its first report
may replace or remove the reviewed whole carriers. Other-device content and
saved receiver selections remain protected.

A daemon restart, host flow write, changed binding or changed saved selections
invalidates the authorization. A failed save grants no rights. If the new run
ends without an accepted report, no later run inherits the permission; inspect
again rather than editing the state file or replaying an old report.
