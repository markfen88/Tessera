# Configuration

The prose reference for how `tesserad` is configured. The schema is implemented by `tessera-state` (`crates/tessera-state/src/config.rs`, the key table in `schema.rs`, checks in `validate.rs`); wire-facing values are provisional for external hardware.

**Configuration comes first.** Every tunable of the daemon (control-socket limits and timeouts, audio-client limits, the media engine's batch sizes, receiver keepalives, the bridge reopen back-off, real-time scheduling, the PTP and mDNS switches, the devices and their channel counts) is a key of one schema with a default, a range and an apply class (`live`, `reload` or `restart`: when a change takes effect). What stays compiled in is what the configuration cannot sensibly change: wire constants (ports, field sizes, timers fixed by a protocol), the structure of the real-time threads, and safety floors (minimum values that protect the system, not fixed values). The command-line options of `tesserad` are overrides of single keys, not a second configuration.

The keys are one table in `schema.rs`. The [schema reference](#schema-reference) at the end of this page and the commented file the packages install as `/etc/tessera/config.toml` are generated from it.

| File | Owner | Purpose |
|---|---|---|
| `/etc/tessera/config.toml` | the package (a conffile) | the **reference**: every key commented out at its default, each with what it does and why, its unit, range, apply class and restart scope. Nothing in it is active; keep it as shipped. `tesserad --print-default-config` prints the reference of the installed version; a copy is installed as `/usr/share/doc/tessera/examples/config.toml`, which is absent on images that strip `/usr/share/doc` |
| `/etc/tessera/config.d/*.toml` | you, other packages | **your configuration**: drop-ins applied in lexical order; tables merge, scalars and arrays replace, `[[device]]` entries merge by `id` (see "Drop-ins"). Copy the lines you want from the reference into a file here and change them |
| `/var/lib/tessera/state.toml` | `tesserad` | current values changed at run time with `tesseractl`, the web portal or a network controller (once the control protocols exist); do not edit while running |

An empty configuration is valid: the defaults give one device named after the host, 2 receive and 2 transmit channels, 48 kHz. Unknown keys are errors, so a daemon older than a key refuses a file that uses it. How the package treats the conffile on an upgrade: [the user guide](user-guide.md#8-upgrade-and-rollback).

## Precedence

From strongest to weakest:

1. `tesserad` command-line options (each overrides one configuration key; the daemon logs every key it overrides, a reload never changes
   such a key, and a live key set this way is locked against run-time changes). For the log filter only, the `RUST_LOG` environment
   variable is stronger still (`RUST_LOG` > `--log-level` > `[daemon] log_level`); it is read at start and a reload does not change it →
2. a value pinned by `locked` in the configuration →
3. `state.toml` (the current value set at run time, used only if within the config bounds) →
4. `config.d/*.toml` (lexical order, later wins) →
5. `config.toml` →
6. the built-in default.

Runtime commands (protocol / CLI / web) are validated against bounds and locks, then persisted to `state.toml` (step 3).

**`locked`.** A key listed in a device's `locked` (`name`, `sample_rate`, `encoding`, `latency`, `rx_channels`, `tx_channels`, `rx_channel_labels`,
`tx_channel_labels`), in `[clock] locked` (`role`, `priority`) or in `[network] locked` (`primary`) is pinned by the configuration (unless the command line overrides it, step 1): `state.toml` (step 3)
is skipped for it, the value from the files (`config.d` over `config.toml` over the default) is the one in effect, and a command that would change it
(`tesseractl`, the web portal, a network controller) is refused with a `Locked` error naming the key. Removing a key from `locked` makes it
changeable again; a state value that was stored before is then ignored until something changes the key. A `locked` entry protects a key from run-time commands only,
not from a later drop-in (see "Locks and drop-ins").

## `tesserad` command line

Every tunable is a configuration key; the daemon's options only override keys. An option that is not given leaves the key to
the files. Each overridden key is logged at start (`the command line (--max-frame-bytes) overrides control.max_frame_bytes; a reload
does not change it`), and the values are checked like file values (the error names the key and `<command line>`). Under systemd the
daemon runs without options: put settings in `/etc/tessera/config.d/` rather than in an `ExecStart=` drop-in.

`tesserad --print-default-config` prints the reference configuration (every key commented out at its default, with what it does and
why, its unit, range, apply class and restart scope) and exits 0; it reads no file and needs no privileges. It is what the packages
install as `/etc/tessera/config.toml`.

| Option | Key |
|---|---|
| `--socket-group` | `[control] socket_group` |
| `--socket-mode` | `[control] socket_mode` (octal) |
| `--audio-socket` | `[audio] socket` |
| `--max-frame-bytes` | `[control] max_frame_bytes` |
| `--max-connections` | `[control] max_connections` |
| `--max-connections-per-uid` | `[control] max_connections_per_uid` |
| `--admin-reserved-connections` | `[control] admin_reserved_connections` |
| `--idle-timeout-secs` | `[control] idle_timeout_secs` |
| `--event-queue` | `[control] event_queue` |
| `--control-threads` | `[control] threads` |
| `--rate-burst` | `[control] rate_burst` |
| `--rate-per-second` | `[control] rate_per_second` |
| `--frame-timeout-secs` | `[control] frame_timeout_secs` |
| `--write-timeout-secs` | `[control] write_timeout_secs` |
| `--stream-write-timeout-secs` | `[control] stream_write_timeout_secs` |
| `--persist-delay-ms` | `[daemon] persist_delay_ms` |
| `--shutdown-timeout-secs` | `[daemon] shutdown_timeout_secs` |
| `--log-level` | `[daemon] log_level` (`RUST_LOG` overrides both; the daemon says so at start) |
| `--no-rt` | `[daemon] realtime = false` |
| `--no-ptp` | `[clock] enabled = false` |
| `--no-discovery` | `[discovery] enabled = false` |
| `--max-audio-clients` | `[audio] max_clients` |
| `--max-audio-clients-per-uid` | `[audio] max_clients_per_uid` |
| `--audio-max-period-frames` | `[audio] max_period_frames` |
| `--audio-ring-budget-mib` | `[audio] ring_budget_mib` |
| `--audio-max-ring-mib` | `[audio] max_ring_mib` |
| `--audio-reorder-window-us` | `[audio] reorder_window_us` |
| `--audio-default-period-frames` | `[audio] default_period_frames` |
| `--tick-us` | `[engine] tick_us` |
| `--rx-batch` | `[engine] rx_batch` |
| `--rx-max-batches` | `[engine] rx_max_batches` |
| `--tx-batch` | `[engine] tx_batch` |
| `--tx-max-per-tick` | `[engine] tx_max_per_tick` |
| `--socket-buffer-kib` | `[engine] socket_buffer_kib` |
| `--rx-margin-us` | `[engine] rx_margin_us` |
| `--rx-keepalive-ms` | `[defaults.flow] rx_keepalive_ms` |
| `--rx-keepalive-timeout-ms` | `[defaults.flow] rx_keepalive_timeout_ms` |
| `--rx-keepalive-payload` | `[defaults.flow] rx_keepalive_payload` |

Command-line only: `--config-dir` (where the files are, so it cannot be one of their keys), `--state-dir` and `--runtime-dir` (explicit
locations for this process, for a separate instance; the configured ones are `[daemon] state_dir` and `runtime_dir`; with
`--state-dir` the configured `state_dir` is not used by this process, so a reload that changes it is accepted, and it is listed as a
restart key although a restart with the same `--state-dir` would not use it either), `--print-default-config`, `--help`, `--version`.

`--socket-group` and `[control] socket_group`: a group given either way that cannot be applied is an error at start (before this release only
the option existed; the file key is new and just as strict). Unset, the sockets get the group `tessera` if it exists, else the daemon's
own group with a warning.

Four keys are read before the daemon takes its instance lock, because the lock and the sockets come first: `[daemon] runtime_dir`
(unless `--runtime-dir`), `[control] socket_mode`, `socket_group` and `[audio] socket`. They are restart-class keys that `state.toml`
and `locked` cannot carry; if the files change between that early read and the state engine's load, start fails rather than run with
two values.

## Apply classes in practice

| Class | Keys (examples) | When a change takes effect |
|---|---|---|
| `live` | device name, rate, encoding, latency, labels, channel counts; `[clock] role`, `priority` | at once, by a command or a reload (a stored run-time value wins over the file unless the key is `locked`) |
| `reload` | `[control]` limits and timeouts | for control connections opened after the reload; open connections keep theirs |
| `reload` | `[audio]` limits | for audio clients that attach after the reload, and for bridges when they next open their card; attached clients keep their rings |
| `reload` | `[daemon] persist_delay_ms`, `shutdown_timeout_secs`, `log_level`, `admin_group` | the next state write window, the next stop, at once, at once (also for open connections) |
| `reload` | `[control] rate_burst`, `rate_per_second` | the next request (control socket and state engine) |
| `reload` | `[defaults.flow]` keepalive keys (and per `[[flow]]`) | when the flow next (re)starts: a keepalive change does not rebuild a running flow |
| `reload` | `[defaults.bridge]` error waits, `error_backoff*` and `error_sleep*` (and per `[[bridge]]`) | at the bridge's next error: the manager's next reopen wait, the bridge thread's next pause; neither reopens a running card |
| `reload` | `[[flow]]`, `[[bridge]]`, `[network]`, `[clock] timestamping`, `free_running_tx`, `[discovery] mdns` | at once, by the runtime reconciler (only the changed entries restart) |
| `restart` | `[engine]`, `[control] threads`, `socket_mode`, `socket_group`, `[audio] socket`, `[daemon] realtime`, `state_dir`, `runtime_dir`, `[clock] enabled`, `[discovery] enabled`, `[backends] enabled`, `[aes67] enabled` | at the next service start. A reload that changes one logs it by name once (`changed in the configuration, takes effect at the next service restart: engine.rx_batch`), a confirmed reload returns it in `restart_keys`, and `tesseractl status` shows it as a warning until the daemon runs with it |

A reload never changes a key the command line overrides.

## Drop-ins (`config.d/`)

Files are applied after `config.toml`, in lexical file-name order; a later file wins. Per key:

- **Tables** (`[daemon]`, `[network]`, `[clock]`, the label tables inside a device, `[device.multicast]`, ...) merge key by key.
- **Scalars and arrays** replace. This includes `[network] interfaces`, `[engine] cpu_affinity` and `latency_bounds_us`: a drop-in that
  sets one of them restates the whole value.
- **`locked` lists combine** instead of replacing: `[clock] locked`, `[network] locked` and each device's `locked` are extended by the entries of later files (a
  set union; duplicates are ignored). A drop-in can add locks but never silently remove one that an earlier file set; to remove a lock, edit
  the file that set it.
- **`[[device]]` entries merge by `id`.**
  - An entry whose `id` already exists (from `config.toml` or an earlier drop-in) overrides only the keys it sets. Label tables merge per
    channel (`rx_channel_labels = { 3 = "Three" }` adds or changes channel 3 and leaves the others), and `[device.multicast]` merges per key.
  - Its `locked` list is extended, as described above.
  - An entry with a new `id` appends a device.
  - The device order is the order of first definition: base-file order, then new ids in drop-in order.
  - A drop-in **cannot remove a device**. Remove it in the base file (or in the file that defines it).
  - Two entries with the same `id` inside one file are not merged with each other; that is the "duplicate device id" error.
  - An entry without an `id` cannot be matched with anything. It counts as a new device and is rejected (`id` is required). When an earlier
    file has already defined devices, the load also prints a warning naming the file and the entry number within it; in the first file that
    defines devices, only the "`id` is required" error is reported.
- **`[[bridge]]` entries have no key to merge by** (`device`, `alsa_pcm`, `direction` and `channels` do not identify an entry), so a drop-in that
  contains `[[bridge]]` **replaces the whole bridge list**, and the load prints a warning naming what was replaced.
- **`[[flow]]` entries are replaced as a whole**, like `[[bridge]]`: a drop-in that contains `[[flow]]` replaces the flow list of the files before it
  and the load prints a warning. (Static flows are provisional, see "Static flows" below; entries replace as a list.)

- **Label keys** (`rx_channel_labels`, `tx_channel_labels`) must be canonical decimal channel numbers (`1`, not `01` or `+1`); other
  spellings are a configuration error, so two files can never name one channel in two ways.

Provenance (which file set which key) follows the rules above: a key set by a drop-in on an existing device is reported under that device's
position in the merged list (`device[0].sample_rate`), `device[i].id` names the file that defined the device, and each element of a merged
`locked` list (`device[0].locked[1]`, `clock.locked[0]`) names the file that added it.

### Locks and drop-ins

- **A lock does not protect a value from a drop-in.** `locked` only blocks changes at runtime (a network controller, `tesseractl`). A later
  drop-in can still set the locked key to another value, and that value then becomes the new fixed value.
- A lock that a drop-in (or a reload) adds clears the value the operator had stored in `state.toml` for that key: the config value wins
  from then on, and removing the lock later does not bring the old stored value back.

### Naming devices when drop-ins add devices

A device without an explicit `name` is named after the host. **Once there are several devices, the default becomes `<host>-<id>`** so the
names stay distinct. Adding a second device in a drop-in therefore renames the first one, which is visible on the network (the
device disappears under its old name and peers lose their subscriptions to it). On a reload this is reported as a warning naming the
device, the old and the new name.

**Set an explicit `name` on every device** when you add devices with drop-ins. An explicit name is never changed by the device count,
and with `locked = ["name"]` it cannot be changed from the network either.

## Example: everything at its default

The shipped `config.toml` has no active line, and an empty configuration is valid: it gives one device `main` named after the host, 2×2 channels, 48 kHz, 24 bit, 4 ms latency on x86 and
10 ms on ARM, on the first wired interface with a carrier and an IPv4 address. No flows, no bridges, mDNS advertising off.

## Example: annotated

A configuration file with most sections, written as a drop-in (many of the values are the defaults, shown for orientation; the reference file has all of them).

```toml
# /etc/tessera/config.d/50-site.toml
[daemon]
log_level = "info"                 # error|warn|info|debug|trace, or "tessera_clock=debug,info"
state_dir = "/var/lib/tessera"
runtime_dir = "/run/tessera"
admin_group = "tessera-admin"      # may run mutating tesseractl commands

[network]
interfaces = ["enp3s0"]            # by name; default: first wired interface with carrier
# secondary = "enp4s0"             # redundancy (Phase 6)
allow_wireless = false
mtu_payload = 1400
locked = []                        # ["primary"]: the configured or automatic primary only, no runtime selection

[clock]
role = "follower"                  # follower | leader | auto (leader-capable, follower-preferred)
priority = 128                     # leader election priority (lower wins, protocol mapping per spec)
timestamping = "auto"              # auto | hardware | software
free_running_tx = false            # send to the network while not locked
locked = []                        # e.g. ["role", "priority"]: pinned by config, refused at runtime

[engine]
workers = 1                        # only 1 is implemented (see "Engine, clock and discovery at run time")
tick_us = "auto"                   # "auto" = 1000; the receive drain and client wake-up period
rt_priority = 80                   # SCHED_FIFO of the media worker, 1..=99 (normal scheduling: `[daemon] realtime = false`)
ptp_rt_priority = 70               # SCHED_FIFO of the PTP threads, 1..=99
cpu_affinity = []                  # e.g. [2, 3]; the first CPU is used for the RT threads
mlock = true                       # mlockall after start-up; a refusal is reported in the metrics, never fatal

[discovery]
mdns = "auto"                      # auto | avahi | builtin | off
advertise_devices = true           # advertise devices with mDNS; the packaged Controller runtime serves the advertised ports

[backends]
enabled = ["pipewire", "alsa"]     # clients meant to attach; today only "empty = no audio.sock" (with `[audio] socket = "auto"`) is enforced
resampler = "auto"                 # off | linear | cubic | sinc-fast | sinc-hq | auto

[[device]]
id = "main"                        # local key; the wire device ID is generated once and persisted
name = "studio-pc"                 # default: hostname (validated against protocol name rules)
rx_channels = 64
tx_channels = 64
sample_rate = 48000                # 44100 48000 88200 96000 176400 192000
encoding = 24                      # 16 | 24 | 32 bit on the wire
latency_us = 4000                  # default 4000 on x86, 10000 on ARM
latency_bounds_us = [1000, 40000]
allow_unsafe_latency = false       # per device: accept latencies below the platform timing floor
locked = []                        # name | sample_rate | encoding | latency | rx_channels | tx_channels | rx_channel_labels | tx_channel_labels
rx_channel_labels = { 1 = "Vox", 2 = "Gtr" }   # defaults; Controller renames go to state
tx_channel_labels = {}

[device.multicast]
allow_tx_multicast = true

[aes67]                            # Phase 5
enabled = false

[[bridge]]                         # optional in-daemon ALSA hardware bridge; needs a build with `alsa-bridge`
name = "speaker"                   # default: <device>-<direction>
device = "main"
alsa_pcm = "hw:CARD=<card>,DEV=0"
direction = "playback"             # playback (network → card) | capture (card → network); `rx`/`tx` also accepted
first_channel = 1                  # device channels first_channel .. first_channel + channels - 1
channels = 2                       # default: the rest of the device's channels in that direction
time_constant_s = 120              # drift loop time constant, seconds; keep it >= 10x a downstream servo's period
max_correction_ppm = 200           # clamp on the loop's correction

[[flow]]                           # PROVISIONAL static flow
id = "studio-to-stage"
direction = "tx"                   # rx | tx
device = "main"
channels = [1, 2]                  # device channels in wire order; default: all in that direction
peer = "192.0.2.2"                 # tx: destination (required); rx: the one sender accepted (or any_source = true)
port = 5004                        # tx: destination port; rx: local UDP port to listen on
```

## Validation (all errors reported together, with key paths)

- Channel counts 0..=65535 (16-bit on the wire; no other fixed limit). A device with 0 receive and 0 transmit channels is an error (the message says the device has no channels); one direction at 0 is fine.
- Tunables: every key has a range (in the schema reference). The ranges are floors that protect the system (for example `[control] max_frame_bytes` at least 64 KiB, `error_backoff_min_ms` at least 10) and ceilings that come from the kernel (`rx_batch` and `tx_batch`, real-time priorities), the wire format, an integer width or memory, not arbitrary caps. `[audio] max_ring_mib` may not exceed `ring_budget_mib`. A ring budget (plus the packet buffers of the flows) above `RLIMIT_MEMLOCK` while real-time scheduling and `[engine] mlock` are on is a warning that names the computed size.
- `sample_rate` in the supported list; `encoding` in {16, 24, 32}.
- `latency_us` within `latency_bounds_us`; the minimum bound at least 1 and not above the maximum. A latency below the platform timing
  floor (1000 us on x86, 2000 us on ARM) is an error, or only a warning when the device sets `allow_unsafe_latency = true` (a
  per-device key, not global).
- Device names and labels: at most 31 UTF-8 bytes, with control characters refused.
- Interface names must exist (warning at load, error at start); wireless refused unless allowed.
- Unique `device.id`s.
- `[[bridge]]`: see "Hardware-card bridges" below (unique names, an existing device, a channel range inside the device that no other
  enabled bridge of that device and direction uses, a card used once per direction, the device's rate, usable sizes and loop tuning). A
  `tesserad` built without the `alsa-bridge` feature refuses any `[[bridge]]` entry, also a disabled one.
- `[[flow]]`: unique ids (letters, digits, `_`, `-`), an existing device, channels within the device's channel count in that direction and not
  repeated, a non-zero port, a `peer` for tx flows, `frames_per_packet` of at least 1, no two rx flows on one port. Whether a packet of
  `frames_per_packet` frames fits `[network] mtu_payload` depends on the packet format and the channel count; the daemon checks it when it
  builds the flow (a flow that cannot be built is reported in the log and retried, the others run).

## Defaults for flows and bridges (`[defaults.flow]`, `[defaults.bridge]`)

A `[[bridge]]` or `[[flow]]` entry is one element of an array of tables. TOML does not allow a table and an array of tables with the same
name, so there is no `[bridge]` table for "the settings of every bridge". This is the trap:

```toml
[bridge]                      # WRONG: not allowed next to [[bridge]], and not a table of the schema
error_backoff = false

[[bridge]]
device = "main"
alsa_pcm = "hw:CARD=<card>,DEV=0"
```

The parser refuses it (`syntax error ... duplicate key`; a lone `[bridge]` gives `bridge: expected an array, found table`). The settings
that apply to every entry live in two separate tables, and each entry can override any of their keys by naming it:

| Table | Keys | Used by | Takes effect |
|---|---|---|---|
| `[defaults.flow]` | `rx_keepalive_ms`, `rx_keepalive_timeout_ms`, `rx_keepalive_payload` | every receive `[[flow]]` (a warning if a `tx` flow sets one) | `reload`, when a flow next (re)starts: a running flow is not rebuilt |
| `[defaults.bridge]` | `error_backoff`, `error_backoff_min_ms`, `error_backoff_max_ms` | every `[[bridge]]` | `reload`, at the bridge's next card error: a running card is not reopened |

The value in effect for an entry is its own key, else the key in `[defaults.*]`, else the built-in default. The defaults tables are ordinary
tables, so they merge key by key across `config.toml` and the drop-ins; the `[[bridge]]` and `[[flow]]` lists themselves are replaced as a
whole by a file that has any (see "Drop-ins").

```toml
# /etc/tessera/config.d/60-bridges.toml
[defaults.bridge]
error_backoff_max_ms = 10000          # every bridge: retry a failed card at least every 10 s

[[bridge]]
device = "main"
alsa_pcm = "hw:CARD=<card>,DEV=0"
error_backoff_min_ms = 200            # this bridge only
```

Each bridge is checked with the pair it ends up with. With `[defaults.bridge] error_backoff_min_ms = 100`, an entry that sets
`error_backoff_max_ms = 50` is refused: `bridge[0].error_backoff_max_ms: 50 ms is below error_backoff_min_ms (100 ms)`. The reasons for the
back-off and sleep keys are under "Reopen back-off and the error pause".

## Channel counts

`rx_channels` and `tx_channels` of a `[[device]]` (default 2 and 2) are `live` keys of restart scope `device`: changing them restarts that
device (its flows and bridges are rebuilt, its audio clients are detached and re-attach with the new ring geometry, and it is announced
again with the new counts), a short silence on its channels. They can be set in three ways:

1. **In the configuration**, in a drop-in, followed by `tesseractl config reload` or `systemctl reload tesserad`. `tesseractl config reload`
   asks for confirmation first when the edited files would restart a device (`--yes` skips the question); `systemctl reload tesserad` is a
   plain `SIGHUP` and never asks: the administrator edited the files, and the daemon applies them and logs what restarted. A count in
   `locked` can only be changed in the files.
2. **At run time with the command `set_device_channels`** (`SetDeviceChannels { device, rx, tx, dry_run }` in `tessera-control-types`): the
   counts are stored in `state.toml` and win over the files unless the key is `locked` (a locked count answers `Locked`). A device with 0
   channels in one direction (a playback-only device runs with `tx_channels = 0`) keeps that direction at 0 and can change the other.
3. **With `tesseractl set-channels --rx N --tx M`** ([docs/cli.md](cli.md#set-channels)) **or the channel counts form on a device's page of the web portal** ([docs/web-portal.md](web-portal.md)). Both show the impact and ask before they apply; see below.

Because the change restarts the device, the command is **not applied until it is confirmed**: sent unconfirmed it is refused with
`confirmation_required` and the impact (what restarts and why, what is dropped and for how long, the subscriptions that are deleted, the
audio clients that are detached, the ring memory before and after), and nothing changes. A dry run (`dry_run: true`) returns the same impact without
changing anything. The client shows the impact and sends the command again, confirmed with the impact's token:

* `tesseractl set-channels` asks the daemon for a dry run, prints the impact on standard error and asks `Apply this change? [y/N]`; only `y` or
  `yes` goes on. Without a terminal it exits 1 and changes nothing; `--yes` applies without asking (for scripts, it skips the dry run), and
  `--dry-run` only prints the impact and exits 0. The same question is asked for every change that restarts something, `tesseractl config reload`
  included when the edited files restart a device.
* The web form shows a confirmation page with the impact, **Confirm and apply** and **Cancel**; if the situation changed meanwhile nothing is
  applied (409) and the new impact is shown.
* The JSON API of the portal answers 409 `confirmation_required` with the impact and has no confirmed form; a reply the daemon refuses to send
  above its frame limit is a 502.

The details and the refusals (a flow or bridge that uses a channel that would go away, the memory budget) are under "Runtime channel
counts" in the schema reference.

Limits: each count is 0..=65535 (16 bits on the wire); both 0 is refused, by the command and by the loader (a device needs a channel; rx-only and tx-only
devices are legitimate); the memory a count costs must fit `[audio] ring_budget_mib`. There is no other fixed limit: more channels need
more ring memory (`ring_budget_mib`, and `max_ring_mib` for a single ring) and, on the network, a packet format that carries them.

## The control socket frame size (`[control] max_frame_bytes`)

Every request and every response on `control.sock` is one length-prefixed frame, and `[control] max_frame_bytes` (default 1 MiB, `reload`;
at least 64 KiB) caps its size in both directions. The floor exists so that the daemon can always send its own normal replies and error
frames. A change applies to connections opened after the reload.

* The daemon announces the value in `Status.limits.max_frame_bytes` (the field is absent from the status of an older daemon). Clients learn it
  from there: `tesseractl` and `tessera-web` never read the daemon's configuration files.
* A response that would be larger than the cap is not sent. The daemon answers the same request with the typed error `ResponseTooLarge`
  (`response_too_large` in JSON: `size`, `limit` and `key`, which is `control.max_frame_bytes`) and the connection stays open. The fix is to
  raise the key (a status with very many devices or channels is the usual cause).
* `tesseractl` has its own, separate cap on what it accepts, `--max-response-bytes` (default 8 MiB), which protects the client from a
  misbehaving daemon. It is larger than the daemon's default on purpose; raising `max_frame_bytes` above 8 MiB therefore needs the client
  option as well.

## Persisted state (machine-owned)

```toml
# managed by tesserad - do not edit while the daemon is running
schema_version = 1                 # a missing key is read as 1; older versions are migrated on load

[clock]                            # only the values changed at runtime
role = "auto"
priority = 10

[device.main]
wire_device_id = "0200000000000001"   # generated once (16 hex digits)
name = "studio-pc"
sample_rate = 48000
encoding = 24
latency_us = 4000

[device.main.rx_channel_labels]
1 = "Vox"

[[device.main.subscription]]
rx_channel = 1
tx_channel = "Out 1"
tx_device = "stagebox-a"
```

Subscriptions are kept by name even when they do not currently resolve.

## Static flows (PROVISIONAL)

**Provisional static flow configuration; automatic vendor flow control is unavailable.** A hand-written `[[flow]]` opens a receive
port or sends to a fixed address: it works between tessera instances, or with a partner that can be set up to send unicast to a fixed
address and port. Hardware transmitters that need a negotiated flow request cannot start solely
from this file: Tessera does not send those requests. Use a matching Tessera
peer or independently configured hardware peer. Packet settings remain
provisional for external hardware ([compatibility limits](interoperability.md)).
Flows belong to configuration files; `state.toml` never stores them.

| Key | Default | Meaning |
|---|---|---|
| `id` | required | Name of the flow (letters, digits, `_`, `-`); appears in metrics and logs, never on the wire. |
| `direction` | required | `rx` (network to device) or `tx` (device to network). |
| `device` | required | `id` of the device the flow belongs to. Rate, bit depth and latency come from the device and follow its changes. |
| `channels` | all channels of the device in that direction | Device channels (1-based) in wire order. The i-th entry is the i-th channel of the packet. |
| `peer` | none | `tx`: destination address (required; unicast, or a multicast group: the send interface is the flow's interface). `rx`: the **only sender accepted**: datagrams from any other address are dropped and counted (`engine.rx_filtered`), so another host on the segment cannot inject audio. An rx flow needs `peer` or `any_source = true`. The address must be a usable unicast address (not `0.0.0.0`, not broadcast, not multicast): receiving from a multicast group is **not supported yet**. |
| `any_source` | `false` | `rx`: accept datagrams from any address (an explicit opt-in; contradicts `peer`). |
| `port` | required | `tx`: destination UDP port. `rx`: local UDP port to listen on. |
| `interface` | first of `[network] interfaces` (or the default wired interface) | The interface; its first IPv4 address is bound, and the flow's sockets are restricted to it with `SO_BINDTODEVICE` (unprivileged on Linux 5.7 and later; a flow whose socket cannot be restricted does not start). A wireless interface is refused unless `allow_wireless`. |
| `local_address` | the interface's address | Override of the bound address. |
| `format` | `"reference"` | Packet format; `reference` is the only one until the real format exists. |
| `frames_per_packet` | `tx`: 32 at 48 kHz (a 667 us packet, rounded to frames, reduced to fit the MTU) | Frames per packet. Bounded by `floor((mtu_payload - 9) / (channels * bytes per sample))`. A hardware receiver asks for a frame count in its flow request; `tesserad` does not negotiate it, so set this to what the receiver asks for. Automatic negotiation is unavailable. |
| `send_offset_us` | 0 | `tx`: extra delay after a packet's last frame before it is sent (`tx_send_offset`). |
| `dscp` | 46 | `tx`: DSCP of the packets (expedited forwarding). |
| `max_lag_us` | half the device latency, at least one packet period (4 ms latency: 2 ms), taken when the flow starts | `tx`: how late after its release time a packet may still be sent; a flow that falls further behind skips to the latest due packet (never a burst) and counts a `late_skip`. Raise it on hosts whose scheduler wakes the engine late now and then, as far as the receivers' latency allows (latency minus packet duration minus network). |

How a flow behaves:

* **tx** packets are stamped with the media time of their first frame and sent no earlier than the media time of their last frame plus
  `send_offset_us`. The audio is read from the TX client rings of the device (`audio.sock`): positions the client did not write are silence
  (counted as `underrun_samples`), never repeated audio. A client that writes **late** (a graph-driven client writes a cycle's audio when the
  cycle ends) either shifts the positions it writes ahead (as `tessera-pw` does with its TX lead) or needs `send_offset_us` of at least its
  period plus margin.
* Nothing is sent while the media clock is neither `locked`, `leader` nor in holdover, unless `[clock] free_running_tx = true`.
* **rx** datagrams are drained every engine tick, written at their media positions into the flow's buffer, and **delivered** to the RX client rings
  of the device one tick later, once their playout time (`timestamp + latency`) has passed. Missing audio is silence, counted as `hole_samples`;
  a packet that arrives after its playout time is counted as `late_packets` and dropped. The latency published to RX clients is the flow latency
  plus one engine tick plus `[engine] rx_margin_us` (default 250 us), because delivery happens once per tick.
* **rx keepalives** (PROVISIONAL). A unicast transmitter stops a flow whose receiver stays silent (about 1.3 s for hardware), so every rx
  flow sends a keepalive from its own socket (its local address and `port`) to the address and port the media comes from, learned from the
  most recent datagram that decodes as media (one dropped by the `peer` filter, or one that does not decode, never redirects them). With
  `any_source = true` the keepalive target follows any host that sends decodable media: set a `peer` to pin it. The first one goes out as
  soon as media arrives, then one per `rx_keepalive_ms` (default 250; `0` disables); none before the first packet. After
  `rx_keepalive_timeout_ms` (default 10000) without media they stop, and they start again with the next packet; `0` keeps them going as
  long as the flow exists. `rx_keepalive_payload` is `empty` (default: a zero-length datagram, as a hardware receiver sends) or `1337`
  (the two bytes `13 37`, an alternative form, provisional). These three are `[defaults.flow]` keys, and a `[[flow]]` can override each. Counted per flow as `keepalives_sent` and `keepalive_failures` (`tesseractl
  metrics --json`). Multicast reception is not supported yet; multicast flows need no keepalives.
* A change of the device's rate, encoding, latency or channel count rebuilds the device's flows and detaches its audio clients, which re-attach
  with the new ring geometry.
* `SIGHUP` (or `tesseractl config reload`) applies added, changed and removed flows without touching the others and without detaching clients.
  A reload that does not validate is refused and changes nothing.

## Hardware-card bridges (`[[bridge]]`)

An in-daemon bridge between a local sound card and one contiguous channel range of a device: `playback` plays the device's
RX channels on the card (network to card), `capture` feeds the card's input into the device's TX channels (card to network). The card has
its own clock; the bridge absorbs the drift between it and the media clock with an asynchronous resampler whose ratio a PI loop trims. It needs a `tesserad` built with the cargo feature `alsa-bridge` (off by default; it links libasound). A build without it refuses
a configuration that has any `[[bridge]]` entry, at start and at reload, with an error naming the feature.

| Key | Default | Meaning |
|---|---|---|
| `name` | `<device>-<direction>` | Name in status, metrics and logs (letters, digits, `_`, `-`; unique). Two bridges of one device and direction need explicit names. |
| `enabled` | `true` | `false` keeps the entry (shown as `disabled`) but leaves the card alone. |
| `device` | required | `id` of the device. |
| `alsa_pcm` | required | ALSA PCM name of the card: `hw:CARD=<card>,DEV=0`, `hw:Loopback,0,0`, `plughw:1`. Opened non-blocking, interleaved, at exactly the device's rate (the first of S32, FLOAT, S24, S24_3, S16 the card offers). Two bridges may not use the same name in one direction; two *spellings* of one card (`hw:Loopback,0,0` and `hw:CARD=Loopback,DEV=0`) are not recognised as the same: the second one then stays `reopening` with "Device or resource busy". |
| `direction` | `playback` | `playback` or `capture`; the older `rx` / `tx` mean the same. |
| `first_channel` | `1` | First device channel (1-based); card channel 1 carries it. |
| `channels` | the rest of the device's channels | Channel count, which is also the card's. The older form, a list such as `[3, 4]`, is accepted if it is one ascending run. |
| `sample_rate` | the device's | Must equal the device's rate: the resampler's nominal ratio is 1, it compensates drift, not rate differences. Left out, the bridge follows the device (a rate change restarts it). Set, it pins the rate: `tesseractl set-rate` to another rate is refused while the bridge is enabled, and a reload that pins (or re-enables a bridge pinning) a rate other than the device's *effective* rate (including one set at run time) is refused. |
| `period_frames` | 10 ms at the device rate | Frames per card period (the card may round it). |
| `buffer_frames` | 4 periods | Card buffer, at least two periods. The bridge keeps all but one period queued (3 of 4 periods by default: 30 ms of card latency at 10 ms periods), so the playback path adds about that to the device latency. |
| `resampler` | `auto` | `off`, `linear`, `cubic`, `sinc-fast`, `sinc-hq` or `auto` (= `[backends] resampler`, whose `auto` is `cubic` on ARM and `sinc-fast` elsewhere). `off` disables drift compensation: the position error is then removed by re-anchoring, an audible slip. |
| `time_constant_s` | `120` | Time constant τ of the drift loop in seconds (a number; fractions allowed), `τ = 1 / (2π f_n)` with `f_n` the loop's natural frequency (critically damped). Long on purpose: see below. Any positive value whose transient `0.4 × max_correction_ppm × 1e-6 × time_constant_s` is at most 60 s (the guard, the lead and the ring grow by it; 750 000 s at 200 ppm); below 4 s (ten times the bridge's 0.4 s filter on the measured error) is a warning, since the loop may ring. |
| `max_correction_ppm` | `200` | Largest correction of the resampling ratio, feed-forward included, in ppm (a number; fractions allowed). Above 0, at most 3000 (the range the bridge's resampler is built for). A card whose offset is above the clamp cannot be followed: the error grows until the loop re-anchors (counted as `slips`). Lowering it live below the offset the loop has learned is reported as the bridge's `last_error`. |
| `rt_priority` | `75` | `SCHED_FIFO` priority of the bridge thread; `0` = normal scheduling; `[daemon] realtime = false` forces normal scheduling. |
| `cpu` | not pinned | CPU to pin the bridge thread to. By default it is not pinned (and so does not share the media worker's CPU from `[engine] cpu_affinity`); a change restarts the bridge. A CPU that cannot be used is reported as `rt_degraded`. |
| `error_backoff`, `error_backoff_min_ms`, `error_backoff_max_ms`, `error_sleep`, `error_sleep_min_ms`, `error_sleep_max_ms` | `[defaults.bridge]`: `true`, `500`, `30000`, `true`, `2`, `64` | The wait before a failed card is reopened, and the pause of the bridge thread after an xrun or card error (see "Reopen back-off and the error pause"). Set in `[defaults.bridge]` for every bridge; an entry that names one overrides it for this bridge. |

**Headroom for a slow loop.** A loop with time constant τ and clamp c lets a position error of up to about `0.37 c τ` seconds build up
when it meets a clock offset as large as the clamp (the first start): about 8.9 ms at the defaults (120 s, 200 ppm). The playback guard (how far behind the newest delivered audio the bridge reads) and the capture lead are at least
`0.4 c τ` plus one (playback) or two (capture) periods, and the re-anchor threshold at least twice that, so short periods or a slower loop
get more headroom (and that much more latency, shown as `added_latency_frames`) automatically, and the ring the bridge reads or writes is
sized for it. A live retune whose transient no longer fits the headroom of the open bridge reopens it at once with the new tuning (the
same bridge: its counters and history stay; the new open counts in `reopens` and changes `generation`, it is not an open failure).
The ring is shared memory of `capacity × channels × 4` bytes, its capacity the next power of two above latency, card buffer, a period and
the guard: a slow loop makes it large. At the 60 s transient limit a 2-channel bridge at 48 kHz needs 4 Mi frames (32 MiB); at 96 kHz
8 Mi frames, 64 MiB of samples for 2 channels (the header makes it just over the default limit) and 2 GiB for 64. A ring whose total size, header included, is above `[audio] max_ring_mib` (64 MiB per ring by default) is refused:
the bridge is `failed` with that reason (lower the time constant or the clamp, or raise the limit).

**Time constant and a second loop downstream.** When the card the bridge plays into is itself servoed by another loop (a DSP stage reading
the other end of an `snd-aloop` cable and trimming the cable's `PCM Rate Shift` to follow the DAC), the two loops must not fight: keep the
bridge's `time_constant_s` at least ten times the downstream loop's adjustment period, and the clamp no larger than the offsets you expect.
Then the bridge only follows the average rate the downstream loop settles on. The default, 120 s, meets that rule for CamillaDSP's default
`adjust_period` of 10 s; a downstream loop with a longer period needs a longer time constant. Straight to a DAC (no second loop), a shorter
time constant (10 to 30 s) converges faster.

### Reopen back-off and the error pause

Two waits protect the system from a card that keeps failing. Both exist only in the error path, never in a healthy run, both are set in
`[defaults.bridge]` for all bridges and in a `[[bridge]]` entry for one, and both are `reload` keys that apply at the bridge's next error (a
running card is never reopened or disturbed because of a change). The defaults are the values that were built in before they became keys.

**1. The reopen wait** (`error_backoff*`). A bridge whose card is missing, busy, cannot be configured, was unplugged, or has an xrun or card
error in each of three consecutive seconds is closed and opened again after a wait (state `reopening`, the reason in `last_error`). The wait
is kept by the bridge manager, a normal-priority control thread, which schedules the next attempt.

| Key | Default | Meaning |
|---|---|---|
| `error_backoff` | `true` | Double the wait after each failed reopen. `false`: wait `error_backoff_min_ms` every time. It never means "do not wait". |
| `error_backoff_min_ms` | `500` | The first wait, and the wait after a run of 10 s or more. At least 10. |
| `error_backoff_max_ms` | `30000` | The longest wait. At least `error_backoff_min_ms`, at most 600000 (10 minutes). |

Why, and why there is a floor: a card that keeps failing must not be opened again in a tight loop. Every attempt opens the card and builds
the bridge, and a card that opens and fails again starts a `SCHED_FIFO` bridge thread each time, so without a wait a broken card would keep
the manager and a series of real-time threads busy and could starve the rest of the machine, the clock and media threads included. Lower
`error_backoff_max_ms` to recover sooner from a card that comes back (a USB interface that is plugged in again), raise it to retry a card that
is gone less often.

**2. The error pause** (`error_sleep*`). While the card stays open, the running bridge thread pauses after a step that met an xrun or a card
error, so that a card in trouble cannot turn the `SCHED_FIFO` loop into a spin. Polling the card cannot replace the pause: in exactly those
states the card reports the error at once.

| Key | Default | Meaning |
|---|---|---|
| `error_sleep` | `true` | Double the pause on each further step with an error; the first clean step resets it. `false`: pause `error_sleep_min_ms` every time. It never means "do not pause". |
| `error_sleep_min_ms` | `2` | The first pause. At least 1. |
| `error_sleep_max_ms` | `64` | The longest pause while the trouble lasts. At least `error_sleep_min_ms`, at most 1000. Higher values use less CPU on a broken card but make an xrun recover more slowly (the card is serviced again only after the pause). |

This pause is the one system call the bridge thread makes besides the card's I/O and `clock_gettime` (an exception to the rule that a real-time thread makes
no other system call, taken because polling the card cannot replace the pause). It is never called in a healthy run
(counted as `backoffs` in the bridge's health), and the thread reads the setting lock-free, so a reload applies at its next error. Each key can
be overridden per `[[bridge]]`; both pairs are validated with the values the bridge ends up with.

How a bridge runs:

* **Threads.** One control thread for all bridges (normal priority) opens the cards, builds the bridges (resampler, buffers), creates a ring
  for the bridge's channel range sized like an audio client's, attaches its other end to the media engine and the clock overlay, and then
  hands the bridge to its own real-time thread. That thread does only card I/O, the resampler and the loop (no allocation, lock or log;
  besides the card's calls and `clock_gettime` only a `clock_nanosleep` (2 to 64 ms by default, `error_sleep*`) after an xrun or card error; see "Reopen back-off and the error pause"). Live retunes reach it through the bridge's lock-free queue, the controller already built; a retune the queue cannot take yet
  is retried, and the status shows the tuning in effect.
* **Playback** reads the RX ring `effective latency + guard` behind the media clock (the same latency an audio client gets, including the
  `[engine] rx_margin_us` margin), so the audio leaves the card about `latency + guard + card queue` after its media time. Positions the network did
  not deliver are silence (counted in `missing_frames`). **Without a subscription or a flow** the card plays continuous digital silence; the
  card never stops.
* **Capture** writes the TX ring three chunks ahead of the media clock; a TX flow of the device sends it like an audio client's.
* **Card failures never stop the daemon or another bridge.** A card that is missing, busy (another program has it) or cannot be configured, a card
  unplugged while running, and a card that has an xrun or a card error in each of three consecutive seconds is closed and reopened after a
  back-off (by default 0.5 s doubling to 30 s, see "Reopen back-off and the error pause"; state `reopening`, the reason in `last_error`). A single under- or overrun is recovered in place and
  counted (`xruns`). If the bridge cannot work with what the card accepted (for example a buffer too small for its sizes) or the device rate no
  longer matches a pinned `sample_rate` (also a warning in `tesseractl status`), the state is `failed` until the configuration changes. A
  panic on a bridge thread (a bug) ends only that bridge: it is `failed` with the panic message, the daemon and the other bridges go on.
  (A panic of the bridge manager's own thread aborts the daemon, which the service manager restarts.)
* **Status** (`tesseractl status`, `tesseractl bridges`, `tesseractl metrics`, `Query::Bridges`, the JSON status of `tessera-web`): per
  bridge the state (`starting`, `running`, `reopening`, `failed`, `disabled` for `enabled = false`), card, direction, channels, card fill
  (frames queued in the card), the loop's position error (frames), its correction and learned offset (`correction_ppb`, `trend_ppb`), the
  resampling ratio, the time constant and clamp in effect, xruns, `reopens` (successful opens after the first; failed attempts are
  `open_failures`), `generation` (changes on every start of the bridge, not on a live retune), card errors, chunks, silent chunks (capture:
  discarded), missing frames (not delivered by the network), short frames (playback: frames the bridge's own resampler did not produce,
  played as silence; 0 when healthy), slips, whether real-time scheduling was refused, and the last error. Gauges are at most about a second
  old.

## Engine, clock and discovery at run time

What is applied live (reload, `tesseractl`, the portal) and what needs a restart:

| Change | Effect |
|---|---|
| `[clock] role`, `[clock] priority` (leader-capable roles only; a follower never uses it), `[clock] timestamping` | Applied live: sent as a command to the running PTP runner, which re-runs the election. **No restart, no step of media time, the epoch does not change** (it changes only if the servo later has to step). A role change on an isolated node moves media time by 0 ns; between two daemons the difference stays within the sync error (measured: a few microseconds). `timestamping = hardware` on an interface that cannot stamp PTPv1 is rejected: the previous mode stays, the daemon logs an error and counts it in `commands_rejected`. |
| `tesseractl network primary <name>` (runtime selection) | Same as a change of the primary interface below. Kept in `state.toml` across restarts; `tesseractl network primary default` clears it. |
| `[network] interfaces` (the primary interface) | The PTP runner of the affected interface is restarted. The overlay and its writer are kept, the new port starts at the old mapping's time and rate and is published as `holdover` while it catches up (transmit keeps flowing), and the **epoch changes** (the new port is created a thread start-up after the old mapping was sampled, so media time is not exactly continuous). |
| `[clock] free_running_tx` | Applied live. |
| `[[flow]]` | Applied live (see above). |
| `[[bridge]]` | Applied live, per bridge: a changed card, direction, channels, sizes, resampler, priority (or a device rate, latency or channel count change) restarts only that bridge; a changed `time_constant_s` or `max_correction_ppm` is applied to the running loop without a restart; unchanged bridges are not touched. |
| device name, rate, encoding, latency, channel labels | Applied live; the mDNS advertisement is updated (rename, channel records); rate, encoding and latency also rebuild the flows and detach clients. |
| `[discovery] mdns` | Applied live (the backend is restarted). |
| `[engine]` (`workers`, `tick_us`, `rt_priority`, `ptp_rt_priority`, `cpu_affinity`, `mlock`, the batch and buffer sizes) | Takes effect at the next service start: a reload logs each changed key by name and the status shows it until then. `ptp_rt_priority` and the PTP CPU are read at start too (a PTP runner restarted for an interface change keeps the values the daemon started with). |

* **Priority mapping (PROVISIONAL, confidence: guess).** PTPv1 has no priority field (`OQ-CLK-1`). `tessera-clock` compares dataset members in the order
  preferred, stratum, identifier, variance, ...; a leader-capable port advertises `[clock] priority` (lower wins, default 128) as: *preferred* for
  priority below 64; *stratum* 4 up to priority 128 and one worse per further 32 (255 gives 7; never 1-3, which could outrank hardware); and
  *variance* around the reference default (-4000, 16 per step) for the fine ordering. The default priority advertises exactly what an unconfigured
  leader-capable port does (stratum 4, not preferred, variance -4000). A lower priority never loses on any criterion. A follower advertises
  slave-only (stratum 255) whatever the priority, so a priority change on a follower sends nothing. A node promoted live from follower keeps its
  current timeline: no jump to TAI and no epoch change, unlike a first start as a leader.
* **Primary interface.** In order: with `[network] locked = ["primary"]` the configured or automatic choice, and runtime selection is refused;
  else a runtime selection (`tesseractl network primary <name>`) if its interface exists, has an IPv4 address and is not Wi-Fi (unless
  `allow_wireless`); else the first entry of `[network] interfaces`; else automatically the first wired, non-virtual interface (by name) with
  carrier and an IPv4 address, re-evaluated as links change. A selected interface that is missing is not an error: the daemon falls back,
  warns, shows `fallback` in `tesseractl network show`, and switches back when the interface returns. The device identity does not change
  with the interface; the PTP clock identity (the interface MAC) and the clock epoch do.
* **Interfaces.** The first interface of `[network] interfaces` is the primary: its PTP port publishes the media clock all devices share. Further
  interfaces run their own PTP port (status only). A configuration with only `lo` has no media network: PTP and discovery are not started.
  If a PTP runner cannot start (no address, no privilege for the PTP ports) the clock stays free-running, the failure is logged and the start is
  retried every few seconds.
* **`[engine] workers`**: only one media worker is implemented; a larger value is logged and ignored.
* **The data path** is tuned by `[engine]` (`tick_us`, `rx_batch`, `rx_max_batches`, `tx_batch`, `tx_max_per_tick`, `socket_buffer_kib`,
  `rx_margin_us`; read at start) and `[defaults.flow]` (`rx_keepalive_ms`, `rx_keepalive_timeout_ms`, `rx_keepalive_payload`, per flow at its
  next (re)start, each `[[flow]]` can override them); `[daemon] realtime = false` never requests real-time scheduling or locks memory,
  `[clock] enabled = false` runs no PTP, `[discovery] enabled = false` advertises nothing. The command-line options of the same names
  override them ("`tesserad` command line" above).
* **Real-time scheduling is requested, never silently missing.** If `SCHED_FIFO`, the CPU pin or `mlockall` is refused the daemon logs a warning at
  start and reports it in `tesseractl metrics` (`engine.rt_degraded`, `engine.mlock_failed`).
* **Discovery.** Enabled by default; set `[discovery] advertise_devices = false` to disable device records. Each device is advertised with `tessera-discovery` (ARC, CMC and one channel record per transmit channel; a renamed channel gets its factory record and a friendly record) by the backend `[discovery] mdns` selects, and withdrawn (goodbye packets) when the daemon stops. The packaged Controller compatibility runtime serves the first logical device's base control ports; additional logical devices' offset control ports remain unsupported. The daemon logs a warning once per advertised port that no socket is bound to on the wildcard address or a media interface's current address (and an info line when a listener appears). A separate process may answer for it, so this never stops the advertisement. See `troubleshooting.md`.
* **Socket buffers.** Each media socket asks for `[engine] socket_buffer_kib` (2048) KiB. The unit has no `CAP_NET_ADMIN`, so the kernel caps the request at
  `net.core.rmem_max` / `wmem_max` (about 208 KiB by default); `dist/sysctl.d/tessera.conf` raises them to 8 MiB; the `tessera` package installs it as `/usr/lib/sysctl.d/60-tessera.conf` and applies it in the postinst (from a source tree: `sudo install -m 0644 dist/sysctl.d/tessera.conf /etc/sysctl.d/` and `sudo sysctl --system`). The daemon logs the effective size
  of every socket, warns when it was capped, and reports the smallest one and the number of capped sockets in `tesseractl metrics`
  (`engine.socket_buffer_min_bytes`, `engine.socket_buffers_capped`). The wake-up histogram (`engine.wake_lateness_us{<25}` to `{>=4000}` in
  `tesseractl metrics`, `wake_lateness_hist` in the JSON) and `engine.max_tick_work_us` show how late the
  worker woke up and how long a tick took, to attribute a late skip or a hole.

## Schema reference

Every key, its default, unit, range, apply class and restart scope. The tables below, the commented reference file and the checks are
generated from one table in `tessera-state` (`crates/tessera-state/src/schema.rs`).

**The reference file.** `tesserad --print-default-config` prints the full configuration with every key commented out at its default and a
short what-and-why (`tessera_state::reference_config`). Packages install it as `/etc/tessera/config.toml` (a configuration file of the
package: local edits there conflict with upgrades, so keep it as shipped) and as `/usr/share/doc/tessera/examples/config.toml`; put your
changes in `config.d/`. Uncommenting a key line gives a valid configuration; the reference as shipped loads to exactly the built-in
defaults. There is no separate defaults file.

**Types.** The Type column is the TOML type of the default. A `float` key also takes an integer (`max_correction_ppm = 200` and `200.0`
are the same), and a key marked `integer or string` takes either (`tick_us = 1000` or `"auto"`, `socket_mode = "660"` or
`0o660`, never a decimal that is meant as octal).

**Unknown keys are errors**, everywhere in the configuration, and the message lists the valid keys of that table. A daemon older than a key
therefore refuses a file that uses it: upgrade the daemon before using new keys. All problems are reported together with their key paths:
a value of the wrong type only hides the checks that read that value.

**Apply classes.** `live`: can also be changed at run time by a command (tesseractl, the web portal, a network controller); the run-time
value is stored in `state.toml` and wins over the files. A reload applies a changed file value only if no run-time value is stored for it;
otherwise the reload warns that the edit is hidden, and adding the key to `locked` makes the file win. `reload`: takes effect on `SIGHUP` /
`tesseractl config reload`. `restart`: takes effect at the next daemon start; a reload reports every changed `restart` key (never silently).

**Restart scope** says what a change interrupts: `none` (applied in place, or at the next natural (re)start, like the `[defaults.*]` keys),
`device` (that device's flows or bridges restart, its audio clients may be detached, it is re-announced: a short silence on its
channels; `[[flow]]`/`[[bridge]]` entries, `network.mtu_payload` and `network.allow_wireless` are `device` because changing them restarts
flows or bridge threads) or `daemon` (the whole service; no command produces it today). The scope says what restarts now: keys of apply
class `restart` changed by a reload are listed separately (`restart_keys`, "takes effect at the next service restart") and restart
nothing now, so they raise neither the scope nor a confirmation. There is no finer
scope for a single flow or bridge: the impact lists the flows and bridges, and the device is what the user hears stop.

**Confirmation of restarting changes.** A change made through a command (CLI, web, API) whose restart scope is not `none` is applied only
after the user confirmed it: sent unconfirmed, it is refused with `confirmation_required` carrying the impact (what restarts and why, what is
dropped, roughly how long, what to do afterwards, the subscriptions it deletes, the ring memory it costs, the restart-class keys waiting for
the next service restart), and
nothing changes. Locks, invalid values and unknown devices are reported first. The client shows the impact and sends the command again as a
confirmed command carrying the impact's token (`tesseractl ... --yes`, the confirm step of a web form). If the impact changed in between
(the files were edited again, another client added a subscription on a channel that goes away), the daemon does not apply it and answers
`confirmation_required` again with the new impact. A confirmation without a token (a script passing `--yes` without a dry run) applies
whatever the impact is. A dry run (`set_device_channels` with `dry_run`) only returns the impact (`Reply::Impact`); it changes nothing, so any peer that may read
the status may ask for it, while every command, confirmed or not, needs root or the admin group. A confirmed command answers
with the impact of what it did (for a reload: the `restart_keys` too); a plain command still answers `done`.
Changes that restart nothing behave as before; a command that changes nothing needs no confirmation. `tesseractl config reload` is asked too when
the edited files restart a device (a reload that changes only `restart` keys is not). Not asked: network controllers (`Origin::Protocol`:
they confirm in their own interface and cannot answer our question), `SIGHUP` (the administrator edited the files; it only logs), the
daemon itself.

| Command | Restart scope | Why |
|---|---|---|
| `set_device_name`, `set_channel_label` | none | re-announced in place; audio does not stop |
| `subscribe`, `unsubscribe` | none | only that RX channel changes |
| `set_clock_role`, `set_clock_priority` | none | sent to the running PTP port as live commands |
| `set_sample_rate`, `set_encoding`, `set_latency` | device | the device's flows are rebuilt and its audio clients detached (new ring geometry) |
| `set_device_channels` | device | as above; channels beyond the new counts and their subscriptions are removed |
| `set_primary_interface` | device (every device) | PTP restarts on the new interface and every flow moves |
| `reload_config` | computed from the changed files | device for geometry, interfaces, `[[flow]]`/`[[bridge]]` entries, MTU, Wi-Fi permission or a removed device; changed `restart` keys are listed, not gated |

**Runtime channel counts.** `set_device_channels` changes a device's RX and TX channel counts and stores them in `state.toml`. A device
configured with 0 channels in one direction (a playback-only box, `tx_channels = 0`) keeps that direction at 0 and can change
the other. A direction that has channels cannot be taken to 0 while the device runs, by the command or by a reload: set 0
channels in the config file and restart the daemon instead. The only ceilings are the 16-bit count
of the wire format and memory: one audio client ring per direction spanning every channel must fit `[audio] ring_budget_mib`. It is refused
while a `[[flow]]` or `[[bridge]]` entry uses a channel that would go away (change that entry first), and with `locked` when `rx_channels`
or `tx_channels` is locked and that count would change; every reason is reported at once, with its key path (`device[0].rx_channels`).
When RX channels go away, the subscriptions on them are deleted, also stored ones that were already inactive (the one exception to "kept
but inactive"), and the impact lists them all; a request that leaves the effective counts unchanged deletes nothing;
labels of removed channels are kept and come back with the channels. A stored pair that no longer fits (a new flow in the files, a smaller
budget) is kept but inactive, with a warning; sending it again reports why.

**Bridge error waits** (`[defaults.bridge]`, per-`[[bridge]]` overrides). Two waits, each only after an error, never in steady
state, both applied at the bridge's next error:

* `error_backoff*`: how long the bridge manager waits before it reopens a card that failed (by default 500 ms doubling to 30 s): it
  waits `error_backoff_min_ms`, doubling after each failed reopen up to `error_backoff_max_ms`; a 10 s run resets it (a named constant:
  the definition of "recovered"). `error_backoff = false` waits `error_backoff_min_ms` every time; there is never no wait. The floor of
  10 ms keeps a card that keeps failing from being reopened, and its real-time thread restarted, in a tight loop.
* `error_sleep*`: how long the running bridge thread pauses after a step that met an xrun or a card error, while the card stays open
  (by default 2 ms doubling to 64 ms while the trouble lasts; the first clean step resets it). `error_sleep = false` pauses
  `error_sleep_min_ms` every time; there is never no pause (floor 1 ms, ceiling 1000 ms). Without the pause a card in trouble would make
  the `SCHED_FIFO` thread spin: in exactly those states the card reports the error at once, so waiting on the card cannot replace it.
  This is the one system call the bridge thread makes besides the card's I/O and the clock (an exception to the rule that a real-time thread makes
  no other system call); the
  thread reads the setting lock-free, so a reload applies at its next error.

**Command-line options** of `tesserad` each override one key (`--no-ptp` is `[clock] enabled = false`, `--no-discovery` is
`[discovery] enabled = false`, `--no-rt` is `[daemon] realtime = false`, `--log-level` is `[daemon] log_level`, `--control-threads` is
`[control] threads`, the `--audio-*` options are `[audio]` keys, the `--rx-keepalive-*` options are `[defaults.flow]` keys). An overridden
key that commands could change (`clock.role`, `clock.priority`) is locked: the stored run-time value is skipped and commands are refused,
naming the command line as the source.

<!-- schema:begin (generated by tessera_state::schema::markdown_tables) -->
#### `[daemon]`

The process: logging, file locations, shutdown, real-time scheduling.

| Key | Type | Default | Unit | Range | Apply | Restart scope | What and why |
|---|---|---|---|---|---|---|---|
| `log_level` | string | `"info"` |  | error\|warn\|info\|debug\|trace or target=level,... | reload | none | Log filter. Raise to debug to diagnose a problem. --log-level overrides it, and the RUST_LOG environment variable overrides both (and is not changed by a reload). |
| `state_dir` | string | `"/var/lib/tessera"` |  | absolute path | restart | daemon | Where state.toml (values changed at run time, device identities) is kept. |
| `runtime_dir` | string | `"/run/tessera"` |  | absolute path | restart | daemon | Where control.sock and audio.sock are created. |
| `admin_group` | string | `"tessera-admin"` |  | group name | reload | none | Members of this group may change settings through the control socket; others may only read. |
| `shutdown_timeout_secs` | integer | `20` | s | 1..=4294967295 | reload | none | Time allowed for the final state flush and shutdown. Keep the service manager's stop timeout above it. |
| `persist_delay_ms` | integer | `500` | ms | 0..=4294967295000 | reload | none | Debounce window for writing state.toml after a change. Longer means fewer writes on flash storage; a crash inside the window loses the change. |
| `realtime` | boolean | `true` |  | true\|false | restart | daemon | Request SCHED_FIFO priorities and lock memory as [engine] says. false never does (development, containers without the capability); audio then has no timing guarantees. |

#### `[control]`

The local control socket (`control.sock`) used by tesseractl and the web portal: frame size, connection limits, timeouts, rate limits.

| Key | Type | Default | Unit | Range | Apply | Restart scope | What and why |
|---|---|---|---|---|---|---|---|
| `max_frame_bytes` | integer | `1048576` | bytes | 65536..=4294967295 | reload | none | Largest request or response on the control socket. Clients learn it from the status. Raise it if a status with very many devices or channels is refused as too large. |
| `max_connections` | integer | `256` |  | >= 1 | reload | none | Most simultaneous control connections (all users). |
| `max_connections_per_uid` | integer | `16` |  | >= 1 | reload | none | Most control connections one user may hold, so one user cannot use up the slots. |
| `admin_reserved_connections` | integer | `16` |  | < max_connections | reload | none | Connection slots only administrators may use, so other users cannot lock them out. |
| `idle_timeout_secs` | integer | `60` | s | 1..=4294967295 | reload | none | A request/response connection silent this long is closed (event subscribers are exempt). |
| `event_queue` | integer | `256` | events | >= 1; event_queue x max_connections x event size <= 2 GiB | reload | none | Events queued per subscriber before the oldest are dropped (the subscriber is told how many). |
| `rate_burst` | integer | `256` | requests | >= 1 | reload | none | Requests a peer may send back to back before the rate limit applies. |
| `rate_per_second` | integer | `128` | requests/s | >= 1 | reload | none | Requests per second a peer may sustain. |
| `frame_timeout_secs` | integer | `10` | s | 1..=4294967295 | reload | none | Time a peer may take to finish a frame, counted from its first byte (trickling bytes does not extend it). |
| `write_timeout_secs` | integer | `5` | s | 1..=4294967295 | reload | none | Time to wait for a peer that does not read its responses before closing it. |
| `stream_write_timeout_secs` | integer | `30` | s | 1..=4294967295 | reload | none | Time to wait for a stalled event subscriber before dropping it. |
| `threads` | integer | `2` |  | >= 1 | restart | daemon | Worker threads of the control-plane runtime (not the audio threads). |
| `socket_group` | string | unset (e.g. `"tessera"`) |  | group name or number | restart | daemon | Group that owns the sockets. Unset: the group tessera if it exists (a warning otherwise); a group set here that cannot be applied is an error. |
| `socket_mode` | integer or string | `0o660` | octal | 0o000..=0o777, written in octal (0o660 or "660"; a decimal number is refused) | restart | daemon | Permission bits of control.sock and audio.sock. Write them in octal: socket_mode = 416 would be a different mode, so a decimal integer is an error. |

#### `[audio]`

Audio clients on `audio.sock` (PipeWire, ALSA, JACK): how many, and how much shared ring memory they may use.

| Key | Type | Default | Unit | Range | Apply | Restart scope | What and why |
|---|---|---|---|---|---|---|---|
| `socket` | string | `"auto"` |  | auto\|on\|off | restart | daemon | Listen on audio.sock. auto: if [backends] enabled names a client backend. |
| `max_clients` | integer | `64` |  | >= 1 | reload | none | Most simultaneous audio client connections. Applies to new clients. |
| `max_clients_per_uid` | integer | `8` |  | >= 1 | reload | none | Most audio client connections one user may hold. |
| `default_period_frames` | integer | `1024` | frames | 1..=max_period_frames | reload | none | Client period assumed when a client does not state one; sizes its ring. |
| `max_period_frames` | integer | `16384` | frames | >= 1 | reload | none | Largest period a client may ask for. |
| `ring_budget_mib` | integer | `256` | MiB | >= 1 | reload | none | Shared ring memory all audio clients together may reserve. Also the budget a channel-count change is checked against. Keep it under RLIMIT_MEMLOCK when memory is locked. |
| `max_ring_mib` | integer | `64` | MiB | 1..=ring_budget_mib | reload | none | Largest single client ring. Many channels at long latencies need more. At most ring_budget_mib. |
| `reorder_window_us` | integer | `10000` | us | >= 0 | reload | none | Extra ring capacity for packets that arrive out of order. |

#### `[network]`

Network interfaces.

| Key | Type | Default | Unit | Range | Apply | Restart scope | What and why |
|---|---|---|---|---|---|---|---|
| `interfaces` | array | `[]` |  | interface names | reload | device | Interfaces for audio and clock; the first is the primary. Empty: the first wired interface with a carrier and an IPv4 address. |
| `secondary` | string | unset (e.g. `"enp4s0"`) |  | interface name | reload | device | Redundancy interface (Phase 6). |
| `allow_wireless` | boolean | `false` |  | true\|false | reload | device | Allow Wi-Fi interfaces for media. Wi-Fi timing is poor; use it only for tests. |
| `mtu_payload` | integer | `1400` | bytes | 1..=65535 | reload | device | Largest media payload per packet. |
| `locked` | array | `[]` |  | primary | reload | none | Values that run-time commands may not change (primary: the runtime interface selection). |

#### `[clock]`

The PTP clock. It drives a media clock only; the system clock is never steered.

| Key | Type | Default | Unit | Range | Apply | Restart scope | What and why |
|---|---|---|---|---|---|---|---|
| `enabled` | boolean | `true` |  | true\|false | restart | daemon | Run PTP. false leaves the media clock free-running (--no-ptp); devices then cannot sync with others. Read at start only. |
| `role` | string | `"follower"` |  | follower\|leader\|auto | live | none | Clock role. auto: leader-capable but follower-preferred. |
| `priority` | integer | `128` |  | 0..=255 (lower wins) | live | none | Leader-election priority. |
| `timestamping` | string | `"auto"` |  | auto\|hardware\|software | reload | none | PTP timestamps: hardware if the interface supports it, else software. |
| `free_running_tx` | boolean | `false` |  | true\|false | reload | none | Transmit while the clock is not locked. |
| `locked` | array | `[]` |  | role, priority | reload | none | Values that run-time commands may not change. |

#### `[engine]`

The media engine's real-time threads and packet batching. These size real-time structures, so they apply at the next start.

| Key | Type | Default | Unit | Range | Apply | Restart scope | What and why |
|---|---|---|---|---|---|---|---|
| `workers` | integer | `1` |  | >= 1 | restart | daemon | Media worker threads. |
| `tick_us` | integer or string | `"auto"` | us | auto or >= 1 | restart | daemon | Engine tick: how often audio clients and the receive drain wake (auto: 1000). |
| `rt_priority` | integer | `80` |  | 1..=99 | restart | daemon | SCHED_FIFO priority of the media threads. |
| `ptp_rt_priority` | integer | `70` |  | 1..=99 | restart | daemon | SCHED_FIFO priority of the clock thread. |
| `cpu_affinity` | array | `[]` |  | CPU numbers | restart | daemon | CPUs to pin the real-time threads to (isolated cores give the best timing). |
| `mlock` | boolean | `true` |  | true\|false | restart | daemon | Lock memory once the real-time objects exist, so page faults cannot stall audio. |
| `rx_batch` | integer | `32` | datagrams | 1..=1024 (UIO_MAXIOV) | restart | daemon | Datagrams per recvmmsg call. The kernel takes at most UIO_MAXIOV (1024) per call; each one is a preallocated, memory-locked buffer of one packet. |
| `rx_max_batches` | integer | `8` |  | >= 1 | restart | daemon | Receive batches per socket per tick (bounds the work of one tick). |
| `tx_batch` | integer | `32` | datagrams | 1..=1024 (UIO_MAXIOV) | restart | daemon | Datagrams per sendmmsg call. The kernel takes at most UIO_MAXIOV (1024) per call. |
| `tx_max_per_tick` | integer | `32` | packets | >= 1 | restart | daemon | Packets per transmit flow per tick (bounds the work of one tick). |
| `socket_buffer_kib` | integer | `2048` | KiB | 1..=2097151 | restart | daemon | Kernel socket buffer of media sockets. Raise it if the receive-drop counters grow; the sysctl ceilings (net.core.rmem_max) cap it. |
| `rx_margin_us` | integer | `250` | us | >= 0 | restart | daemon | Added to the receive latency published to audio clients on top of one tick (wake-up jitter allowance). |

#### `[discovery]`

mDNS advertisement of the logical devices.

| Key | Type | Default | Unit | Range | Apply | Restart scope | What and why |
|---|---|---|---|---|---|---|---|
| `enabled` | boolean | `true` |  | true\|false | restart | daemon | Discovery at all. false advertises nothing whatever advertise_devices says (--no-discovery). Read at start only. |
| `mdns` | string | `"auto"` |  | auto\|avahi\|builtin\|off | reload | none | mDNS implementation: auto uses Avahi if it is on the bus, else the built-in responder. |
| `advertise_devices` | boolean | `true` |  | true\|false | reload | none | Advertise the logical devices. Enabled by default with the packaged Controller compatibility runtime. |

#### `[backends]`

Which audio-client backends may attach, and the resampler.

| Key | Type | Default | Unit | Range | Apply | Restart scope | What and why |
|---|---|---|---|---|---|---|---|
| `enabled` | array | `["pipewire", "alsa"]` |  | pipewire, alsa, jack | restart | daemon | Audio-client backends that may attach. |
| `resampler` | string | `"auto"` |  | off\|linear\|cubic\|sinc-fast\|sinc-hq\|auto | reload | none | Resampler quality (auto: per platform). |

#### `[aes67]`

AES67 (Phase 5).

| Key | Type | Default | Unit | Range | Apply | Restart scope | What and why |
|---|---|---|---|---|---|---|---|
| `enabled` | boolean | `false` |  | true\|false | restart | daemon | Enable AES67 (Phase 5). |

#### `[defaults.flow]`

Defaults for every [[flow]] entry (each entry can override them). Applied when a flow (re)starts.

| Key | Type | Default | Unit | Range | Apply | Restart scope | What and why |
|---|---|---|---|---|---|---|---|
| `rx_keepalive_ms` | integer | `250` | ms | >= 0 | reload | none | Interval of the receiver keepalives every rx flow sends to its source (a unicast transmitter stops without them); 0 disables them. PROVISIONAL. |
| `rx_keepalive_timeout_ms` | integer | `10000` | ms | >= 0 | reload | none | Stop an rx flow's keepalives after this long without media (they resume with the next packet); 0: never stop. |
| `rx_keepalive_payload` | integer or string | `"empty"` |  | "empty" or "1337" (strings; the integer 1337 is accepted too) | reload | none | What a keepalive carries: empty (a zero-length datagram, as hardware receivers send) or 1337 (the bytes 13 37). PROVISIONAL. |

#### `[defaults.bridge]`

Defaults for every [[bridge]] entry (each entry can override them). Two waits, both only after an error, never in steady state. error_backoff*: when a card is missing, busy, unplugged or keeps failing, the bridge manager (a normal-priority thread) closes it and waits before reopening it, so a failing card is not reopened (and its real-time thread restarted) in a tight loop; the wait starts at error_backoff_min_ms and doubles up to error_backoff_max_ms, and a run of 10 s resets it. error_sleep*: while the card stays open, the real-time bridge thread pauses after a step that met an xrun or a card error, so a card in trouble cannot make it spin (polling the card cannot replace the pause: the card reports an error at once in exactly those states); the pause starts at error_sleep_min_ms and doubles up to error_sleep_max_ms while the trouble lasts, and the first clean step resets it. Changes apply at the bridge's next error.

| Key | Type | Default | Unit | Range | Apply | Restart scope | What and why |
|---|---|---|---|---|---|---|---|
| `error_backoff` | boolean | `true` |  | true\|false | reload | none | Double the wait after each failed reopen. false: always wait error_backoff_min_ms (there is never no wait). |
| `error_backoff_min_ms` | integer | `500` | ms | 10..=error_backoff_max_ms | reload | none | First wait of the bridge manager before it reopens a failed card. The floor of 10 ms stops a failing card being reopened (and its real-time thread restarted) in a tight loop; the manager itself is a normal-priority thread. |
| `error_backoff_max_ms` | integer | `30000` | ms | error_backoff_min_ms..=600000 | reload | none | Longest wait between reopens. Lower it to recover sooner from a card that comes back. |
| `error_sleep` | boolean | `true` |  | true\|false | reload | none | Double the bridge thread's pause on each further step with an xrun or card error. false: always pause error_sleep_min_ms (there is never no pause). The pause is the one system call the real-time bridge thread makes besides the card's I/O and the clock. |
| `error_sleep_min_ms` | integer | `2` | ms | 1..=error_sleep_max_ms | reload | none | First pause of the bridge thread after a step with an xrun or card error. The floor of 1 ms keeps a card in trouble from making the real-time thread spin. |
| `error_sleep_max_ms` | integer | `64` | ms | error_sleep_min_ms..=1000 | reload | none | Longest pause of the bridge thread while the trouble lasts. Higher values use less CPU on a broken card but make an xrun recover more slowly (the card is serviced again only after the pause). |

#### `[[device]]`

A logical device as it appears on the network. Several are allowed; the defaults below apply to each. Values marked live can also be changed at run time and are then stored in state.toml (pin them with `locked`).

| Key | Type | Default | Unit | Range | Apply | Restart scope | What and why |
|---|---|---|---|---|---|---|---|
| `id` | string | required (e.g. `"main"`) |  | letters, digits, _ and - | reload | none | Local key of the device (not its network identity). Required. Changing it makes a new device. |
| `name` | string | host (e.g. `"studio-pc"`) |  | 1..=31 bytes, no control characters (PROVISIONAL: the size of the name on the wire, OQ-CFG-2) | live | none | Name on the network. Default: the host name (<host>-<id> with several devices). The 31-byte limit is the wire size of the name as far as known; PROVISIONAL until a capture confirms it (OQ-CFG-2). |
| `rx_channels` | integer | `2` | channels | 0..=65535 (not both 0) | live | device | Receive channels. Changing it restarts the device. |
| `tx_channels` | integer | `2` | channels | 0..=65535 (not both 0) | live | device | Transmit channels. Changing it restarts the device. |
| `sample_rate` | integer | `48000` | Hz | 44100, 48000, 88200, 96000, 176400, 192000 | live | device | Sample rate. Changing it restarts the device. |
| `encoding` | integer | `24` | bits | 16, 24, 32 | live | device | Bits per sample on the wire. |
| `latency_us` | integer | host (e.g. `4000`) | us | within latency_bounds_us | live | device | Receive latency. Default: 4000 on x86, 10000 on ARM. |
| `latency_bounds_us` | array | host (e.g. `[1000, 40000]`) | us | [min, max] | reload | none | Range a run-time latency change may use. |
| `allow_unsafe_latency` | boolean | `false` |  | true\|false | reload | none | Accept a latency below this platform's timing floor (with a warning). |
| `locked` | array | `[]` |  | name, sample_rate, encoding, latency, rx_channel_labels, tx_channel_labels, rx_channels, tx_channels | reload | none | Values that run-time commands may not change; the configured value always wins. |
| `rx_channel_labels` | table | `{}` |  | { <channel> = "label" } | live | none | Receive channel labels (default RX n). |
| `tx_channel_labels` | table | `{}` |  | { <channel> = "label" } | live | none | Transmit channel labels (default TX n). |
| `multicast.allow_tx_multicast` | boolean | `true` |  | true\|false | reload | none | May this device transmit multicast flows. |

#### `[[bridge]]`

An in-daemon ALSA hardware-card bridge: moves audio between a card and a contiguous channel range of a device, compensating the card's clock drift.

| Key | Type | Default | Unit | Range | Apply | Restart scope | What and why |
|---|---|---|---|---|---|---|---|
| `device` | string | required (e.g. `"main"`) |  | a device id | reload | device | The device this bridge belongs to. Required. |
| `alsa_pcm` | string | required (e.g. `"hw:CARD=<card>,DEV=0"`) |  | ALSA PCM name | reload | device | The card. Required. |
| `direction` | string | `"playback"` |  | playback\|capture | reload | device | playback: network to card; capture: card to network. |
| `name` | string | unset (e.g. `"main-playback"`) |  | letters, digits, _ and - | reload | device | Unique name. Unset: <device>-<direction>. |
| `enabled` | boolean | `true` |  | true\|false | reload | device | false keeps the entry but leaves the card alone. |
| `first_channel` | integer | `1` |  | >= 1 | reload | device | First device channel carried; card channel 1 is this one. |
| `channels` | integer | unset (e.g. `2`) | channels | >= 1 | reload | device | Channels carried. Unset: the rest of the device's channels from first_channel on. |
| `sample_rate` | integer | unset (e.g. `48000`) | Hz | the device's rate | reload | device | The card's rate. Unset: follow the device. |
| `period_frames` | integer | unset (e.g. `480`) | frames | >= 1 | reload | device | Frames per card period. Unset: 10 ms at the device's rate. |
| `buffer_frames` | integer | unset (e.g. `1920`) | frames | >= 2 periods | reload | device | Card buffer. Unset: four periods. |
| `resampler` | string | `"auto"` |  | off\|linear\|cubic\|sinc-fast\|sinc-hq\|auto | reload | device | Resampler quality; auto takes [backends] resampler; off disables drift compensation. |
| `time_constant_s` | float | `120.0` | s | > 0 | reload | device | Time constant of the drift loop. Slow on purpose so it cannot fight a second servo downstream. |
| `max_correction_ppm` | float | `200.0` | ppm | 0..=3000 (the resampler's ratio range, tessera-backend-alsa MAX_CORRECTION) | reload | device | Largest correction the loop may apply. The ceiling is what the bridge's resampler can do (its ratio range, MAX_CORRECTION in tessera-backend-alsa), not a policy. |
| `rt_priority` | integer | `75` |  | 0..=99 (0: normal scheduling) | reload | device | SCHED_FIFO priority of the bridge thread. |
| `cpu` | integer | unset (e.g. `2`) |  | a CPU number | reload | device | CPU to pin the bridge thread to. Unset: not pinned. |
| `error_backoff` | boolean | unset (e.g. `true`) |  | true\|false | reload | none | Override of [defaults.bridge] error_backoff for this bridge. |
| `error_backoff_min_ms` | integer | unset (e.g. `500`) | ms | 10..=error_backoff_max_ms | reload | none | Override of [defaults.bridge] error_backoff_min_ms for this bridge. |
| `error_backoff_max_ms` | integer | unset (e.g. `30000`) | ms | error_backoff_min_ms..=600000 | reload | none | Override of [defaults.bridge] error_backoff_max_ms for this bridge. |
| `error_sleep` | boolean | unset (e.g. `true`) |  | true\|false | reload | none | Override of [defaults.bridge] error_sleep for this bridge. |
| `error_sleep_min_ms` | integer | unset (e.g. `2`) | ms | 1..=error_sleep_max_ms | reload | none | Override of [defaults.bridge] error_sleep_min_ms for this bridge. |
| `error_sleep_max_ms` | integer | unset (e.g. `64`) | ms | error_sleep_min_ms..=1000 | reload | none | Override of [defaults.bridge] error_sleep_max_ms for this bridge. |

#### `[[flow]]`

PROVISIONAL static audio flow between two tessera instances (test scaffolding until flow control exists).

| Key | Type | Default | Unit | Range | Apply | Restart scope | What and why |
|---|---|---|---|---|---|---|---|
| `id` | string | required (e.g. `"rx-a"`) |  | unique name | reload | device | Name of the flow (local, never on the wire). Required. |
| `direction` | string | required (e.g. `"rx"`) |  | rx\|tx | reload | device | rx: network to device; tx: device to network. Required. |
| `device` | string | required (e.g. `"main"`) |  | a device id | reload | device | The device the flow belongs to. Required. |
| `port` | integer | required (e.g. `4321`) |  | 1..=65535 | reload | device | tx: destination UDP port; rx: local UDP port. Required. |
| `channels` | array | `[]` |  | device channels | reload | device | Device channels carried, in wire order. Empty: every channel in that direction. |
| `peer` | string | unset (e.g. `"192.0.2.10"`) |  | IPv4 address | reload | device | tx: destination (required); rx: accept packets only from this address. |
| `interface` | string | unset (e.g. `"enp3s0"`) |  | interface name | reload | device | Interface to use. Unset: the primary. |
| `local_address` | string | unset (e.g. `"192.0.2.20"`) |  | IPv4 address | reload | device | Local address to bind. Unset: the interface's first address. |
| `format` | string | `"reference"` |  | reference | reload | device | Packet format (provisional). |
| `frames_per_packet` | integer | unset (e.g. `32`) | frames | >= 1 | reload | device | tx: frames per packet. Unset: a packet of about 667 us, rounded to frames and reduced to fit the MTU. |
| `send_offset_us` | integer | `0` | us | >= 0 | reload | device | tx: extra delay before a packet is sent. |
| `any_source` | boolean | `false` |  | true\|false | reload | device | rx: accept datagrams from any source (off: only from peer, so others cannot inject audio). |
| `dscp` | integer | `46` |  | 0..=63 | reload | device | tx: DSCP of the media packets (46, EF). |
| `max_lag_us` | integer | unset (e.g. `2000`) | us | >= 1 | reload | device | tx: how late a packet may still be sent before the flow skips forward. Unset: half the latency. |
| `rx_keepalive_ms` | integer | unset (e.g. `250`) | ms | >= 0 | reload | device | rx: override of [defaults.flow] rx_keepalive_ms. |
| `rx_keepalive_timeout_ms` | integer | unset (e.g. `10000`) | ms | >= 0 | reload | device | rx: override of [defaults.flow] rx_keepalive_timeout_ms. |
| `rx_keepalive_payload` | integer or string | unset (e.g. `"empty"`) |  | "empty" or "1337" (strings; the integer 1337 is accepted too) | reload | device | rx: override of [defaults.flow] rx_keepalive_payload. |

<!-- schema:end -->
