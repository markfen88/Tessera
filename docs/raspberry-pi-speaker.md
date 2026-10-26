# Raspberry Pi speaker

A network speaker built from a Raspberry Pi 4, a DAC HAT, Debian 13 (arm64)
and CamillaDSP for per-channel EQ. tessera follows the network's PTP clock and
receives the audio; CamillaDSP filters it and plays it on the DAC. These are configuration patterns for a dedicated wired speaker. Hardware audio
and long-term clock behavior remain experimental; see [compatibility limits](interoperability.md).

> **Shortcut.** Since 0.2.2 the `speaker` profile of the bundle sets up the *Direct* topology below
> for you: `tessera-dsp` installs CamillaDSP and its GUI, writes the device configuration without a
> bridge, seeds a flat CamillaDSP configuration for your card and turns on the portal's Speaker page
> ([dsp.md](dsp.md)):
>
> ```sh
> sudo ./tessera-bundle install --profile speaker --audio-card hw:CARD=<card> --yes
> ```
>
> The installer authenticates the bundle first: provision the release key, or add `--allow-unsigned`
> for a build that has none ([bundle.md](bundle.md#verifying-and-installing)). Install with `--no-dsp`
> to get tessera alone with the in-daemon bridge. The rest of this page explains what the installer
> does and the manual route (including the loopback variant).
>
> **A speaker that already runs CamillaDSP of its own** (a hand-made `camilladsp.service`, a
> program and configurations in `/opt/camilladsp`) keeps them: the program, the configurations, the
> state file and your portal service list are not touched, the EQ GUI is not added, and your unit
> file is moved aside to `camilladsp.service.pre-tessera-dsp` so the packaged unit (which starts and
> stops with the daemon) takes over. `uninstall --purge` deletes only what the bundle made. If the
> speaker has Ethernet and Wi-Fi, name the management port with `[network] interface = "eth0"` in
> `/etc/tessera/web.d/` before installing, so the EQ GUI (when it is added) listens on that address
> only ([dsp.md](dsp.md)).

## Two topologies

Pick **Direct** unless the speaker must also play local sources.

| | Direct: CamillaDSP reads `pcm.tessera` | Loopback bridge: the bridge plays into `snd-aloop`, CamillaDSP reads the loopback |
|--|-----------------------------------|-----------------------------------------------------------|
| Path | network → `tesserad` → ALSA plugin → CamillaDSP (resampler) → DAC | network → `tesserad` bridge (resampler, slow loop) → `snd-aloop` → CamillaDSP (rate adjust of the loopback) → DAC |
| Clock crossings | one: CamillaDSP's resampler between the media clock and the DAC | two loops in series: the bridge follows the loopback, CamillaDSP keeps the loopback in step with the DAC |
| Needs | `tessera-alsa` (the plugin) | `tesserad` built with `alsa-bridge`, `snd-aloop`, `SupplementaryGroups=audio` |
| Local sources (AirPlay, Squeezelite, Bluetooth) | not served | intended to share the loopback through dmix (that ALSA configuration is not provided) |

---

## Common setup

1. **OS.** Debian 13 arm64 on the Pi, wired Ethernet (never Wi-Fi for media:
   `dtoverlay=disable-wifi` in `/boot/firmware/config.txt` if it is unused).
   The Pi 4 has no PTP hardware clock: software timestamps only.
2. **Packages.** Build natively on the Pi (or another arm64 machine); use native
   builds ([user guide](user-guide.md#10-building-the-packages)).
   `SKIP_PW=1` leaves out the PipeWire client, which a headless speaker does
   not need (and which would need the PipeWire development headers):

   ```sh
   SKIP_PW=1 cargo xtask build-deb                                       # Direct
   SKIP_PW=1 TESSERA_DAEMON_FEATURES=alsa-bridge cargo xtask build-deb   # Loopback bridge
   sudo apt install ./target/deb/tessera_*_arm64.deb ./target/deb/tessera-alsa_*_arm64.deb
   ```

3. **Device.** One device with the receive channels the speaker plays. This example uses 96 kHz and 24 bit:

   ```toml
   # /etc/tessera/config.d/50-speaker.toml
   [network]
   interfaces = ["eth0"]

   [clock]
   role = "follower"                 # the network has a hardware PTP leader

   [[device]]
   id = "main"
   name = "<speaker-name>"           # at most 31 bytes
   rx_channels = 2
   tx_channels = 0
   sample_rate = 96000
   encoding = 24
   locked = ["sample_rate"]
   ```

4. **Flows.** Configure a matching static peer flow. Vendor-controller
   subscriptions do not automatically configure Tessera.

   A `[[flow]]` written by hand opens a receive port, or sends to a fixed
   address. It works between tessera instances, or with a transmitter that can
   be set up to send unicast to a fixed address and port
   ([configuration.md](configuration.md#static-flows-provisional)). For
   example, from another tessera device that sends channels 1 and 2 to this
   Pi:

   ```toml
   [[flow]]
   id = "from-transmitter"
   direction = "rx"
   device = "main"
   channels = [1, 2]
   peer = "192.0.2.30"               # the transmitter's address
   port = 5004                       # the receive port the transmitter sends to
   ```

   The channel count is not on the wire: the transmitter's flow must carry the
   same channels in the same order (two here). Receiver keepalives (default
   every 250 ms) keep a hardware transmitter sending once it has started; they
   do not start a flow, and without them it stops after about 1.3 s.

5. **Start and check:** `sudo systemctl start tesserad`, then
   `tesseractl clock show` (state `locked`), `tesseractl metrics` (the flow's
   `PACKETS` grow, `HOLES` and `LATE` stay still).

---

## Direct: CamillaDSP on pcm.tessera

CamillaDSP captures the ALSA PCM `tessera` (the media clock) and plays to the
DAC (its own crystal). Its rate adjust with an asynchronous resampler is the
only clock crossing.

```text
 network ──► tesserad ──RX ring──► pcm.tessera ──► CamillaDSP [AsyncSinc, rate adjust] ──► DAC
            (media clock)                           (EQ per channel)                      (crystal)
```

1. Install `tessera-alsa`. The user that runs CamillaDSP must be in group
   `tessera` (and in `audio` for the DAC).
2. Configure CamillaDSP's devices. The keys below are the ones this setup
   depends on; check their exact spelling and the rest of the file against the
   documentation of your CamillaDSP version:

   ```yaml
   devices:
     samplerate: 48000              # processing rate; see the note below
     capture_samplerate: 48000      # must equal the tessera device's sample_rate
     chunksize: 1024
     enable_rate_adjust: true       # follow the DAC against the capture side
     resampler:
       type: AsyncSinc              # asynchronous resampler: the clock crossing
     capture:
       type: Alsa
       device: "tessera"            # pcm.tessera; channels = the device's RX channels used
       channels: 2
     playback:
       type: Alsa
       device: "hw:CARD=<card>,DEV=0"
       channels: 2
   ```

   The capture side opens `pcm.tessera` at `capture_samplerate`, or at
   `samplerate` when that is unset, and the plugin accepts **only the device's
   rate** (`sample_rate` of the `[[device]]`, 48000 in the speaker profile).
   A mismatch makes CamillaDSP stop at start with
   `Capture error: ALSA function 'snd_pcm_hw_params_set_rate' failed with error
   'Invalid argument (22)'`, and the speaker stays silent while `tesserad` keeps
   receiving. To process at a higher rate (for example `samplerate: 96000`),
   keep `capture_samplerate` at the device's rate; the AsyncSinc resampler then
   converts as part of the clock crossing, at some CPU cost and a little latency.

   The portal's Speaker page shows a capture-rate mismatch, both rates and
   the active configuration, and checks again after a device-rate change.
   When a resampler is configured, or the processing rate already matches
   the device, its one-click fix sets `capture_samplerate` to the device's
   rate and reloads the EQ engine, keeping the file's comments and order.

   The plugin offers `S16_LE`, `S24_LE`, `S24_3LE`, `S32_LE` and `FLOAT_LE` at
   the device's rate. The card has its own list: a format it refuses stops the
   engine with `snd_pcm_hw_params_set_format … Invalid argument`, and the Speaker
   page then names the device and format and says to change `format` under
   `devices` → `playback` ([dsp.md](dsp.md)). Another device or channel range: define your own PCM
   ([audio-backends.md](audio-backends.md#configuration)).
3. Start CamillaDSP after `tesserad`. If the daemon restarts, the PCM reports
   `-ENODEV` and CamillaDSP must reopen it.

Local sources cannot share this path: CamillaDSP reads only the network audio.

---

## Loopback bridge: the bridge plays into snd-aloop

The bridge plays the network audio into one end of an `snd-aloop` cable with a
slow loop; CamillaDSP captures the other end with its own rate adjust (it trims
the loopback's `PCM Rate Shift` to follow the DAC). The intent is that other
local players write into the same loopback through an ALSA `dmix`, so
CamillaDSP filters everything; that part is not provided (see below). This
topology is experimental; validate hardware compatibility for your installation.

```text
 network ──► tesserad bridge [resampler, τ = 120 s] ──► hw:Loopback,0,0 ═══ hw:Loopback,1,0 ──► CamillaDSP ──► DAC
                                                            ▲ (dmix: intended, not provided)   (rate adjust: PCM Rate Shift)
 AirPlay / Squeezelite / Bluetooth ─────────────────────────┘
```

1. `tesserad` with `alsa-bridge`, `SupplementaryGroups=audio`
   ([audio-backends.md](audio-backends.md#prerequisites)), and `snd-aloop`:

   ```sh
   echo snd-aloop | sudo tee /etc/modules-load.d/snd-aloop.conf
   sudo modprobe snd-aloop
   ```

2. The bridge, slow and gentle (these are the defaults):

   ```toml
   [[bridge]]
   name = "to-camilla"
   device = "main"
   alsa_pcm = "hw:Loopback,0,0"
   direction = "playback"
   channels = 2
   time_constant_s = 120             # at least 10 x CamillaDSP's adjust_period
   max_correction_ppm = 200
   ```

3. CamillaDSP captures `hw:Loopback,1,0` at the device's rate, 2 channels, in
   the format the bridge opened (S32 when offered), with `enable_rate_adjust:
   true`.

**The two loops must not fight.** The bridge's time constant must be at least
ten times CamillaDSP's `adjust_period` (10 s by default), so that the bridge
only follows the average rate CamillaDSP settles on. The default of 120 s
meets this for the default period; a longer `adjust_period` needs a longer
time constant. Keep the clamp no larger than the offsets you expect.

Sharing the loopback with local sources: the bridge opens `hw:Loopback,0,0`
directly, and a loopback cable pairs playback subdevice `0,N` only with
capture `1,N`, so local players must reach the same cable (through a `dmix`
that the bridge's open has to fit into). A complete shared-source ALSA configuration is not provided; configure that
separately for your sound card and applications.

## Timing and diagnostics

Keep the conservative ARM latency default while configuring the speaker. Use
adequate cooling and power, and inspect clock state, flow counters and bridge
xruns/slips before lowering buffering. A downstream DSP and the hardware bridge
can have separate rate servos; keep their time constants sufficiently separated.
[Timing and privileges](rt-tuning.md) explains scheduling and clock-domain limits.

Bluetooth routing, automatic latency calibration and a complete shared-source
speaker configuration are outside this release.
