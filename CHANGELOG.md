# Tessera changelog

## 0.6.0

Tessera 0.6.0 makes Tessera an audio routing device with two clean layers:
network channels interoperable with Dante® on one side, and local I/O (sound
cards, USB mixers, local applications, speaker outputs) on the other, joined by
a real-time host router. It adds hardware control surfaces (USB HID, USB MIDI,
and MIDI over TCP), custom mixing-desk panels, daemon-owned speaker amplifier
volume, channel-sliced and local-only sound-card bridges, and Network routing
matrix fixes. Interoperability with Dante® remains uncertified.

### Added

- **Host audio router (`/router`, `tesseractl router`, `[[route]]`).**
  - Two-layer model: the host exposes its logical network device's RX (network
    → this host) and TX (this host → network) channels to network controllers;
    local I/O (`app_out`, `host_out`, `card_in`, `card_out`) stays local until
    patched by a crosspoint.
  - Real-time mixing engine in `tesserad`: compiles active crosspoints into a
    flat lock-free plan (`RouteTable`) hot-swapped into the media worker
    without stopping audio or allocating on RT threads. Every destination sums
    every crosspoint that feeds it, with per-crosspoint gain and mute and
    per-port levels and labels. Taps (a destination port used as a source,
    after its own level) are supported; loops through taps are refused.
  - Default grid reproduces 0.5 behaviour (RX *n* → host out *n*, app *n* → TX
    *n* where no capture bridge feeds it, local monitoring, and configured
    `[[route]]` entries) until the first edit saves it; **Reset to default**
    (`ResetRouter`, `tesseractl router reset`) returns to it.
  - Portal `/router` grid with keyboard navigation (arrow keys, Home/End),
    crosspoint and port editors (`/router/crosspoint`, `/router/port`), phone
    destination-card layout, and **Patch cards to the network** (adds TX/RX
    channels with confirmation, labels them, and patches `card_in → network_tx`
    or `network_rx → card_out` in one flow).
  - CLI: `tesseractl router show|patch|unpatch|gain|mute|port-gain|port-mute|set --file|reset`.
- **Sound-card bridges (`/audio`, `tesseractl bridge`).**
  - Runtime bridge management (`SetCardBridge`, `RemoveCardBridge`) alongside
    `[[bridge]]` drop-ins, with in-place channel renaming and device channel
    growth on `/audio`.
  - Channel slicing: `card_first_channel` (CLI `--card-first`, portal **First
    card input / output**) selects a contiguous slice of a multi-channel card
    (for example USB channels 15–16 for an Allen & Heath CQ's Main L/R mix).
  - `local_only = true` bridges open a sound card for the host router only
    (`card_in` / `card_out` ports), keeping it off the network until patched.
    The Audio page can convert an existing network bridge to or from
    `local_only` while preserving its signal path.
  - Low-latency card periods (`--low-latency` / 96 frames, 2 ms at 48 kHz).
- **Local monitoring (`monitor_rx_from_tx`, `SetMonitor`, `tesseractl monitor`).**
  - Mixes a device's local TX inputs into its RX host outputs after TX channel
    gain and mute, with `monitor_delay_us = 0` (`auto`) as default. Superseded
    by the host router for arbitrary mixes; when a router grid is saved,
    `SetMonitor` updates that device's TX-tap → `host_out` crosspoints.
- **Levels and daemon-owned host volume (`[host_volume]`, `tesseractl volume`).**
  - Per-channel input gain and mute (`SetChannelGain`, `SetChannelMute`,
    `tesseractl gain|mute|unmute`), post-mix RX output stage (`SetOutputGain`,
    `SetOutputMute`, `tesseractl output-gain|output-mute`), and router port /
    crosspoint levels (`SetPortGain`, `SetPortMute`, `SetCrosspointGain`,
    `SetCrosspointMute`).
  - `[host_volume]` (`card`, `control`, `min_db`, `max_db`) hands the speaker
    amplifier's ALSA simple mixer control to `tesserad` (using alsa-lib's mixer
    API rather than `amixer`; needs `tesserad` built with `alsa-bridge`), with
    persistence in `state.toml`, live hardware readback, automatic reapply when
    a card reappears, and `tesseractl volume show|set|mute`.
  - The speaker bundle installer writes `/etc/tessera/config.d/60-host-volume.toml`
    alongside the portal's `[appliance.volume]`; on first start with a daemon
    that has no saved host volume yet, the portal hands its saved
    `/var/lib/tessera-web/volume.json` value over once and stops calling
    `amixer`.
- **Hardware control surfaces (`/surfaces`, `tesseractl surfaces`, `tesseractl surface`).**
  - Outbound transports: USB HID (`/dev/hidraw*`), USB MIDI (ALSA rawmidi
    `/dev/snd/midi*`, opened read-write once), and **MIDI over TCP**
    (`tcp_midi`, raw MIDI both ways on a TCP connection `tesserad` opens with
    reconnect back-off, e.g. Allen & Heath CQ on port `51325`). Tessera
    connects out only and never opens a listener. Writes go through a writer
    thread per device (whole messages, never blocking the daemon); a write
    error ends the session. A lost device is found from its device errors
    (USB) or TCP keep-alive (about 6 s); an idle desk is never dropped for
    being silent.
  - Shareable TOML/JSON [surface profiles](docs/surface-profiles.md) with
    piecewise-linear fader laws (`[curves]`), built-in Allen & Heath CQ series
    profile (`allen-heath-cq.toml`), shipped directory
    `/usr/share/tessera/surfaces/`, operator directory
    `/etc/tessera/surfaces.d/`, portal import/export, and `Query::SurfaceProfiles`.
  - Interactive **Learn a control** (`Query::LearnControl`, `tesseractl surface
    learn`) for HID and MIDI, plus `tesseractl surface set --file` (JSON
    `SurfaceSpec`), `tesseractl surface tcp-midi`, and `tesseractl surfaces --json`.
  - Mapping actions: channel gain/mute, RX output gain/mute, host volume/mute,
    router port gain/mute, crosspoint gain/mute, and **Follow** (the control
    acts exactly like a control on another surface: that control's bindings
    apply with its surface as the origin, chains apply through, and its motor
    moves, in dB through each surface's own fader law; mutual pairs are
    allowed without echo loops, longer cycles are refused, and a follow
    binding owns no level itself).
  - Button modes: toggle, momentary (hold to mute), and push-to-talk.
  - Fader priority: a connected non-motorized fader owns the level it drives
    (`Gain`, `HostVolume`, `PortGain`, `CrosspointGain`; portal shows it
    read-only and other origins get `GainOwned` or `LevelOwned`). Mute is never
    owned, and motorized faders own nothing and follow changes made elsewhere.
- **Mixer panels (`/mixer`, `/mixer/edit`, `tesseractl mixer`).**
  - Default mixing desk of channels in use plus Main L/R (`host_volume` when
    daemon-owned, else portal `[appliance.volume]`, else RX output stage), with
    live vertical faders on desktop and horizontal strips on phones.
  - Custom saved panels (`MixerSpec`, `SetMixer`, `RemoveMixer`, `Query::Mixers`)
    with any number of faders or buttons targeting channels, host volume,
    router ports, or crosspoints, groups, master strips, reorder, and derived
    hardware-control links (`StripControl::links_to`, created via `SetSurface`
    then `SetMixer`).
  - CLI: `tesseractl mixer list|set --file|remove`.
- **Control contract (`tessera-control-types`, additive, wire version 1).**
  - New commands: `SetOutputGain`, `SetOutputMute`, `SetCardBridge`,
    `RemoveCardBridge`, `SetMonitor`, `SetRouter`, `ResetRouter`,
    `SetCrosspoint`, `RemoveCrosspoint`, `SetCrosspointGain`,
    `SetCrosspointMute`, `SetPortGain`, `SetPortMute`, `SetHostVolume`,
    `SetHostMute`, `SetMixer`, `RemoveMixer`.
  - New queries/replies: `Surfaces`, `LearnControl`, `SurfaceProfiles`,
    `Router`, `Mixers`.
  - New events: `OutputGainChanged`, `OutputMuteChanged`, `RouterChanged`,
    `CrosspointGainChanged`, `CrosspointMuteChanged`, `PortGainChanged`,
    `PortMuteChanged`, `HostVolumeChanged`, `MixerChanged`.
  - New errors: `LevelOwned`, `InvalidMixer`.
  - New status fields: `Status.host_volume`, `ChannelStatus.output_gain`,
    `ChannelStatus.output_muted`, `ChannelStatus.output_gain_owner`,
    `DeviceStatus.monitor_rx_from_tx`, `DeviceStatus.monitor_delay_us`,
    `BridgeStatus.card_first_channel`, `BridgeStatus.local_only`.
  - Surface connection epochs: `SurfaceStatus.connection_epoch` and the
    optional `epoch` of `ReportSurfaceConnection` (absent from JSON when not
    set). A report is refused for a disabled surface or an old epoch, so a
    quick disable/enable or source change cannot leave a fader's ownership
    lost or bring back an old connection.
  - `SurfaceSpec` and `NamedCurve` are `#[non_exhaustive]`: build them with
    `SurfaceSpec::new` (and `with_enabled`) and `NamedCurve::new` (Rust API
    only; the JSON and TOML forms are unchanged).

### Changed

- **Monitored and card inputs reach host outputs without a network flow.**
  Because the host router mixes directly into host outputs on every engine
  tick, local monitoring (`SetMonitor`) and card inputs patched to `host_out`
  now play locally even when no network RX flow is active on that channel.
- **`SetOutputGain` obeys `HostOut` port ownership.** While an `OutputGain`
  surface binding still owns nothing, a connected non-motorized fader bound to
  `PortGain` on a `HostOut` port now owns that host output's level:
  `SetOutputGain` (and `SetPortGain`) from other origins is refused with
  `LevelOwned` and `ChannelStatus::output_gain_owner` names the surface.
- **`[discovery] browse_devices` now defaults to `true`** (was `false`), so the
  portal's **Network routing** page lists devices on the segment without extra
  configuration. Set `browse_devices = false` in `/etc/tessera/config.d/` to
  opt out.
- **`tesserad.service` runs with `SupplementaryGroups=audio`** so sound-card
  bridges, `[host_volume]`, and rawmidi control surfaces can open `/dev/snd/*`
  without adding a manual systemd drop-in.
- **Surface profile files refuse unknown keys everywhere.** Controls, `midi`
  addresses, and `[curves]` points now use strict parsing (`deny_unknown_fields`)
  just like top-level fields, `[match]`, and `[[binding]]`; typos such as
  `motorised` or `lable` are rejected with the key name and file path instead
  of being silently ignored.
- **Local monitoring delay defaults to `0` (`auto`)** (`monitor_delay_us = 0`
  and `SetMonitor` default), choosing the lowest safe delay for a feeding
  capture bridge automatically.
- **Control values stay unknown until reported.** Surface control tiles show
  *touch it to see it* (and CLI prints `-`) until the hardware reports a value
  (or Tessera moves a motorized fader there: the position it was sent to), and
  saving a surface spec re-evaluates its faders.
- **Portal navigation**: the inter-device routing matrix is labelled **Network
  routing** (`/routing`) to distinguish it from the host **Router** (`/router`).

### Fixed

- **Portal sliders now send all hidden form fields.** In-place live posts from
  sliders, momentary buttons, and `data-live-post` forms in `/live.js` now
  include the form's hidden fields (such as `stage=output`, `port`, and `xp`).
  Previously, dragging an RX **Output (after the mix)** slider on `/devices` or
  an output-stage **Main L/R** fader on `/mixer` sent only `db`, which applied
  the change to channel input gain instead of output level.
- **Network routing matrix (`/routing`) fixes:**
  - Static `[[flow]]` entries now appear in the matrix as receiving (or waiting
    / no media), read-only, naming the config flow ID (and unmatched static
    flows appear in the input header); collapsed-band counts include them.
  - Channel bridge annotations come only from **enabled** bridges (preferring a
    running bridge), so a disabled bridge no longer labels channels carried by
    another bridge.
  - All local RX inputs of this host are listed with their configured channel
    labels even if the runtime reports a shorter page.
  - Plain-language reasons explain when a receiver cannot be routed (for
    example `operation_unavailable` explains that the compatibility runtime is
    not registered while local-mix cells stay clickable).
- **Device page level tables** stack vertically until the viewport is wide
  enough for both RX and TX tables without clipping.
- **Rawmidi devices** are opened once read-write instead of opening separate
  read and write handles.
- **Fader priority holds in the corners:** `ClearConfiguration` keeps an RX
  output level owned by a `port_gain` fader on its host output (as it keeps
  owned channel gains); unpatching an owned crosspoint and patching it again
  at another gain from another origin is refused with `LevelOwned`; and
  narrowing `[host_volume]` no longer blocks editing the other strips of a
  saved mixer panel whose host strip has its own range.

### Upgrade notes

- **Packages:** upgrade `tessera`, `tessera-alsa`, `tessera-web`, `tessera-dsp`
  (and `tessera-pw` if installed) together, or run `tessera-bundle install`
  with the new archive. Restart `tesserad` when an audio interruption is
  acceptable.
- **Speaker volume handoff:** running the 0.6.0 speaker bundle installer writes
  `/etc/tessera/config.d/60-host-volume.toml` once (leaving `web.d/60-speaker.toml`
  and `/var/lib/tessera-web/volume.json` in place). When `tesserad` starts with
  `[host_volume]` and no saved volume yet, `tessera-web` sends its saved
  `volume.json` value once via `SetHostVolume` and stops calling `amixer`.
- **Existing speakers without the installer:** to hand amplifier volume to
  `tesserad` manually, add `[host_volume]` (`card`, `control`, `min_db`,
  `max_db`) to `/etc/tessera/config.d/60-host-volume.toml` and reload
  `tesserad` (requires `tesserad` built with `alsa-bridge`).
- **Custom profiles:** check any hand-written files in `/etc/tessera/surfaces.d/`
  for unknown keys; invalid files are reported and skipped at load.
- **Rollback to 0.5.x:** back up `/var/lib/tessera/state.toml` first. An older
  daemon ignores the new `state.toml` sections (`[router]`, `[host_volume]`,
  `[[mixer]]`, `[[surface]]`, `rx_output_*`) and drops them on its next write
  while keeping subscriptions, bridges, and monitor lists. Remove any
  0.6.0-only keys (`[host_volume]`, `[[route]]`, `local_only`,
  `card_first_channel`) from `/etc/tessera/config.d/*.toml` before starting a
  0.5.x daemon, because unknown configuration keys are errors. On a speaker,
  the kept `[appliance.volume]` and `volume.json` let a rolled-back portal
  resume controlling the amplifier. With the bundle: run the 0.6.0 bundle's
  `uninstall --yes` (not `--purge`) first: it removes
  `config.d/60-host-volume.toml`, keeps a copy of the daemon state as
  `/var/lib/tessera/state.toml.<version>` and prints the downgrade steps (the
  0.5.x installer refuses while a newer version is installed); then install
  with the 0.5.x bundle. To return to 0.6.0, install it, stop `tesserad`, copy
  the kept `state.toml.<version>` back and start it. Details:
  `packaging/bundle/README.md`.

### Not in 0.6.0 (roadmap)

- **Inbound MIDI port on Tessera** (exposing a rawmidi gadget, RTP-MIDI or TCP
  listener for external software to control Tessera) is not implemented in 0.6.0;
  Tessera connects out to control surfaces only.
- **RTP-MIDI client** (`rtp_midi`), per-port delay/metering, router scenes, and
  fixed-ratio sample-rate conversion for bridges whose card cannot match the
  device rate remain future work.

## 0.5.2

Tessera 0.5.2 is a packaging release. The daemon, the CLI and the portal are
those of 0.5.1; the only reason for this version is to publish signed release
packages and bundles, built and signed in public.

### Packaging

- Release workflow: GitHub Actions builds the Debian 13 packages and bundles
  (amd64 and arm64, in `debian:trixie` containers) and the experimental Ubuntu
  24.04 arm64 bundle from the `v*` tag with `cargo xtask build-deb`,
  `check-deb` and `build-bundle`, signs both checksum layers, records GitHub
  build provenance for every file and publishes the GitHub release once a
  maintainer approves. Pull requests build and check the same way. It replaces
  the bundle-only workflow, which failed because its runner had no `lintian`.
- Release key: `packaging/tessera-release.pub` (minisign, key id
  `4E330826A26C2812`) is built into `tessera-bundle`, so a release bundle
  installs without `--allow-unsigned` and without provisioning
  `/etc/tessera/trusted-release-keys`. An installer from 0.5.1 or earlier still
  needs that file (or the option). [Verifying](docs/bundle.md#verifying-and-installing).
- `cargo xtask check-deb` names `lintian` when it is not installed instead of
  failing with `xtask: No such file or directory`.

## 0.5.1

Tessera 0.5.1 answers a controller's question for the device latency range, and
lets a speaker's whole audio chain follow the tessera device's sample rate. It
is interoperable with Dante® in the areas listed in 0.5.0; it is not certified.

### Native device role

- Device latency: the device now tells a controller the range a latency change
  may use (smallest and largest value, from `latency_bounds_us` raised to the
  platform's timing floor unless `allow_unsafe_latency`), and both the
  configured and the active latency are writable, as the compatibility runtime
  answered when the vendor's controller offered its Device Latency control.
  The same rule checks every latency write. A write may name either latency
  value (both must agree), and every accepted write is logged with its form.
  Whether the vendor's controller now enables its Device Latency control has not
  been verified yet.
- A latency the configuration locks (`locked = ["latency"]`) is shown as
  read-only to controllers.

### Portal

- Rate follower (`[appliance.rate_follow]`, appliance mode, off by default):
  on a speaker whose sources share an ALSA `dmix` bus in front of CamillaDSP,
  the tessera device's sample rate (set from the vendor controller, the portal
  or `tesseractl set-rate`) is the only rate. Every 2 s the portal writes it
  into `/var/lib/tessera-rate/alsa.conf` (`tessera.rate <Hz>`, which the bus
  reads when it opens); while the bus runs at another rate it stops CamillaDSP
  and restarts the listed players so the bus reopens; and it keeps CamillaDSP's
  `samplerate` and a set `capture_samplerate` at the device's rate, in the
  active configuration (only those values change) and in the running engine. A
  restored or GUI-applied configuration at another rate is corrected the same
  way. A rate change interrupts every source for a few seconds. Failures are
  retried after 2 s, doubling to 60 s, and shown on the Speaker page under
  *Sample rate follows the network device*; the capture-rate notice offers no
  button while the follower runs.
  [Setup](docs/raspberry-pi-speaker.md#shared-sources-and-one-sample-rate),
  [keys](docs/web-portal.md#rate-follower).
- `tessera-web` creates `/var/lib/tessera-rate` (0755, `tessera-web`) with the
  file at 48000, and its unit may write there.
- CamillaDSP 4.x refusals (`"result": {"ConfigValidationError": "..."}` or an
  error name) are reported as the engine's refusal with its message, not as a
  protocol error (after which the boost and the capture-rate fix restarted the
  engine).

### Limits

- The 0.5.0 limit "the vendor's controller could not change a native device's
  sample rate in one setup": that device had a bridge with `sample_rate` set,
  which pins the device rate (`tesseractl status` and the portal name the
  bridge). Without such a bridge the controller offers the rate. Leave
  `sample_rate` out of a bridge whose card can follow the device, as the rate
  follower requires.
- The 0.5.0 limits otherwise still apply.

## 0.5.0

Tessera 0.5.0 lets `tesserad` serve the device role itself. A host that enables
the **native device role** (`--native-device`, opt-in) answers network
controllers, requests the flows of its saved subscriptions and serves flows to
other receivers, in place of the packaged Controller compatibility runtime.
Hosts that do not enable it keep the runtime, as before. The portal can now
route receivers, and a device can be locked behind a PIN. Tessera is
interoperable with Dante® in the areas listed here; it is not certified, and the
limits below are part of this release.

### Native device role (opt-in)

- `tesserad --native-device` (`TESSERA_NATIVE_DEVICE=true` in a service
  drop-in) serves the first logical device's control, flow-control and
  device-information ports (UDP 4440, 4455, 8700 and 8800). Stop and disable
  `tessera-controller@main` on that host first: both bind those ports
  exclusively. `--native-vendor` sets the vendor field of the device-information
  messages; use the `--vendor` value of `/usr/lib/tessera/controller.args`.
  How to switch and how to go back:
  [interoperability](docs/interoperability.md#native-device-role).
- A controller's change (subscriptions, device and channel names, sample rate,
  encoding, device latency) enters the daemon as the same typed command the CLI
  and the portal send. Saved subscriptions remain the only route store, and
  every lock applies.
- Receiving: one flow per receive input, so a change on one input never
  interrupts another. A changed input is requested on a fresh port and the old
  flow is stopped only after the new one is installed (make before break). A
  refused or unanswered request is retried after 5 s; a flow without media is
  requested again. Receive ports are UDP 14336 to 14589.
- Transmitting: the daemon answers flow requests, updates and stops. One
  transmit channel can feed several inputs, each as an independent session. A
  receiver that restarts retires its older sessions; a unicast session whose
  receiver was heard and then stayed silent for 10 s is released, and one that
  was never heard from is released 30 s after its last request. A receiver may
  ask for an empty channel position (channel 0); it is sent as silence. Static
  transmit flows and `set_protocol_flows` accept channel 0 the same way (at
  least one real channel per flow).
- Manual multicast flows: a controller can create a transmit multicast flow (1
  to 8 channels, a group in 239.255.0.0/16, UDP 4321) and delete it. The flow is
  advertised, listed and restored at start. A deleted flow keeps sending,
  unadvertised, for 8 s so that its receivers move to unicast before it stops.
- Multicast receive: a native receiver takes a channel from an advertised
  multicast flow of the same rate and encoding, moving from unicast make before
  break, and falls back to unicast first when the flow is withdrawn or delivers
  nothing for 6 s (a silent group is not tried again for that input for 30 s).
  The firewall must pass UDP 4321 to the group; otherwise the input stays on
  unicast.
- Device latency can be read and set from a controller; the configured latency
  is reported.
- Latency monitoring: a heartbeat every second carries each receive flow's
  largest lateness and late-packet count, and a second message the per-channel
  signal peaks.
- Identify: a controller's identify request shows a notice on the portal's
  status page and in `tesseractl status` for `--native-identify-secs` seconds
  (default 10).
- Reboot from a controller restarts the daemon, not the host.
- Factory reset and clear configuration from a controller are applied only with
  `[control] allow_factory_reset = true` (see Settings).
- Interface statistics are not answered, and network-configuration writes from a
  controller are refused.

### Routing

- The portal's **Routing** page shows a matrix (transmit channels down the left,
  receive inputs across the top) and a list. A crosspoint opens a review; the
  confirmed change (Connect, Replace or Disconnect) goes through the daemon,
  which checks the selection and the locks again before it sends one command.
- Routing works from the receiver's own portal and from another Tessera host's
  portal; the receiving device applies its own locks, and a locked receiver is
  shown read-only with the reason. A source on another host must be qualified
  in `[control] routing_sources` on the host that sends the change; the host's
  own transmitter needs no qualification. Discovery browsing
  (`[discovery] browse_devices = true`) must be on.
- With the native device role, one transmit channel can feed several inputs of
  the same receiver, each with its own flow. A receiver still on the
  compatibility runtime takes one source channel on only one of its inputs; the
  matrix disables the other cells.
- `tesseractl routing` lists devices, channels and receiver selections, executes
  a reviewed change, and inspects and reconciles an operation whose outcome is
  unknown.
- Devices that advertise a manual multicast flow remain available as sources.

### Clock (PTPv1)

These apply to every host, with or without the native device role.

- A leader-capable port advertises stratum 0x2A to 0x2D (by priority) instead
  of 4, so that hardware leaders keep precedence in the election. A follower
  advertises stratum 255 and never claims Preferred Leader.
- Sync every 250 ms (was 1 s). Delay_Req carries the parent's Sync interval.
  General messages use DSCP 46 (EF, was 8); event messages keep DSCP 56 (CS7).
- Between two Preferred Leaders only the lower MAC address decides. Turning
  Preferred Leader on or off changes only the preferred flag.
- Clock role `follower` and Preferred Leader (priority below 64) exclude each
  other: the daemon refuses the combination from any source. At start, the
  follower role wins and a stored or configured preferred priority is dropped
  with a warning.
- While this device leads and hears another Preferred Leader, it logs a warning
  once per clock and `tesseractl status` shows a `clock.<interface>` warning
  while that clock is heard. Role `leader` never yields.
- Stale or repeated Syncs are ignored, and a lost parent starts a new election
  at once.

### Settings

- Sample rate and encoding: the portal, `tesseractl` and a controller are
  offered exactly the values the device accepts now (`DeviceStatus.formats`).
  When only the current value is left, the reason is shown: an enabled
  `[[bridge]]` that sets `sample_rate`, a `locked` key, or no other supported
  value. With the native device role every rate or encoding write from a
  controller is logged with its outcome
  ([troubleshooting](docs/troubleshooting.md#a-controller-cannot-change-the-sample-rate-or-encoding)).
- Device access lock: `tesseractl access lock|unlock|reset` and the Lock and
  Unlock forms of the portal's Device page freeze a device's configuration and
  routing behind a four-digit PIN, kept as an Argon2id hash in
  `access-lock.toml`. Changes from the CLI, the portal and network controllers
  are refused while it is set; reading and running audio continue. A forgotten
  PIN is reset by a local administrator (`sudo tesseractl access reset`); the
  portal cannot reset it.
- `[control] allow_factory_reset` (default `false`) and the
  `clear_configuration` command return a device's run-time settings (name,
  labels, subscriptions, rate, encoding, latency, clock role and priority,
  controller and manual multicast flows and, unless kept, the primary interface)
  to the configuration files.
- `--native-identify-secs` sets how long an identify request stays visible.

### API

- `tessera-control-types` 0.5.0 is additive: `AccessPin`, the commands
  `LockAccess`, `UnlockAccess`, `ResetAccessLock`, `ClearConfiguration` and
  `RouteReceiver` (with receiver operation inspection and reconciliation), the
  errors `AccessLocked` and `IncorrectPin`, `DeviceStatus.access_locked`,
  `DeviceStatus.formats`, `Event::AccessLockChanged` and the routing receiver
  page. Rust clients that match exhaustively must handle the new variants. The
  wire envelope stays at version 1.
- The portal adds `GET /api/v1/routing/receiver`,
  `GET /api/v1/routing/operation` and `GET /api/v1/routing/carrier-recovery`,
  and refuses `reset_access_lock` (403).

### Limits

- A controller of the vendor's protocol sees the device access lock but cannot
  set or clear it; Tessera does not advertise itself as lockable. The
  compatibility runtime's own control server is not covered by the lock.
- Whether the vendor's controller shows its latency graph for a native device
  has not been verified.
- Why the vendor's controller could not change a native device's sample rate in
  one setup is still being investigated. Tessera now logs every rate and
  encoding write it receives, so the log shows whether a write arrived.
- The newer flow-list queries of current controllers are not answered; some
  tools refuse some operations because of that (one refuses a sample-rate
  change in its own checks).
- Changing Preferred Leader from a controller has not been verified; use
  `tesseractl clock set-priority` or the portal's Clock page.
- The native device role and the compatibility runtime serve only the first
  logical device.
- A remote receiver is routed only when all its inputs fit on one page.
- Not certified: external hardware audio content, sustained clock behaviour and
  controller interoperability.
- The deprecated `90-probe-flows.toml` fallback is still read; its removal
  remains deferred.

## 0.4.1 — development checkpoint

This checkpoint retains the existing audio transport. Browser Connect/Replace/
Disconnect, verified source audio and continuity are unfinished. The device picker
edits saved subscription intent; it does not establish or remove audio flows.
Private report admission and process-association diagnostics remain default-off.

- Bounded backend admission, actor and connector diagnostics, with regression
  coverage and a separate deterministic CI job. No authenticated registration,
  production admission or native routing dispatcher is enabled.
- Keep the deprecated `90-probe-flows.toml` compatibility fallback in 0.4.1;
  its previously announced removal is deferred.

- The Device page offers device/output selectors and review for existing saved
  subscription intent, with advanced manual entry for offline sources. This
  does not establish or remove audio flows; existing transport is preserved.
- Read-only routing inventory through the daemon/API, with paged device and
  channel records. Local receiver status separates saved intent, effective
  carrier and packet activity; packet activity does not verify the selected source.
- Network browsing is opt-in through `discovery.browse_devices` (default
  `false`). Enabling it may browse permitted interfaces even when advertising
  is disabled. Existing advertising and routing behavior remains unchanged.
- Optional `subscriptions` host lock refuses legacy subscription changes and
  runtime RX reductions that discard intent. Defaults remain unlocked. The new
  `ControlLock::Subscriptions` variant requires Rust clients with exhaustive
  matches to update; existing wire fields and envelope version 1 remain intact.
- Successful HTTP commands retain `ok` and include the typed daemon `reply`.
- Browser enhancement source migrates to strict TypeScript. Its generated asset
  is embedded by Rust; appliance builds and runtime require no Node.

## 0.3.0

Tessera 0.3.0 brings together the CamillaDSP speaker and Speaker control panel
of 0.2.2 with a rewrite of the installation, packaging, network and release
machinery that Tessera now shares with NDIMon-R. A 0.2.2 CamillaDSP speaker
keeps every file it has. What changes for an operator is how the add-on bundle
is installed (one program instead of two scripts, and it authenticates the
release first), the network backends and the Network page, and a number of
fixes found by installing, upgrading and purging the release candidate on
clean machines.

### Highlights

- **CamillaDSP speaker** (from 0.2.2). The `speaker` profile of the add-on
  bundle installs the new `tessera-dsp` package (CamillaDSP 4.1.3 and the
  CamillaGUI backend 4.1.0, the unmodified upstream releases) and sets up the
  whole chain: network, `tesserad`, the ALSA PCM `tessera`, CamillaDSP, the
  sound card. The portal's Speaker page is a control panel with live Volume and
  Signal boost sliders, an "EQ engine (CamillaDSP)" button, service switches
  that act in place and one signal-path overview. A tessera-only speaker
  converts in place; `uninstall --purge` removes what the bundle created (and the
  account) and never the operator's own files in `/opt/camilladsp`. See
  [docs/dsp.md](docs/dsp.md).
- **One installer program, authenticated releases.** The bundle archive carries
  `tessera-bundle install|uninstall|verify` in place of `install.sh` and
  `uninstall.sh`, with the same options. Before anything is installed it checks
  the release's minisign signature (prehashed Ed25519), its `SHA256SUMS` and
  every payload file. Packages and bundles are built by `cargo xtask`; the
  source tree contains no shell or Python scripts, and the Debian maintainer
  hooks are native programs. See [docs/bundle.md](docs/bundle.md).
- **Network configuration on four backends.** The standalone Network page and
  `tessera-network` helper now drive systemd-networkd, Netplan with networkd,
  NetworkManager (including Netplan's NetworkManager renderer) and ifupdown, with
  the same draft, apply, Keep and Revert flow, journalled recovery and boot
  recovery. Management can carry up to six DNS search domains. An administrator
  can allow Keep from any reachable address (`[network] confirm = "token_only"`
  in `web.toml`); the default still requires the new management address.
  Networking can be handed to and received from NDIMon-R with
  `tessera-network handoff`. See [docs/network-vlans.md](docs/network-vlans.md).
- **Controller flows are a typed command.** Flows requested through the
  Controller compatibility runtime are stored in `state.toml` and applied over
  the control socket. `tesseractl flows show|set|clear` manages them, a host
  such as NDIMon-R can own the flows with `[control] locked = ["flows"]`, and
  they survive a restart of `tesserad`. Changes requested by a controller that
  restart a device (rate, encoding, latency, subscriptions that rewrite flows)
  are applied instead of being refused for lack of a terminal.
- **Capture-rate check on the Speaker page.** When the EQ engine's ALSA capture
  rate differs from the tessera device's rate, the page says so, shows both rates
  and the engine's error, and offers a one-click fix and reload when the
  configuration allows it. A sound card that refuses the playback format gets its
  own notice.
- **Local portal settings** can live in `/etc/tessera/web.d/*.toml`, read after
  `web.toml`; the network helper reads the same merged view.

### Upgrade notes and breaking changes

From 0.2.2 (CamillaDSP speaker):

- Upgrade `tessera`, `tessera-alsa`, `tessera-web` and `tessera-dsp` together
  (and `tessera-pw` if it is installed); the packages depend on each other's
  exact version. With a bundle, extract the new archive and run the same
  `tessera-bundle install --profile speaker --audio-card ...` command. The
  device configuration, the portal's `web.d/60-speaker.toml`, the CamillaDSP
  configurations and the state file are not rewritten.
- **`install.sh` and `uninstall.sh` are gone.** Run `tessera-bundle install`
  and `tessera-bundle uninstall` from the extracted archive instead. The options
  are unchanged, `verify` is new, and the record of an earlier installation
  (`/var/lib/tessera-bundle/record.json`) is read as it is. Scripts that call the
  old names must be changed.
- **The bundle is authenticated before it is installed, and this release
  ships no trusted key.** `tessera-bundle` accepts a release only when a key
  listed in the root-owned `/etc/tessera/trusted-release-keys` vouches for it
  (mode 0644, bare minisign public keys, one per line). Without a key it stops
  with `untrusted_key: no trusted release key is installed` and changes nothing.
  Until you have provisioned a key obtained from a channel you trust, install
  with `--allow-unsigned`, after checking the archive against the `SHA256SUMS`
  you downloaded. `--allow-unsigned` is meant for development builds; `verify`
  has no such bypass. Integrity, path, manifest and platform checks still apply.
- Restart `tesserad` (and with it `camilladsp`), `camillagui` and the Controller
  runtime at a moment when an interruption is acceptable: an upgrade never
  restarts a running audio daemon or engine. The portal restarts itself.
- The EQ engine is now started again whenever `tesserad` starts (below). The
  upgrade creates the link that does this; an engine you switched off stays off.
- The portal now saves the amplifier volume itself. The unit drop-in an earlier
  installer wrote (`/etc/systemd/system/tessera-web.service.d/60-speaker.conf`) is
  never rewritten; to let the Volume pane follow the sound card across a change
  of card index, replace its `DeviceAllow=/dev/snd/controlC<n>` line with the
  device lines of `/usr/share/doc/tessera-web/examples/appliance-paths.conf`, and
  drop `ReadWritePaths=-/var/lib/alsa`, then restart `tessera-web`.

From 0.2.1:

- Read the 0.2.2 notes in `packaging/NEWS.Debian` as well. Installing the speaker
  profile again turns a tessera-only speaker into the CamillaDSP speaker in
  place: the profile's `[[bridge]]` is removed from
  `/etc/tessera/config.d/60-speaker.toml`, CamillaDSP takes the sound card and
  starts with a flat configuration. Add `--no-dsp` to keep the speaker as it is.
  A `[[bridge]]` that is not the profile's own makes the install refuse before
  anything changes.
- CamillaGUI (port 5005) has no login. It now listens on loopback by default,
  and the bundle installer sets the speaker's management address, so it is
  reachable from the network the speaker is managed on and from no other
  ([docs/dsp.md](docs/dsp.md#operating-it)). A speaker that has an engine of
  its own gets no GUI.
- `tessera-dsp` is new, for amd64 and arm64 only. The bundle creates the
  `camilla` account; a purge of the bundle removes it again.

From either:

- `90-probe-flows.toml` is still read in 0.3.0, with a deprecation warning, as
  long as no controller flow set has been stored. Its removal, first announced for
  0.3.0, moves to **0.4.0**. Re-submit the flows through the Controller runtime
  or `tesseractl flows set` before then.
- `tessera-control-types` is 0.3.0. The control wire version stays 1 and every
  change is additive. Network documents gain the optional row field
  `dns_search`: a helper or NDIMon-R older than this release rejects a document
  that carries search domains, so upgrade both ends before configuring any.
  Documents without search domains are unchanged.
- Debian 13 (trixie), amd64 and arm64, is the supported platform of the packages
  and bundles. The Ubuntu 24.04 arm64 bundle remains **experimental** and needs
  `--allow-ubuntu-24.04`. Other releases may build from source; a bundle never
  crosses OS or architecture.
- A sound card that appears after the portal started now restarts the portal (a
  udev rule starts `tessera-web-sound.service`), but only on a speaker whose
  portal has a Volume pane. Sessions end with that restart; sign in again.
- Removing the packages now also removes what they left loaded (firewall table,
  slices, raised socket-buffer ceilings). The network files the portal committed
  stay, by design.

### Security

- Releases are authenticated with prehashed Ed25519 minisign signatures, a strict
  `SHA256SUMS` parser, binding of product, version and file name, and a complete
  file check. The compiled trust list is empty and fails closed; the root-owned
  key file must be a regular file with mode 0644 under root-owned parents without
  symlinks. Keys can be rotated without trusting an unsigned transition.
- The privileged network helper keeps its journal, lock and records at
  root:root 0600/0700, refuses linked or special files and migrates only the one
  known legacy empty lock. Network confirmation errors are typed, the Keep, Revert
  and Pending routes (and their JSON aliases) share one budget of five failures
  per minute, and the confirmation policy captured when a change is staged cannot
  be weakened while it is pending.
- `tessera-network.service` now allows `AF_PACKET`, which its duplicate-address
  check (`arping`) needs. On a speaker with a Volume pane the portal may open
  the sound-card control devices `controlC0` to `controlC31`, and no PCM,
  sequencer or timer node.
- The Debian packages have no handwritten maintainer scripts, and installing,
  removing or purging them no longer depends on a shell.

### Notable fixes

- A network **Revert** no longer ends in an HTTP 500 and a failed transaction.
- A CamillaDSP speaker stays audible across network changes: the EQ engine now
  starts whenever `tesserad` starts.
- The amplifier volume survives a reboot, follows the card when its index
  changes and recovers when the card appears after the portal started; a muted
  playback switch is announced with an **Unmute** button.
- The capture-rate notice works on the shipped `Speaker.yml` and on files saved by
  CamillaGUI; the seeded playback format is one the card accepts.
- Controller flows stored with `tesseractl flows set` survive a restart of
  `tesserad` with the Controller runtime enabled.
- A stock cloud-image Netplan file with DNS search domains is adopted instead of
  refused.
- The ALSA plugin no longer offers a buffer the daemon cannot feed after a late
  first drain.
- When the clock port restarts (the interface was unplugged and plugged back in),
  media time no longer steps back by the new port's thread start-up. On a busy
  machine that step was hundreds of microseconds, and the followers of a restarted
  leader left the locked state for tens of seconds while their servo worked it off.
- Package removal and purge clean up what the 0.2.x packages left behind, and the
  bundle no longer changes the APT mark of a dependency that is already installed.

### Known limitations

The NetworkManager and ifupdown backends are new in this release and have been
exercised in isolated test guests; the first run on a real Raspberry Pi found the
Netplan-profile defect fixed above, the second the recovery defects fixed below
(after a Keep and on an explicit Revert), and live acceptance of those fixes is
still pending, so treat both backends as candidates. The Controller compatibility runtime serves the first
logical device only. External hardware audio content, sustained clock behaviour and
hardware timestamping through VLAN interfaces are not certified.

### Carried over from 0.2.2

- New package `tessera-dsp` (amd64, arm64): CamillaDSP 4.1.3 and the CamillaGUI
  backend 4.1.0, the unmodified upstream releases pinned by SHA-256
  (`packaging/dsp/dsp.lock`), in a root-owned tree with their services, the
  `camilla` account and the `/etc/camilladsp` GUI configuration. `cargo xtask
  build-deb` fetches and verifies the pinned archives in Rust; `SKIP_DSP=1`
  leaves the package out. Native hooks create the state file and enable the
  units without starting them; a purge removes `/opt/camilladsp` and
  `/etc/camilladsp`. Documented in `docs/dsp.md`.
- The `speaker` profile of the bundle sets up the whole chain by default (no
  in-daemon bridge, a flat CamillaDSP configuration seeded for the chosen card,
  the portal's Speaker page, service switches, volume and signal boost).
  `tessera-bundle install --no-dsp` keeps the tessera-only speaker with the
  bridge. A tessera-only speaker (0.2.1, or 0.2.2 with `--no-dsp`) converts in
  place; a 0.2.2 CamillaDSP speaker upgrades without any of its files being
  rewritten. `uninstall --purge` removes the CamillaDSP data and the `camilla`
  account the bundle created. `manifest.json` gains the additive boolean `dsp`.
- The Speaker page is a control panel: a master card with live Volume and
  Signal boost sliders, an "EQ engine (CamillaDSP)" button (`gui_url` accepts
  `{host}`), service switches and restarts that act in place, and the overview's
  signal path, clock and subscriptions merged into one page. The capture-rate
  notice follows the master card.
- `tessera-web` reads `/etc/tessera/web.d/*.toml` after `web.toml` (later files
  win; tables merge, lists are replaced). `tessera-network` reads the same
  merged view for the portal's listen port and its `[network]` policy.
- Changes requested through the Controller compatibility runtime that restart a
  device (rate, encoding, latency, subscriptions that rewrite flows) are
  confirmed on the operator's behalf instead of being refused for lack of a
  terminal.

### Detail: alignment with NDIMon-R (network, packaging, releases) and timing

- Never let the ALSA plugin's estimate of the daemon's engine tick fall below a
  steady tick. It divided the time between drains by the ticks they found, which
  is off by up to one tick over the 12 samples that were trusted: a late first
  drain followed by a prompt one gave 44 frames for a 48-frame tick and the
  plugin offered a buffer the daemon cannot feed. The estimate is now an upper
  bound for any drain timing and a daemon up to a tick late (about 23 % high when
  first trusted, 4 % once smoothed, so the smallest period and buffer it offers
  grow by that much), with deterministic estimator tests. The ALSA fake daemon
  skips a tick it missed by more than a tick, as `tesserad` does, instead of
  sending a burst.

- Make ALSA callback regressions advance explicit media time and acknowledged
  fake-engine ticks, retaining every allocation, continuity, lag and pause bound
  under CPU load. Acknowledge audio-ipc stress progress and web cancellation
  interleavings; remove scheduler-selected sample-rate/timebase windows.
- Include recorded transmitter packet skips and their frame-equivalent upper
  bound in pre-fault silence failures without excusing missing audio.

- Drive PipeWire engine regressions with explicit media time, synchronous
  fake-daemon ticks and acknowledged thread interleavings, preserving their
  continuity and signal bounds when builds compete for CPU time.

- Purge generated daemon, Controller and network-helper state/runtime trees
  from native package hooks and bundle removal, retaining documented accounts
  and the journal of a separately retained portal.

- Safely migrate the exact empty legacy root:tessera-web network lock at boot
  recovery to root:root 0600; continue refusing busy or other unsafe files.

- Restore daemon state/runtime writes under its filesystem sandbox without
  competing directory ownership. Give tmpfiles the Controller working tree
  and wait for the daemon's first persisted identity without restarting.

- Give tmpfiles sole ownership of package directories, keep the network journal
  root:root 0700, and check private-store and shared-tree declarations across
  the complete Debian package set. Service restarts no longer chown these trees.
- Provision target ALSA development libraries and architecture-specific linker
  and pkg-config lookup in both ARM cross-build CI jobs.

- Add semantic network API 2 with decimal-string u64 revisions, explicit legacy
  adapters, stale-revision errors and administrator-policy checks under lock.
- Start proven ISC ifupdown DHCP clients without waiting for optional leases;
  restore management and static networks even when media DHCP is offline.
- Enforce exact private journal ownership/modes and reject linked or special
  records. Require selected-link evidence for pre-rename selectors and refuse
  unknown addressing intent during adoption.
- Add schema-1 network ownership release, read-only import and recoverable
  receive commits on all four backends. Root CLI and helper-socket commands
  review an impact token, preserve UUIDs and restore the old owner on failure,
  Revert or expiry; kept transfers archive the releasing engine's private state.
- Correct the nft cgroup capability probe, VLAN-parent JSON verification,
  unchanged-file preflight snapshots and Debian ifupdown state/hook handling
  found by real container execution. Add an isolated Rust nspawn scenario driver.

- Add UUID-scoped NetworkManager and explicitly handed-off ifupdown network
  transactions, alongside a common backend lifecycle with rooted command
  fixtures. Full live acceptance remains pending.
- Pin networkd and Netplan/networkd parent/VLAN configuration ahead of
  distribution defaults, report foreign selectors as drift, preserve route
  metrics and fail activation when backend selection cannot be verified.
- Normalize network transaction phases, capture the staged confirmation policy,
  verify recovery addressing, and schedule independent 10-second recovery retries.
- Add body-token JSON aliases for network Pending/Keep/Revert. All token routes
  share the five-failure rolling budget; Keep remains enabled and the helper
  decides whether policy, readiness and expiry permit confirmation.
- Add an administrator-controlled network confirmation policy in web.toml:
  `require_management_address` remains the default; `token_only` accepts a
  ready, unexpired change's token from any reachable address.
- Return typed network confirmation refusals over the helper socket, retaining
  legacy human messages and compatibility with older helper replies.
- Make pending-page instructions follow the confirmation policy, show typed
  Keep/Revert failure reasons, and limit only token mismatches or unclassified
  Keep failures per client. Early Keep presses never consume the limit.
- Align portal-generated API errors with the shared error/message envelope,
  including `unauthorized` logins and `permission_denied` CSRF/origin refusals.

- Preserve DNS search domains. A network row may carry
  `dns_search`, at most six host names, for DHCP and static management alike;
  any other network is refused with the existing management-only message. All
  four backends render the list on the management definition only (Netplan
  `nameservers.search`, NetworkManager `ipv4.dns-search`, networkd `Domains=`,
  ifupdown `dns-search`) and adopt it from the same keys, so a stock
  cloud-image Netplan file is adopted instead of refused as
  `unrepresentable_configuration`. NetworkManager adoption reads the configured
  property and never lease domains; the property is snapshotted and restored
  with the connection. More than six entries, an invalid name (a trailing dot,
  a `~` routing domain), or search domains on another definition are still
  refused, and the detail names the file or connection and the key. An empty
  list is omitted from documents, so existing network documents and ownership
  handoff files are byte-identical. The Network page gains a "Search domains"
  field for each network.

- Authenticate releases with prehashed Ed25519 minisign signatures, strict
  SHA256SUMS parsing, product/version/file binding and complete file checks.
  Trust starts empty and can be provisioned through the protected root-owned
  key override. Bundle installation now requires authentication unless the
  development-only `--allow-unsigned` option is explicit.
- Add `cargo xtask release`, `verify-release` and owner-only `release-keygen`;
  keep signing secrets out of diagnostics, generate encrypted 0600 keys only
  on explicit owner use, and document manual verification, custody and rotation.

- Fix compiler and Clippy diagnostics and stale audit/default expectations in
  the packaging prerequisites. Validate packages with lintian, resolve unit
  dependencies across the package set, correct package metadata and symlinks,
  and document the native helper commands with manual pages.

- Replace every packaging, bundle lifecycle and fuzz shell entry point with
  Rust: `cargo xtask` tasks and bundled `tessera-bundle install|uninstall|verify`.
  Preserve record format, options, recovery, package provenance, dependency
  checks and explicit restart/removal controls. Add rootless lifecycle goldens
  and archive fixtures, with signed release verification (see
  `packaging/README.md`, "Signed releases").
- Build Debian packages with declarative sysusers/tmpfiles, units and presets;
  native Rust dpkg hooks handle legacy migration, password/policy initialization
  and purge without shell. Package names, dependencies, conffiles and NEWS
  behavior are retained. No debhelper/cargo-deb hooks are generated.
- Enforce `cargo xtask check-no-scripts` in CI with an empty allowlist. Port
  install/controller/network/NEWS/bundle checks and both fuzz launchers to Rust;
  the merge gate retains all six embedded `no_std` checks. Controller fixtures
  now inspect socket-applied flows and legacy fallback upgrades.

- Controller flow updates now use one typed command on `control.sock`, with
  protocol origin, shared validation, restart impact and debounced persistence
  in `state.toml`. Host flows remain separate; duplicate peer/port/direction
  endpoints are refused across the effective flow set.
- Hosts can own flows with `[control] locked = ["flows"]`, alongside the existing
  clock role/priority and primary-interface locks. Lock refusals and runtime
  intent changes are visible as events with field paths and origins.
- Add `tesseractl flows show|set|clear` and readable host audit events.
  `tessera-control-types` is 0.3.0; all wire changes are additive (version 1).
- Deprecate `90-probe-flows.toml`: it remains a fallback for this transition
  release, with a warning naming the file. Any stored controller set, including
  an explicit empty set, supersedes it. **File support is removed in 0.4.0.**
  The compatibility adapter no longer writes the file; its subsequent reload
  action is a deprecated successful no-op.

- Show CamillaDSP capture-rate mismatches on the Speaker page, including
  the device and processing rates and recognised capture errors. Offer a
  one-click capture-rate fix and EQ reload when the configuration permits it,
  preserving YAML comments and order.
- Align regression checks with runtime audit events, capture stop-reason
  reads and shipping discovery defaults; preserve strict lock, confirmation
  and read-only guarantees.

### Detail: fixes from the first verification of the release candidate

Found by installing, upgrading and purging the release candidate on clean Debian 13
and Ubuntu machines.

- The Speaker page's capture-rate notice and one-click fix now work on the
  shipped `Speaker.yml` and on files CamillaGUI saves: both put the `pipeline:`
  sequence at the key's own column, which the reader refused, so the notice never
  appeared. Only `devices` is read; a configuration that cannot be checked is
  logged once at info level with the reason.
- An explicit network Revert no longer ends in HTTP 500 and a `failed`
  transaction: `tessera-network.service` allows `AF_PACKET`, which the
  duplicate-address check (`arping`) needs. A Revert pressed while the apply
  worker still holds the helper's lock waits up to five seconds, then answers
  the retryable `still_applying` (409).
- The amplifier volume survives a reboot. The portal saves it itself in
  `/var/lib/tessera-web/volume.json` (the unit's new `StateDirectory`, 0700) on
  every committed change and sets it again at start, as soon as the card is
  there; `alsactl store` and the `/var/lib/alsa` path are gone. The signal boost
  is a line of the active EQ configuration and persists with it.
- The Volume pane follows the card across index changes. The unit drop-in the
  installer writes (and `appliance-paths.conf`) opens the control devices
  `controlC0` to `controlC31` instead of the one the card had at install time; no
  PCM, sequencer or timer node is opened. A missing or unopenable card is named
  in the pane. A drop-in written by an earlier installer is never rewritten:
  replace its `DeviceAllow=/dev/snd/controlC<n>` line with those of
  `appliance-paths.conf` (and drop its `ReadWritePaths=-/var/lib/alsa`) to get this.
- The seeded `Speaker.yml` plays in the first of `S32_LE`, `S24_3_LE`, `S24_4_LE`
  and `S16_LE` that the sound card accepts (the installer asks the kernel's ALSA
  PCM interface when it can open the card; the template has a new marker
  `@PLAYBACK_FORMAT@`). When the card refuses a format the Speaker page now says
  so: the engine's message, the device and the format, and where to change it.
- The unsigned-bundle refusal keeps its code and says why: `untrusted_key: no
  trusted release key is installed`, the key file
  `/etc/tessera/trusted-release-keys`, and that `--allow-unsigned` is for
  development builds only.
- `dist/tmpfiles.d/tessera-web.conf` no longer declares `/etc/polkit-1/rules.d`,
  which polkitd owns (the "Duplicate line" message at every boot is gone).
- The bundle no longer changes the APT mark of a dependency that is already
  installed (`iproute2 set to manually installed`): only a missing dependency, or
  one older than the bundle's minimum, is named to apt.
- Controller flows stored with `tesseractl flows set` or `set_protocol_flows`
  survive a restart of `tesserad` with the Controller compatibility runtime
  enabled. The runtime restarts with the daemon and reports an empty set at
  start; the adapter now applies an empty report only from a run that has already
  applied one. The controller owns what it submitted in its current run
  (docs/control-interface.md, "Who owns the controller flow set").
- Removing or purging the packages unloads the helper's firewall table
  `inet tessera_network`, stops `tesseraaudio.slice` and the controller template's
  slice, and puts `net.core.rmem_max` / `wmem_max` back to the values recorded
  when the package first raised them. The network files the portal committed
  stay by design.
- Release builds: the `0.3.0-1` NEWS entry is dated `Mon, 26 Oct 2026 00:00:00 +0000`,
  and `check-deb` / `build-bundle` stop with a plain message when a NEWS entry is
  dated later than `SOURCE_DATE_EPOCH` instead of a bare lintian tag.
- Each call to the EQ engine now ends with the websocket closing handshake.

### Detail: fixes from the second verification of the release candidate

Found by repeating the run on virtual hardware with a real sound-card codec.

- The EQ engine comes back whenever the daemon starts again. `camilladsp.service`
  was only `PartOf=tesserad.service`: a stop reached it, but a plain start of the
  daemon after a stop (what `tessera-network` does on every apply, revert and
  expiry) did not start it, so a CamillaDSP speaker went silent after any network
  change. The unit is now also `WantedBy=tesserad.service`: enabling the engine
  links it into the daemon's `.wants`, the package hook and the bundle installer
  enable it on every install and upgrade (so a speaker upgraded from 0.2.2 gets
  the link), and an engine switched off (`systemctl disable`, the Speaker page's
  switch) stays off. `check-deb` fails a unit without the three relationships.
- The Volume pane and the boot restore now recover from a sound card that appears
  after the portal started (a USB DAC plugged in later, a card that enumerates after
  the portal or takes another index at run time). systemd resolves the unit's
  `DeviceAllow=` paths, and binds `/dev/snd`, when the portal starts, so such a
  card stayed out of reach until the portal restarted. A udev rule
  (`60-tessera-web-sound.rules`) starts the new oneshot `tessera-web-sound.service`
  when a sound card's control device is added; after a 5 second settle it runs
  `systemctl try-restart tessera-web.service`, and only where a drop-in of the
  portal's unit lists a `/dev/snd/` device. The portal's sessions end with the
  restart. `DeviceAllow=char-alsa` stays rejected (it would let the portal play and
  record). The unit and `docs/web-portal.md` give the reasoning.
- The socket-buffer ceilings are restored on removal. The configure hook called
  `systemd-sysctl` by its bare name, which is on no `bin` directory of dpkg's `PATH`,
  so it did nothing, no record was written and `rmem_max`/`wmem_max` stayed raised after a
  purge. It now runs `/usr/lib/systemd/systemd-sysctl` (else `/lib/systemd/...`) and says
  so when that fails.
- Removing or purging `tessera-web` also stops the slice of the service-switch helper
  jobs, `system-tessera\x2dweb\x2dunitfile.slice`.
- `/etc/polkit-1/rules.d` is put back to polkitd's 0750 root:polkitd when an earlier 0.2.x
  package left it at 0755 (configure and the bundle installer, only that exact mode and only
  where the group `polkitd` exists).
- The Volume pane shows a muted hardware switch: a card that comes up with its playback
  switch off (`[off]` in `amixer sget`) is announced under the slider with an **Unmute**
  button (`POST /speaker/volume/unmute`, `amixer sset <control> unmute`), and a value set
  on a muted card is answered with "… but the output is muted". The live answer gains the
  field `muted`. The portal never unmutes by itself.

### Detail: fixes from the first run on real hardware (Raspberry Pi 4 speaker)

Found by installing the release candidate over a working speaker with a real
amplifier HAT, a hand-made CamillaDSP and a dual-homed NetworkManager network.

- **The arm64 bundle installs on Debian 13.** It named `libasound2`, a virtual
  package there, and the installer's dpkg query could not see it. A cross build
  now declares the package a native build declares (`libasound2t64`), `check-deb`
  compares both kinds of package, and the installer treats a package that
  provides a dependency as meeting it, as dpkg and apt do. Before it writes
  anything the installer asks apt whether the missing dependencies can be
  installed; a refusal leaves no record and no package behind.
- **Your own engine unit is no longer shadowed in silence.** A
  `/etc/systemd/system/camilladsp.service` wins over the packaged unit (old
  program, no start with the daemon). The installer moves it, and the drop-ins
  that sat next to it, to `*.pre-tessera-dsp`, reloads systemd and says so.
  Your engine program, configurations and state file are not touched.
- **The portal keeps your services.** The speaker profile no longer writes an
  `[[appliance.service]]` list over one you have in `web.toml` or `web.d` (a list
  in a drop-in replaces the whole list, so your services and their polkit grants
  vanished). Add `camilladsp` to your list for the EQ engine switch.
- **The EQ GUI is not opened to every network.** The first candidates listened
  on `0.0.0.0` with no login, and an upgrade turned the GUI on for a speaker
  that had none. `camillagui.yml` now ships `127.0.0.1`; the installer sets the
  management address (the `[network] interface` port, else the only port with a
  default route) and the portal's link uses it; an upgrade keeps the GUI and the
  engine on or off as they were, and narrows a GUI that listened on every
  interface. The network helper's firewall also rejects TCP port 5005 on every
  link but management and loopback.
- **A switched-off EQ engine stays off.** The portal's switch ran
  `systemctl disable --now --no-reload`; systemd kept the engine's link to the
  daemon in memory, so the next start of `tesserad` (every network change)
  brought the engine back. Enable and disable now reload systemd.
- **`uninstall --purge` deletes only what the bundle made.** It removed all of
  `/opt/camilladsp`, which also holds an engine program, configurations and
  filter files of your own. It now removes the seed configuration the installer
  recorded, the state file when it names no configuration that exists, the GUI
  settings and empty directories, and lists what it leaves. `apt purge
  tessera-dsp` does the same.
- **Refusals state the remedy.** `ambiguous_management_port` (Ethernet and Wi-Fi
  both with a default route) and the related refusals name
  `[network] interface = "<port>"` in `web.toml` or `web.d` and list the wired
  candidates; the portal's "VLAN configuration unavailable" page says so too.
- **Network changes apply on a Netplan-generated NetworkManager profile** (the
  Raspberry Pi OS default). Every apply rolled back with
  `foreign_configuration_selected` because the profile does not keep
  `connection.interface-name`; the helper now proves the parent by its active
  connection, and a real mismatch names the connection and the property.
  A failed or reverted change restores the parent's Netplan YAML and keyfile
  byte for byte (it left keys `nmcli` had added), and a profile that holds a
  credential is refused (`unsafe_configuration`) instead of being copied into
  the journal. The second hardware run (below) applied, kept, reverted and
  expired changes on real NetworkManager.
- **IPv6 privacy-extension settings no longer refuse adoption** (Netplan `ipv6-privacy`,
  networkd `IPv6PrivacyExtensions=`, NetworkManager `ipv6.ip6-privacy`, ifupdown
  `privext`). Stock Armbian Netplan and Raspberry Pi OS profiles carry them. The
  managed profiles have IPv6 off, so they have no effect there; Revert and a failed
  apply put the exact setting back.
- The Volume pane shows the amplifier's lowest step as `−∞ dB` with the slider at
  its floor instead of ALSA's mute value (`−100000.0 dB`).
- `restart pending` in the bundle record is cleared (`restart done`) once the
  daemon has been restarted or the board rebooted.
- The unit drop-in `appliance-paths.conf` that earlier documentation told you to
  copy is removed when everything in it is in the profile's drop-in.
- The `tessera-dsp` README and the other documents that ship with the packages
  link to each other where the packages put them (`web-portal.md` now ships in
  `tessera-web`, `raspberry-pi-speaker.md` in `tessera-dsp`), and the generated
  configuration reference no longer carries an internal open-question code.

### Detail: fixes from the second run on real hardware (Raspberry Pi 4 speaker)

The second candidate installed, upgraded and purged correctly on the same speaker,
and applied, kept and reverted network changes on its Netplan-generated
NetworkManager profile. What it found:

- **A recovery after a Keep no longer fails for good, and never leaves the speaker
  silent.** NetworkManager's Netplan writer puts `ipv6.ip6-privacy: "-1"` back into
  the restored `/etc/netplan/90-NM-<uuid>.yaml` within a second of every
  `nmcli connection reload` (and every activation refreshes `connection.timestamp`),
  so a restore of a profile that a kept change had saved never matched its saved
  bytes: the expiry failed every ten seconds, and `tesserad`, `camilladsp` and the
  controller stayed stopped, for eight minutes until the journal was removed by hand.
  The comparison now leaves those lines out (anything else still counts), a recovery
  that does not verify starts the daemon again at once (the engine and the controller
  follow), and a retry no longer stops it a second time.
- **`tessera-network accept-current [--yes]`** is the administrator's way out of a
  recovery that cannot finish: it prints the reason and the files that differ, and
  with `--yes` keeps the network as it is, puts the firewall back, closes the
  pending recovery and starts the audio services
  ([network-vlans.md](docs/network-vlans.md), "When a recovery cannot finish").
- **The first explicit Revert no longer fails.** Both Reverts of the run answered
  HTTP 500 (`connection_restore_mismatch`) and the retry ten seconds later restored
  the same files: a property read straight after a reload is not final. The
  recovery waits up to ten seconds for NetworkManager to settle, and an error names
  the connection, each property and its two values. A recovery that did not verify
  is answered as `apply_failed` with step `recover` and the detail, which the portal
  maps to 409 (it was an untyped 500).
- **The EQ GUI is restarted when its address changes.** A narrowed GUI kept
  listening on every interface (without a login) until somebody restarted it, and
  the installer's remedy, "name the port and install again", did nothing at the
  installed version. The installer restarts `camillagui` when a run leaves its
  `bind_address` other than it was before the packages ran, says so, and starts
  nothing that was off; a run of the installed version applies the GUI's address
  and the portal's link without `--reinstall`.
- **The portal's link to the GUI is written.** A `web.d/60-speaker.toml` written
  before the address was known gets its `gui_url` (one line, in its
  `[appliance.camilladsp]` table); a link or a drop-in of yours is left alone and
  the installer prints the line to set.
- **A moved engine unit leaves no dangling link, and the plan names it.** The
  enable links of an engine unit that is moved aside (`multi-user.target.wants/…`)
  are removed, and the plan, a dry run included, lists the unit files and
  drop-in directories it will move.
- **A bare DHCPv6 switch and NetworkManager's default metric no longer refuse
  adoption.** Stock Armbian Netplan has `dhcp6: yes` and every stock NetworkManager
  profile has `ipv4.route-metric -1`, so the Network page was read-only on the
  primary boards. Netplan `dhcp6: true` and networkd `DHCP=yes|ipv6`, without
  DHCPv6 settings, IPv6 addresses, gateway, DNS or `accept-ra: true`, adopt as a
  distribution default: the managed output turns DHCPv6 off and Revert restores the
  source file. `-1` is NetworkManager's Ethernet default 100, accepted once the
  default route it installed agrees; any other metric it cannot establish refuses,
  naming the connection and `ipv4.route-metric`. NDIMon-R adopts the same.
- **`appliance-paths.conf` also goes when its only extra line is
  `ReadWritePaths=-/var/lib/alsa`**, which the portal has not needed since it keeps
  the volume in its own state directory. Removing the file is the safe choice;
  merging the line into the profile's drop-in would keep a write permission nothing
  uses.
- **Package dependencies are checked against what the packages ship.** The
  `tessera-dsp` GUI is a Python runtime that brings OpenSSL, zlib, libffi, bzip2,
  lzma, readline, tinfo, uuid and libstdc++ in its own `_internal` directory, so
  those need no `Depends`; its glibc floor (2.35) was above the 2.34 that
  `dpkg-shlibdeps` found for CamillaDSP alone, and the arm64 packages stated no
  floor at all. `check-deb` now reads the `NEEDED` libraries of every ELF object in
  a package, accepts one that a shared object beside the program provides, and
  requires every other to be in its table of library packages and in `Depends`,
  and `libc6 (>= v)` to cover the highest glibc symbol the package uses; the
  builds state the floor.

## 0.2.1

- Enable Controller interoperability and device discovery by default.
- Include the portal in bundles and list core Services without Speaker mode.
- Initialize fresh portal installations with password `tessera`, preserving
  existing passwords and configured service lists.
- Migrate legacy helper enablement and preserve separate portal ownership
  during bundle removal.

- Drive PTP and media scheduling from the raw hardware oscillator so NTP
  frequency adjustments cannot change the audio clock. Safely convert Linux
  software timestamps and operating-system timer deadlines at their boundaries.
- Reacquire frequency after a sustained clock error instead of repeatedly
  stepping at the stale rate.
- Bump the local clock overlay to layout 2.0. Upgrade and restart the daemon,
  ALSA clients and PipeWire clients together; older local clients reject the
  new layout. Network packets and control APIs are unchanged.

## 0.2.0-beta

### Standalone network configuration

- Added a modular privileged `tessera-network` helper and portal Network editor
  for named management/audio networks on one wired port.
- Supports untagged or VLAN 1–4094 networks with DHCP/static IPv4, management-only
  default gateway/DNS and explicit audio interface locking.
- Native networkd and Netplan/networkd are supported. Unsupported complex
  configurations are refused instead of silently losing settings.
- Applies journal the previous state and arm independent device-side recovery.
  Once ready, explicitly Keep through the new management address within 60
  seconds. Revert, failure, expiry or boot recovery restores the previous state.
- Management/audio traffic uses owned firewall isolation. NDIMon ownership
  disables standalone mutations so there is one OS network manager.

### Daemon, control and portal

- Added configuration-first controls, drop-ins/locks, runtime channel counts,
  impact-token confirmation and a generated configuration reference.
- Improved subscription receive-state reporting and live portal updates.
- Added shared appearance controls and optional Speaker/service/EQ integration.
- Retained the PTPv1 virtual media clock, static UDP flows, ALSA/PipeWire clients
  and optional sound-card bridge with drift compensation.

### Packaging and NDIMon integration

- Added profile-based bundles for speakers and NDIMon runtime provisioning,
  with configuration preservation and deliberate restart/removal behavior.
- Added separate experimental Ubuntu 24.04 arm64 bundle support with explicit
  OS opt-in. It cannot install Debian binaries onto Ubuntu or vice versa.
- Updated source/workspace versions to 0.2.0-beta. Debian packages use
  0.2.0~beta-* so they sort before the eventual final 0.2.0 packages.
- Plugin sources and installable host archives are published separately in
  [NDIMon-Plugins](https://github.com/markfen88/NDIMon-Plugins). The Rust runtime
  remains here, and each plugin pins an exact runtime bundle/checksum.
- Added initial integration feature/API metadata. The host does not yet offer
  automatic warnings for every unverified exact-build combination.

### Limitations

This beta does not implement vendor-controller subscriptions/device control,
AES67, JACK, Bluetooth calibration, HDMI ARC/eARC or automatic lip-sync. Static
wire/flow parameters remain provisional for external hardware. Audio content
interoperability and sustained clock/hardware PTP behavior are not certified.
Source publication does not deploy to an existing appliance.
