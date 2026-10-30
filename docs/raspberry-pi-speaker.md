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
| Local sources (AirPlay, Squeezelite, Bluetooth) | not served | share the loopback through a `dmix` bus at the device's rate (see [Shared sources](#shared-sources-and-one-sample-rate)) |

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
the loopback's `PCM Rate Shift` to follow the DAC). Other local players can
write into the same loopback through an ALSA `dmix`, so CamillaDSP filters
everything (see [Shared sources](#shared-sources-and-one-sample-rate)). This
topology is experimental; validate hardware compatibility for your installation.

```text
 network ──► tesserad bridge [resampler, τ = 120 s] ──► hw:Loopback,0,0 ═══ hw:Loopback,1,0 ──► CamillaDSP ──► DAC
                                                            ▲ (dmix bus, optional)             (rate adjust: PCM Rate Shift)
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

### Shared sources and one sample rate

A loopback cable pairs playback subdevice `0,N` only with capture `1,N` and
carries one rate, and a `dmix` never converts rates: it takes its rate when its
first client opens it. So the bridge, every local player, the bus and
CamillaDSP must run at the tessera device's rate. With the portal's **rate
follower** that rate is the only setting: change it from the vendor controller,
the portal or `tesseractl set-rate`, and the chain follows; CamillaDSP's own rate
keys are overwritten.

1. The bus reads its rate from a file the portal writes
   (`/var/lib/tessera-rate/alsa.conf`, `tessera.rate <Hz>`; the `tessera-web`
   package creates it with 48000). Each new bus reads it when it opens:

   ```text
   # /etc/asound.conf
   pcm.speaker_dmix {
       type dmix
       ipc_key 2041048               # unique on the host
       ipc_key_add_uid false         # one bus for every user
       ipc_gid 29                    # a group every client is in (getent group audio)
       ipc_perm 0660
       slave {
           pcm "hw:Loopback,0,0"
           format S32_LE
           channels 2
           rate {
               @func refer
               file "/var/lib/tessera-rate/alsa.conf"
               name tessera.rate
           }
       }
   }
   pcm.speaker_mix { type plug; slave.pcm "speaker_dmix"; }   # the local players
   ```

2. The bridge opens the bus itself, not the plug: through `speaker_mix` it
   would resample into a bus left at the old rate, while on `speaker_dmix` its
   open fails until the bus runs at the device's rate, and it retries. Leave
   `sample_rate` out (the bridge follows the device; set, it pins the rate)
   and keep the retry short:

   ```toml
   [[bridge]]
   name = "speaker-playback"
   device = "main"
   alsa_pcm = "speaker_dmix"
   direction = "playback"
   channels = 2
   error_backoff_max_ms = 2000       # back within 2 s of the bus reopening
   ```

3. CamillaDSP captures `hw:Loopback,1,0` (2 channels, `S32_LE`).
4. The follower, in a `/etc/tessera/web.d` drop-in. `restart` lists the players
   that may hold the bus open; each must be an `[[appliance.service]]` unit (a
   member of a group counts):

   ```toml
   [appliance.rate_follow]
   enabled = true
   bus_hw_params = "/proc/asound/Loopback/pcm0p/sub0/hw_params"
   restart = ["squeezelite", "gmediarender", "shairport-sync", "bluealsa-aplay"]
   ```

**A rate change interrupts every source for a few seconds.** Within 2 s the
portal writes the new rate, stops CamillaDSP, restarts the listed players that
run, sets `samplerate` (and a set `capture_samplerate`) in the active
configuration and reloads it; the bus reopens at the new rate with its first
client and the bridge joins at its next retry. A configuration restored or
applied in CamillaDSP's GUI at another rate is corrected the same way. A card
that refuses the rate stops CamillaDSP: the portal retries, waiting 2 s doubling
to 60 s, and the Speaker page shows the error under *Sample rate follows the
network device*; so does a bus that a player outside `restart` holds at the old
rate. Filters must suit every rate: name FIR files with CamillaDSP's
`$samplerate$` token ([web-portal.md](web-portal.md#rate-follower)).

## Timing and diagnostics

Keep the conservative ARM latency default while configuring the speaker. Use
adequate cooling and power, and inspect clock state, flow counters and bridge
xruns/slips before lowering buffering. A downstream DSP and the hardware bridge
can have separate rate servos; keep their time constants sufficiently separated.
[Timing and privileges](rt-tuning.md) explains scheduling and clock-domain limits.

Bluetooth routing, automatic latency calibration and a shared-source
configuration for each player are outside this release.
