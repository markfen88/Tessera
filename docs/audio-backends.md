# Audio backends

How audio gets between a Tessera device and Linux. All backends use the
daemon's media clock and configured RX/TX channel ranges.

| Backend | Runs in | Whose clock | Resampling | Package / build |
|---------|---------|-------------|------------|-----------------|
| [PipeWire (`tessera-pw`)](#pipewire-tessera-pw) | a user-session process | the media clock when it drives the graph; a hardware card's when that drives | only when following another driver | `tessera-pw` |
| [ALSA plugin (`pcm.tessera`)](#alsa-plugin-pcmtessera) | the application | the media clock | never | `tessera-alsa` |
| [Sound-card bridge (`[[bridge]]`)](#in-daemon-sound-card-bridge-bridge) | `tesserad`, one real-time thread per bridge | the card's, followed by a PI loop | always (unless `resampler = "off"`) | `tesserad` built with `alsa-bridge` |
| [JACK](#jack) | | | | not implemented |

The first two are clients of `/run/tessera/audio.sock`: they attach to a
device's RX or TX channels and get a shared-memory ring, the read-only clock
overlay and a wake socket. The user must be a member of group
`tessera`; any member may attach any device. The bridge uses the
same rings from inside the daemon.

**Rate and latency, for every backend.** The device's sample rate is the
rate of its rings: nothing in tessera converts between nominal rates (the
resamplers only absorb drift around a ratio of 1). Received audio reaches a
client `latency + one engine tick + --rx-margin-us` after its media time: by
default 4 ms + 1 ms + 0.25 ms on x86 and 10 ms + 1 ms + 0.25 ms on ARM (the
default device latency is in
[configuration.md](configuration.md#example-everything-at-its-default); the
lowest latency accepted without `allow_unsafe_latency` is 1 ms on x86 and 2 ms
on ARM). Each backend adds its own buffering on top, listed in its section.

---

## PipeWire (`tessera-pw`)

`tessera-pw` creates, for each device of the daemon, one `Audio/Source` node
(the device's receive channels) and one `Audio/Sink` node (its transmit
channels), named with the prefix `tessera`. Source: `crates/tessera-backend-pipewire`.

```sh
sudo usermod -aG tessera "$USER"                 # then log out and in
systemctl --user enable --now tessera-pw.service
journalctl --user -u tessera-pw -f
wpctl status | grep -i tessera
pw-link -o | grep tessera                         # ports, to link with pw-link, qpwgraph, Helvum
```

| Topic | Behaviour |
|-------|-----------|
| Clock | The nodes are graph drivers timed by the media clock, with `priority.driver` 500 (below hardware cards, about 1000). When nothing else drives, the graph follows the network. When a hardware card drives, the nodes follow it and an asynchronous resampler (`--resampler`, default `cubic` on ARM, `sinc-fast` elsewhere) with a slow PI loop (0.05 Hz, at most 3000 ppm) absorbs the drift |
| Rate | the nodes run at the device's rate (`node.rate`) |
| Period | `--period` 256 frames requested, `--max-period` 2048 |
| TX lead | a cycle's audio is written one period plus 64 frames ahead of the media clock (`--tx-ahead-periods`, `--tx-ahead-margin`), so it leaves the wire that much later |
| Channels | up to 64 per node (`--max-node-channels`); a larger device is split into several nodes |
| Format | asks the daemon for `f32` rings first, `i32` on refusal |
| Daemon restarts | the nodes stay (silent) and the client reconnects with back-off (`--reconnect-initial` 200 ms to `--reconnect-max` 5 s); a silent clock overlay for `--overlay-stale` 2000 ms counts as a hung daemon |
| PipeWire restarts | the unit is bound to `pipewire.service` and comes back with it |
| Device list | read from the control socket (`Query::Status`) every `--discover-interval` 10 s; a device disappears after 3 answers without it; `--static-device KEY:RX:TX[:RATE]` works without the control socket |
| Labels | a relabel updates the node's properties; port names change only when the node is recreated |
| Real time | `--rt-priority` 20 (`SCHED_FIFO` of the control loop; 0 = off), `mlockall` (`--no-mlock`); both need PAM limits for a user service ([rt-tuning.md](rt-tuning.md)) |

All options: [cli.md](cli.md#tessera-pw). Change them with a drop-in:

```sh
systemctl --user edit tessera-pw.service
# [Service]
# ExecStart=
# ExecStart=/usr/bin/tessera-pw --audio-socket /run/tessera/audio.sock --control-socket /run/tessera/control.sock --period 128
```

Headless machines need `sudo loginctl enable-linger USER` so that the user
session, and PipeWire with it, start at boot. The published Ubuntu bundle targets Ubuntu 24.04 arm64 and does not include PipeWire.

---

## ALSA plugin (`pcm.tessera`)

An ALSA ioplug PCM that is a thin client of the daemon: it attaches the
channels it is configured for and copies between the application's buffer and
the ring. Its clock is the media clock, so nothing is resampled. Code:
`crates/tessera-backend-alsa` (feature `plugin`). Needs
alsa-lib 1.2.1 or newer for exact pointer arithmetic. On a network speaker the
client is CamillaDSP, which captures `pcm.tessera` and plays to the sound card
([dsp.md](dsp.md)); the PCM accepts only the device's own rate.

```sh
aplay -D tessera some.wav                             # into the device's TX channels
arecord -D tessera -f S32_LE -r 48000 -c 2 out.wav    # from its RX channels
aplay -D plug:tessera some-44k1.wav                   # let alsa-lib convert format and rate
```

### Configuration

The package installs `/usr/share/alsa/alsa.conf.d/60-tessera.conf` (linked from
`/etc/alsa/conf.d/`). The file is the full, commented sample
(`crates/tessera-backend-alsa/dist/alsa.conf.d/50-tessera.conf` in the source
tree): every option at its default, plus a `tessera_plug` PCM. In its
essentials:

```text
pcm.tessera {
    type tessera
}

pcm.tessera_plug {
    type plug
    slave.pcm "tessera"
}
```

Define your own PCMs in
`~/.asoundrc` or `/etc/asound.conf`:

```text
pcm.tessera_main_rx {            # capture of RX channels 1-8 of device "main"
    type tessera
    device "main"
    capture { first_channel 1  channels 8 }
}
pcm.tessera_out_34 {             # playback into TX channels 3 and 4
    type tessera
    playback { first_channel 3  channels 2 }
}
```

| Key | Default | Meaning |
|-----|---------|---------|
| `device` | `""` | the device key; empty: the daemon's default |
| `channels` | 2 | channels of the PCM |
| `first_channel` | 1 | first device channel, 1-based |
| `rate` | 0 | 0: the device's rate; another value must equal it |
| `socket` | `$TESSERA_AUDIO_SOCK`, else `/run/tessera/audio.sock` | the daemon's audio socket (the environment is ignored in secure-exec processes) |
| `latency` | 64 (frames) | margin added to one period, in **frames** (not microseconds like the device's `latency_us`): playback lead, capture lag |
| `max_buffer` | 16384 | largest buffer in frames (sizes the ring); 16-bit formats may use all of it, 32-bit formats half |
| `ring_format` | `i32_or_f32` | `i32`, `f32`, `i32_or_f32`, `f32_or_i32` |
| `connect_timeout` | 1000 ms | bound for connecting and attaching |
| `overlay_stale` | 2000 ms | a silent clock overlay means a hung daemon (0: never) |
| `client` | `alsa:<process>` | the name in the daemon's status |
| `daemon_uid` | `root,tessera,self` | users the daemon may run as (`SO_PEERCRED` of the socket); `any` turns the check off |

`playback { ... }` and `capture { ... }` override any key per direction (TX
and RX numbering of a device are independent). Unknown keys are errors.

### Behaviour

| Topic | Behaviour |
|-------|-----------|
| Formats | `S16_LE`, `S24_LE`, `S24_3LE`, `S32_LE`, `FLOAT_LE`; interleaved and non-interleaved read/write access; the device's rate. Anything else, mmap included, through `plug:tessera` |
| Playback timeline | frame 0 is written `period + latency` frames (`latency`: the plugin key) ahead of the media clock at `start`; an application that keeps less queued underruns visibly (no write is silently dropped as late); `snd_pcm_delay` includes the lead, so an application that steers its buffer level by `snd_pcm_delay` must target more than the lead plus one of its own writes (a client that writes one 512-frame period per cycle and targets two periods keeps only about half a period really queued and underruns between writes) |
| Capture timeline | frame 0 is read `effective latency + engine tick + latency` behind the clock (the device's effective latency in time, plus `latency` frames, the plugin key); the engine tick is estimated from the wake socket (5 ms assumed until measured; never below a steady tick, up to about 23 % above it at first), which also sets the minimum period and buffer |
| Poll descriptor | one descriptor (event `POLLIN` in both directions): an epoll set of a **timerfd** and the session's wake socket. The timer is armed from the media clock for the moment `avail >= avail_min` (never sooner than one engine tick plus 0.1 ms), so the descriptor becomes readable when the application can move `avail_min` frames, a drain has ended, or the plugin has seen an xrun, a stop or a lost daemon. The wake socket is in the set only for its hang-up, so a dead daemon wakes the application at once |
| `poll_revents` | `POLLIN`/`POLLOUT` when `avail >= avail_min` and at the end of a drain; `POLLERR` for an xrun, a lost daemon and the states SETUP, OPEN and SUSPENDED (like a kernel driver: `snd_pcm_wait` on a stopped PCM returns `-EIO`) |
| Wake-ups | alsa-lib raises `avail_min` to the period, so an application wakes about `rate / max(avail_min, period)` times a second. |
| Xruns | a step of the media clock (overlay epoch) is an xrun; `prepare` re-anchors; stale audio is never replayed |
| Daemon loss | a closed socket, a `Detach`, or `overlay_stale` without a clock update or a tick disconnects the PCM; later calls return `-ENODEV`; reopening connects to the new daemon |
| Pause, drain | pause silences the output and freezes the pointer; resuming re-anchors. A drain ends when the last frame has been played |
| RT rules | the copy and pointer path does not allocate or log; callbacks make only bounded non-blocking syscalls (one `timerfd_settime` per wake-up cycle, non-blocking `recv` drains, one zero-timeout `poll` per `poll_revents`) |
| Diagnostics | `TESSERA_ALSA_STATS=1` prints the stream counters to stderr when a stream ends |

Limits of the poll descriptor: a clock step, or a daemon
that freezes without closing its socket, is noticed only when the armed timer
fires or the application calls the PCM, so up to `avail_min` late; with
nothing armed (a prepared capture stream, a paused stream with too little
available) not until the application calls the PCM.

The timer-backed poll descriptor lets applications wait until their requested
period is available instead of waking on every daemon tick.

---

## In-daemon sound-card bridge (`[[bridge]]`)

`tesserad` can drive a local sound card itself: no PipeWire, no
`arecord | aplay` pipes, a real-time thread that starts with the daemon and is
designed to reopen a card that goes away (not yet exercised with a real
unplug). Every key: [configuration.md](configuration.md#hardware-card-bridges-bridge).

### Prerequisites

1. A `tesserad` built with the cargo feature `alsa-bridge` (off by default):
   `cargo build --release -p tesserad --features alsa-bridge`, or packages from
   `TESSERA_DAEMON_FEATURES=alsa-bridge cargo xtask build-deb` (the `tessera`
   package then depends on the native ALSA library). A daemon without the feature refuses
   any `[[bridge]]`. The default package build is still without it
.
2. Access to `/dev/snd` for the service user (the unit allows ALSA devices,
   but the files belong to group `audio`):

   ```sh
   sudo systemctl edit tesserad.service
   # [Service]
   # SupplementaryGroups=audio
   ```

### How it works

- Direction `playback` plays the device's RX channels `first_channel ..` on the
  card; `capture` feeds the card's input into the device's TX channels.
- The card opens non-blocking at exactly the device's rate, with the first of
  S32, FLOAT, S24, S24_3, S16 it offers; default period 10 ms, buffer 4 periods,
  3 of them kept queued (30 ms of card latency in playback).
- A resampler around a ratio of 1 and a critically damped PI loop follow the
  card's clock. `time_constant_s` (τ, default 120) and `max_correction_ppm`
  (default 200, at most 3000) tune it. The bridge reserves a headroom of
  `0.4 × max_correction_ppm × 1e-6 × time_constant_s` seconds (9.6 ms at the
  defaults: 0.4 × 200 ppm × 120 s; the loop's own transient is about 8.9 ms),
  plus one period (playback) or two (capture), as added latency (shown as
  `added_latency_frames`), and sizes its ring for it. A configuration whose
  headroom term exceeds 60 s is refused.
- Playback reads `effective latency + guard` behind the media clock: the audio
  leaves the card about `latency + guard + card queue` after its media time.
  Positions the network did not deliver are silence (`missing_frames`). With no
  subscription or flow the card plays continuous digital silence.
- A missing, busy, unplugged or persistently failing card is reopened with
  back-off (0.5 s doubling to 30 s, state `reopening`); a single xrun is
  recovered in place. A reload applies a new `time_constant_s` or
  `max_correction_ppm` to the running loop; any other change restarts only that
  bridge.

### Choosing the time constant

| Situation | `time_constant_s` |
|-----------|-------------------|
| straight to a DAC, nothing else servos the card | 10 to 30 s converges faster |
| into an `snd-aloop` cable read by CamillaDSP with `enable_rate_adjust` (a second loop) | at least 10 × CamillaDSP's `adjust_period` (10 s by default): 100 or more; the default 120 meets it |
| a downstream loop with a longer period | 10 × that period |
| below 4 s | warned: the loop may ring (its error filter has a 0.4 s time constant) |

Set `max_correction_ppm` above the largest offset you expect, but not far
above it (a larger clamp lets a larger transient build up): a card whose
offset is above the clamp cannot be followed and slips (`SLIPS`).

### Recipes

**Headless speaker straight to a DAC:**

```toml
# /etc/tessera/config.d/60-speaker.toml
[[bridge]]
name = "speaker"
device = "main"
alsa_pcm = "hw:CARD=<card>,DEV=0"   # from `aplay -L`
direction = "playback"              # RX channels 1 and 2
channels = 2
time_constant_s = 30
```

**CamillaDSP through a loopback** (the bridge plays into one end of an
`snd-aloop` cable; CamillaDSP captures the other end, runs its filters, plays to
the DAC, and keeps the cable in step with the DAC by trimming the loopback's
`PCM Rate Shift`):

```sh
echo snd-aloop | sudo tee /etc/modules-load.d/snd-aloop.conf
sudo modprobe snd-aloop
```

```toml
[[bridge]]
name = "to-camilla"
device = "main"
alsa_pcm = "hw:Loopback,0,0"        # CamillaDSP captures hw:Loopback,1,0
direction = "playback"
channels = 2
time_constant_s = 120               # >= 10 x CamillaDSP adjust_period
max_correction_ppm = 200
```

Both ends of a loopback cable must agree on rate, channels and format: the
device's rate, 2 channels, and the format the bridge picked (S32 first). Two
spellings of one card (`hw:Loopback,0,0` and `hw:CARD=Loopback,DEV=0`) are not
recognised as the same; the second bridge then stays `reopening` with "busy".

The Raspberry Pi version of both topologies:
[raspberry-pi-speaker.md](raspberry-pi-speaker.md). These remain experimental hardware integration patterns.

### Watching it

```sh
tesseractl bridges          # state, fill, position error, correction and trend in ppm, ratio, xruns, slips, short frames, reopens, last error
tesseractl --json bridges   # every field of BridgeStatus
```

A missing card may cause alsa-lib to print an error on every reopen attempt.
Hardware reconnect and drift behavior depend on the card and driver.

---

## JACK

Not implemented. `tessera-backend-jack` is a placeholder crate, and
`[backends] enabled` accepts `"jack"` without anything to attach. A JACK
application can reach tessera through PipeWire's JACK emulation and
`tessera-pw`; verify the resulting clock and latency configuration for your system.

---

## `[backends]`

| Key | Default | Today |
|-----|---------|-------|
| `enabled` | `["pipewire", "alsa"]` | decides only whether `audio.sock` is opened when `tesserad` runs with `--audio-socket auto` (an empty list: no socket); clients are not filtered by kind |
| `resampler` | `auto` | the bridge's default quality (`[[bridge]] resampler = "auto"`); `auto` is `cubic` on ARM, `sinc-fast` elsewhere. `tessera-pw` has its own `--resampler` |

`tesserad --audio-socket on|off` overrides `enabled`.
