# Troubleshooting

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
entries, receiver keepalives, optional mDNS advertising, the local clients
and (with `alsa-bridge`) the sound-card bridge. It does not implement the
vendor's control protocols, so nothing a controller asks for is carried out
by `tesserad` itself.

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
package's snippet defines only `pcm.tessera` with default options; there is
no `tessera_plug` PCM in it (use `plug:tessera`). Options: `audio-backends.md`.

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
| the daemon refuses the configuration: the bridge needs the `alsa-bridge` feature | this `tesserad` was built without it (the default). Build with `--features alsa-bridge` or `TESSERA_DAEMON_FEATURES=alsa-bridge packaging/build-deb.sh` |
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
   still has an open discrepancy about one of them). Until captures settle it,
   do not block UDP between media peers on the audio VLAN; restrict by
   interface and VLAN rather than by port.
6. **Subscription stuck "in progress"**: no rx `[[flow]]` carries that rx channel
   (or the flow started less than 5 s ago). A subscription is stored by name, but
   `tesserad` does not implement the flow control that would set it up; audio is
   carried by `[[flow]]` entries (`configuration.md`, "Static flows"), and the
   state follows the packets of the flow that carries the channel: "receiving",
   or "no media" when none arrived for a few seconds (check the sender, the port
   and `peer`, item 7).
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
   request, which `tesserad` does not send ([interoperability.md](interoperability.md)).

## A controller lists the device but shows it greyed out or unreachable

The controller found the device in mDNS (so `[discovery] advertise_devices =
true`) but gets no answer on the control ports the records advertise: ARC (UDP
4440), CMC (8800) and flow control (4455), each plus 10 per further device.
The control protocols are not served by `tesserad` yet. This is an implemented
feature boundary, so changing VLANs or restarting a controller cannot add it.
Use matching static flows with Tessera peers or independently configured peers.
Optional advertisement can list ports that have no listener; the daemon warns
about these ports. Disable device advertisement unless it is deliberately needed.
A locked PTP clock does not imply vendor-controller compatibility. See
[compatibility limits](interoperability.md).

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
  so that `apt` resolves them; `tessera-pw` needs the same version of
  `tessera`.
* **Reproduce a clean state**: `sudo apt purge tessera tessera-pw`, remove the
  user and groups by hand, install again.
* **`build-deb.sh` fails linking the ALSA plugin** (`cannot find -lasound`):
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
