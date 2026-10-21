# Command-line reference

The programs of tessera and every option they take. The help texts in the
fenced blocks were captured from `--help` of a build of the current `main`:
`tesserad`, `tesseractl` and each of its subcommands (for `clock`, `network`
and `config`, the group's own help), and `tessera-web`. The subcommand blocks
are **abridged**: they keep the description, the `Usage` line, the arguments
and the options of that subcommand, verbatim, and leave out the six global
options, which every subcommand accepts and which are listed once under
[Global options](#global-options). Not captured, and described from the source
instead (marked where they appear): the commands inside `clock`, `network` and
`config`, the `tessera-web` subcommands, and `tessera-pw`.

| Program | Package | Purpose |
|---------|---------|---------|
| [`tesseractl`](#tesseractl) | `tessera` | control and inspect a running daemon; validate configuration files |
| [`tesserad`](#tesserad) | `tessera` | the daemon (normally started by `tesserad.service`) |
| [`tessera-web`](#tessera-web) | `tessera-web` | the web portal; sets its password |
| [`tessera-pw`](#tessera-pw) | `tessera-pw` | the PipeWire client (started by the user unit) |

Man pages with the same text are installed by native package builds
(`man tesseractl`, `man tesserad`, `man tessera-web`, `man tessera-pw`).

---

## tesseractl

`tesseractl` talks to the daemon over its control socket (default
`/run/tessera/control.sock`). Reading needs membership of group `tessera`;
changing settings needs root or the admin group (`tessera-admin` by default)
**as well** as `tessera` (the socket is mode 0660, group `tessera`, and the
kernel checks that before the daemon sees the request). `config check` and
`config show` (without `--effective`) read the files directly and need no
daemon. `completions` needs neither.

```text
Control the tessera daemon (tesserad)

Usage: tesseractl [OPTIONS] <COMMAND>

Commands:
  status        Show devices, subscriptions, clocks and warnings
  metrics       Show the daemon's counters and gauges
  bridges       Show the in-daemon hardware-card bridges (`[[bridge]]`)
  watch         Stream state-change events until interrupted
  set-name      Rename the device
  set-rate      Set the sample rate
  set-encoding  Set the wire encoding
  set-latency   Set the latency
  set-channels  Set the receive and transmit channel counts (restarts the device)
  set-label     Set or reset a channel label
  subscribe     Subscribe a receive channel to a transmit channel
  unsubscribe   Remove the subscription on a receive channel
  clock         Clock status and settings
  network       Network interfaces and the primary interface
  config        Configuration
  completions   Print a shell completion script
  help          Print this message or the help of the given subcommand(s)

Options:
  -s, --socket <SOCKET>
          Control socket of the daemon [env: TESSERA_SOCKET=] [default: /run/tessera/control.sock]
      --json
          Machine-readable JSON output
  -d, --device <DEVICE>
          Logical device key; optional when the daemon has exactly one device [env: TESSERA_DEVICE=]
      --timeout <TIMEOUT>
          Seconds to wait for the daemon to answer (0 = forever) [default: 10]
      --max-response-bytes <MAX_RESPONSE_BYTES>
          Largest response frame accepted from the daemon, in bytes [default: 8388608]
  -y, --yes
          Apply a change that restarts something without asking
  -h, --help
          Print help
  -V, --version
          Print version

Exit codes: 0 ok, 1 error (also: a change that restarts something was not confirmed), 2 usage, 3 cannot connect, 4 permission denied.
Changing settings needs root or membership of the daemon's admin group (tessera-admin by default); reading status does not.
```

### Global options

Every subcommand accepts these, before or after the subcommand name, and each
subcommand's `--help` repeats them (with `-h, --help`).

| Option | Environment | Default | Meaning |
|--------|-------------|---------|---------|
| `-s`, `--socket PATH` | `TESSERA_SOCKET` | `/run/tessera/control.sock` | control socket |
| `--json` | | off | machine-readable output (pretty-printed JSON; `watch` prints one JSON object per line) |
| `-d`, `--device KEY` | `TESSERA_DEVICE` | the only device | logical device key (`[[device]] id`); required when the daemon has several devices and the command changes one |
| `--timeout SECS` | | 10 | how long to wait for an answer; 0 waits forever |
| `--max-response-bytes N` | | 8388608 (8 MiB) | largest answer accepted |
| `-y`, `--yes` | | off | apply a change that restarts something without asking (see [Changing settings](#changing-settings)) |

**Size limits.** Two limits can refuse an answer, with different messages. The
daemon caps every frame it sends or accepts at `[control] max_frame_bytes`
(1 MiB by default; it announces the value in `limits` of the status). An answer
that would be larger is not sent; the connection stays open and the daemon
answers with a typed error, which `tesseractl` prints as `response too large:
N bytes, limit M (config key control.max_frame_bytes)`. Raise the key in the
daemon's configuration to get such an answer. Separately, `--max-response-bytes`
protects the client from a misbehaving daemon: an announced frame above it is
not read, and `tesseractl` prints `response of N bytes exceeds client limit M
(--max-response-bytes)` (followed by a hint naming the daemon's own limit, when
the status was read earlier in the same run). Both exit with 1.

### Exit codes

| Code | Meaning | Example |
|------|---------|---------|
| 0 | ok (also when standard output was closed early, as with `\| head`) | |
| 1 | error: the daemon refused the command, a configuration file is invalid, or a change that restarts something was not confirmed (answered no, or no terminal to ask on and no `--yes`; nothing was changed) | `sample_rate is locked by the configuration` |
| 2 | usage: bad command line, or several devices and no `--device` | `the daemon has several devices (a, b); pick one with --device` |
| 3 | cannot connect to the socket | `cannot connect to /run/tessera/control.sock: No such file or directory` |
| 4 | permission denied: no access to the socket, or a change without admin rights | |

### Reading

#### `status`

```text
Show devices, subscriptions, clocks and warnings

Usage: tesseractl status [OPTIONS]
```

Options: the [global options](#global-options).

Without `--device`: the version, a device table (`DEVICE NAME RATE BITS LATENCY
RX TX LOCKED`), subscriptions, the primary interface and how it was chosen,
the clock settings, a `Clocks` table, a `Bridges` table and warnings (each
section only when it has content). With `--device KEY`: that device in
detail, with its channels and labels.

```sh
tesseractl status
tesseractl --device main status
tesseractl --json status | jq '.devices[].name'
```

#### `metrics`

```text
Show the daemon's counters and gauges

Usage: tesseractl metrics [OPTIONS]
```

Options: the [global options](#global-options).

A `COUNTER VALUE` table of the daemon (`daemon.*`), the audio socket
(`audio.*`) and the media engine (`engine.*`, including the wake-up lateness
histogram `engine.wake_lateness_us{<25}` to `{>=4000}`), then tables for the
static flows (`FLOW DEVICE DIR PACKETS FRAMES LATE GAPS HOLES UNDERRUN SKIPS
REFUSED MAX LATENESS`), bridges and clocks. The JSON has more than the table:
per-flow histograms and the receiver keepalive counters `keepalives_sent` and
`keepalive_failures` are only there.

```sh
tesseractl metrics
tesseractl --json metrics | jq '.flows[] | {id, rx}'
```

#### `bridges`

```text
Show the in-daemon hardware-card bridges (`[[bridge]]`)

Usage: tesseractl bridges [OPTIONS]
```

Options: the [global options](#global-options).

One row per `[[bridge]]`: `BRIDGE DEVICE DIR CHANNELS CARD STATE FILL POS ERR
CORR PPM TREND PPM RATIO XRUNS SLIPS SHORT REOPENS`, then the last error of
each bridge that has one. `No bridges.` when there are none. The state is
`starting`, `running`, `reopening`, `failed` or `disabled`, followed by
`(no rt)` when real-time scheduling was refused. The same rows appear in
`status` and `metrics`; the JSON has every field of `BridgeStatus`
([control-interface.md](control-interface.md#bridges)).

#### `watch`

```text
Stream state-change events until interrupted

Usage: tesseractl watch [OPTIONS]

Options:
      --count <COUNT>
          Stop after this many events
      --for <SECONDS>
          Stop after this many seconds
```

Plus the [global options](#global-options).

Prints one line per state change, made by any client, until `--count` events
were shown, `--for` seconds passed, or Ctrl-C. When the daemon reports that this
subscriber fell behind, the human view prints `-- N events were dropped;
current state follows --` and the current status; the JSON view prints the
`lagged` reply.

```sh
tesseractl watch
tesseractl --json watch --count 1
# {"event":{"device_renamed":{"device":"main","name":"studio-c","origin":{"cli":{"uid":1000}}}}}
```

### Changing settings

A change that restarts something (a new rate, encoding, latency, channel
counts, a new primary interface, a configuration reload that restarts a
device) is not applied at once. `tesseractl` shows what restarts and is
dropped (the daemon's description of the impact, on standard error) and asks
`Apply this change? [y/N]` on a terminal; only `y` or `yes` goes on. Without a
terminal it refuses with exit code 1 and changes nothing; `--yes` (`-y`)
applies without asking, for scripts. The confirmed request carries the digest
of the impact that was shown: if the situation changed meanwhile (for example
a subscription appeared), the daemon applies nothing, and `tesseractl` shows
the new impact and asks again.

All of these need admin rights. A refused change exits with 1 (4 for
`permission_denied`) and prints the daemon's reason (`invalid latency: 50 us is outside the allowed range [1000, 40000]`,
`name is locked by the configuration`, ...). With `--json` a success prints
`{"ok":true}`. Changes are stored in `/var/lib/tessera/state.toml` and survive
a restart. Every command below also takes the [global options](#global-options).

#### `set-name`

```text
Rename the device

Usage: tesseractl set-name [OPTIONS] <NAME>

Arguments:
  <NAME>  The new name (at most 31 bytes)
```

#### `set-rate`

```text
Set the sample rate

Usage: tesseractl set-rate [OPTIONS] <HZ>

Arguments:
  <HZ>  Rate in Hz
```

Supported rates: 44100, 48000, 88200, 96000, 176400, 192000. Refused while an
enabled `[[bridge]]` of the device pins another `sample_rate`. A rate change
rebuilds the device's flows and detaches its audio clients, which re-attach.

#### `set-encoding`

```text
Set the wire encoding

Usage: tesseractl set-encoding [OPTIONS] <BITS>

Arguments:
  <BITS>  Bits per sample (16, 24 or 32)
```

#### `set-latency`

```text
Set the latency

Usage: tesseractl set-latency [OPTIONS] <MICROS>

Arguments:
  <MICROS>  Latency in microseconds
```

The value must lie within the device's `latency_bounds_us` (default
`[1000, 40000]`) and must not be below the platform floor (2000 us on ARM,
1000 us elsewhere). `allow_unsafe_latency = true` on the device lifts only
the floor, not the bounds.

#### `set-channels`

```text
Set the receive and transmit channel counts (restarts the device)

Usage: tesseractl set-channels [OPTIONS] --rx <RX> --tx <TX>

Options:
      --rx <RX>
          Number of receive channels
      --tx <TX>
          Number of transmit channels
      --dry-run
          Only show what the change would do; change nothing
```

Sets the device's channel counts at run time; the values are kept in
`state.toml`. Not both counts may be 0; a count that is `0` now may stay `0`,
but a direction that has channels cannot be taken to `0` at run time. The device
restarts: its flows are rebuilt, its audio clients are detached and must
re-attach, it is announced again, and subscriptions on RX channels that no
longer exist are deleted. Refused while `rx_channels` or `tx_channels` is in the
device's `locked` list, and when the audio rings would not fit the memory
budget (`[audio] ring_budget_mib`).

Without `--yes` it first asks the daemon for a dry run and prints the impact:
the flows that restart, the subscriptions that will be deleted, the audio
clients that will be detached, and the ring memory before and after; then it
asks `Apply this change? [y/N]` and applies with the digest of that impact.
`--dry-run` prints the impact and exits 0 without changing anything (with
`--json`: `{"dry_run":true,"impact":{...}}`; after a change, `{"ok":true,"impact":{...}}`).
`--dry-run` and `--yes` exclude each other; `--yes` skips the dry run and the
question. A count equal to the current one changes nothing and asks nothing.

```sh
tesseractl --device main set-channels --rx 16 --tx 8 --dry-run
tesseractl --device main set-channels --rx 16 --tx 8          # asks
tesseractl --device main set-channels --rx 16 --tx 8 --yes    # for scripts
```

#### `set-label`

```text
Set or reset a channel label

Usage: tesseractl set-label [OPTIONS] <DIRECTION> <CHANNEL> [LABEL]

Arguments:
  <DIRECTION>
          Receive or transmit channel

          Possible values:
          - rx: Receive channels (from the network)
          - tx: Transmit channels (to the network)

  <CHANNEL>
          Channel number, from 1

  [LABEL]
          The new label

Options:
      --reset
          Restore the default label
```

```sh
tesseractl set-label rx 3 "Room mic"
tesseractl set-label rx 3 --reset
```

#### `subscribe`, `unsubscribe`

```text
Subscribe a receive channel to a transmit channel

Usage: tesseractl subscribe [OPTIONS] <RX_CHANNEL> <TX_CHANNEL> <TX_DEVICE>

Arguments:
  <RX_CHANNEL>  Local receive channel, from 1
  <TX_CHANNEL>  Name of the transmit channel on the remote device
  <TX_DEVICE>   Name of the remote device
```

```text
Remove the subscription on a receive channel

Usage: tesseractl unsubscribe [OPTIONS] <RX_CHANNEL>

Arguments:
  <RX_CHANNEL>  Local receive channel, from 1
```

> `tesserad` does not implement the vendor control plane, so vendor
> controllers cannot route to it on their own; see
> [interoperability.md](interoperability.md).

A subscription is stored by name, exactly as given (names are treated as
case-sensitive, provisional; the daemon does not resolve them yet), and kept
even while the transmitter is not there. **The current daemon does not
implement flow control**, so a subscription carries no audio by itself; audio
between devices is configured with `[[flow]]`
([configuration.md](configuration.md#static-flows-provisional)). The subscription
shows `in progress` until a rx flow carries its channel, then `receiving` while
packets arrive and `no media` when they stop (shown about 2 to 3 s after the last
packet; for a flow that never received one, 5 s after it started). That state is read from the flow,
so it does not prove that the media comes from the transmitter named in the subscription.

```sh
tesseractl subscribe 2 "Out 2" stagebox-a
tesseractl unsubscribe 2
```

### Clock and network

#### `clock`

```text
Clock status and settings

Usage: tesseractl clock [OPTIONS] <COMMAND>

Commands:
  show          Show the clock table
  set-role      Set the clock role
  set-priority  Set the leader-election priority (lower wins)
  help          Print this message or the help of the given subcommand(s)
```

Plus the [global options](#global-options). The commands inside `clock` were
not captured; from the source (`crates/tesseractl/src/cli.rs`):

| Command | Arguments | Effect |
|---------|-----------|--------|
| `clock show` | none | `Clock settings: role R, priority P (locked: ...)`, then the table `INTERFACE STATE TIMESTAMPS OFFSET RATE GRANDMASTER EPOCH`, or `No clocks.`; with `--json` an object `{"settings": ..., "clocks": [...]}` |
| `clock set-role ROLE` | `follower`, `leader` or `auto` (leader-capable, follower-preferred) | applied live: the election re-runs, media time is not stepped |
| `clock set-priority N` | 0 to 255, lower wins | applied live to a leader-capable role; the mapping onto PTPv1 fields is provisional ([configuration.md](configuration.md#engine-clock-and-discovery-at-run-time)) |

The clock state is one of `free_running`, `listening`, `acquiring`, `locked`,
`holdover`, `leader` in the JSON; the table prints `free-running` for the
first.

```sh
tesseractl clock show
tesseractl clock set-role auto
tesseractl clock set-priority 20
```

#### `network`

```text
Network interfaces and the primary interface

Usage: tesseractl network [OPTIONS] <COMMAND>

Commands:
  show     Show the primary interface, how it was chosen and the candidates
  primary  Select the primary interface; kept across restarts
  help     Print this message or the help of the given subcommand(s)
```

Plus the [global options](#global-options). The commands inside `network`
were not captured; from the source:

| Command | Arguments | Effect |
|---------|-----------|--------|
| `network show` | none | `Primary interface: NAME (how)`, the stored selection, whether it is locked, and the table `INTERFACE MAC IPV4 LINK TYPE USABLE` (the primary marked `*`); with `--json` the `NetworkStatus` object |
| `network primary NAME` | an interface name, or `default` (also `auto`) to clear the selection | selects the primary interface at run time and keeps it in `state.toml`; refused with `[network] locked = ["primary"]`; restarts the primary PTP port (the clock epoch changes) |

"How" is one of `locked by configuration`, `selected`, `configured`,
`automatic` or `fallback` (a selected interface that cannot be used now; the
reason is printed).

```sh
tesseractl network show
tesseractl network primary enp4s0
tesseractl network primary default
```

### Configuration and completions

#### `config`

```text
Configuration

Usage: tesseractl config [OPTIONS] <COMMAND>

Commands:
  reload  Ask the daemon to re-read its configuration files
  show    Show configuration
  check   Validate the configuration files without touching the daemon
  help    Print this message or the help of the given subcommand(s)
```

Plus the [global options](#global-options). The commands inside `config` were
not captured; from the source:

| Command | Options | Effect |
|---------|---------|--------|
| `config reload` | none | asks the daemon to re-read its files, like `systemctl reload tesserad`; a configuration that does not validate is refused and the old one keeps running |
| `config show` | `--effective`, `--provenance`, `--config-dir DIR` | default: the merged files of `DIR` (default `/etc/tessera`, environment `TESSERA_CONFIG_DIR`) as TOML with every key explicit; `--provenance` adds which file set each key; `--effective` asks the daemon what it uses (files plus values stored at run time) and cannot be combined with `--provenance` |
| `config check` | `--config-dir DIR` | validates the files without a daemon: `configuration OK (N file(s), M warning(s))`, or every error with its key path and file (exit 1); with `--json` `{"ok":true,"files":N,"warnings":M}` |

`config check` accepts `[[bridge]]` entries even though a daemon built without
`alsa-bridge` refuses them at reload; only the daemon knows how it was built.

```sh
tesseractl config check
tesseractl config check --config-dir ./test-config
tesseractl config show --provenance
tesseractl config show --effective
```

#### `completions`

```text
Print a shell completion script

Usage: tesseractl completions [OPTIONS] <SHELL>

Arguments:
  <SHELL>  The shell [possible values: bash, elvish, fish, powershell, zsh]
```

```sh
tesseractl completions bash | sudo tee /usr/share/bash-completion/completions/tesseractl >/dev/null
```

Native packages install the bash, zsh and fish completions.

---

## tesserad

The daemon. Under systemd it is started as `/usr/bin/tesserad` with no
arguments. **Every setting is a key of the configuration files**
(`/etc/tessera/config.toml`, `config.d/*.toml`; [configuration.md](configuration.md));
`tesserad --print-default-config` prints them all, commented out at their
defaults, with what each does and why. Each option below overrides one key
(the table in [configuration.md](configuration.md#tesserad-command-line)):
a value given on the command line wins over every file, is logged at start,
and a reload (`SIGHUP`) does not change it. Prefer a drop-in in
`/etc/tessera/config.d/` to an `ExecStart=` override: it can be reloaded.
Only `--config-dir`, `--state-dir`, `--runtime-dir` and the action
`--print-default-config` have no key.

```text
tessera audio-over-IP daemon.

Every setting lives in the configuration files (/etc/tessera/config.toml and config.d/*.toml; `tesserad --print-default-config` prints every key with its default). An option given here overrides its configuration key, and a reload (SIGHUP) does not change it; an option that is not given leaves the key to the files.

Usage: tesserad [OPTIONS]

Options:
      --config-dir <CONFIG_DIR>
          Directory holding `config.toml` and `config.d/`

          [env: TESSERA_CONFIG_DIR=]
          [default: /etc/tessera]

      --state-dir <STATE_DIR>
          Directory for `state.toml`, for this process only. Default: `[daemon] state_dir` of the configuration (`/var/lib/tessera`)

          [env: TESSERA_STATE_DIR=]

      --runtime-dir <RUNTIME_DIR>
          Directory for `control.sock` and `audio.sock`, for this process only. Default: `[daemon] runtime_dir` of the configuration (`/run/tessera`)

          [env: TESSERA_RUNTIME_DIR=]

      --print-default-config
          Print the reference configuration (every key at its default, with what it does) and exit

      --socket-group <SOCKET_GROUP>
          Group (name or number) that owns the sockets. Overrides `[control] socket_group` (default: `tessera` if that group exists, else the daemon's own group with a warning; a group given here or in the file that cannot be applied is an error)

      --socket-mode <SOCKET_MODE>
          Permission bits of the sockets, octal. Overrides `[control] socket_mode` (default 0o660)

      --audio-socket <AUDIO_SOCKET>
          Listen on `audio.sock` for audio clients. Overrides `[audio] socket` (default auto)

          Possible values:
          - auto: Listen if the configuration enables at least one backend client
          - on:   Always listen
          - off:  Never listen

      --max-frame-bytes <MAX_FRAME_BYTES>
          Largest control request or response frame, bytes. Overrides `[control] max_frame_bytes` (default 1048576)

      --max-connections <MAX_CONNECTIONS>
          Most simultaneous control connections (all peers). Overrides `[control] max_connections` (default 256)

      --max-connections-per-uid <MAX_CONNECTIONS_PER_UID>
          Most control connections one uid may hold. Overrides `[control] max_connections_per_uid` (default 16)

      --admin-reserved-connections <ADMIN_RESERVED_CONNECTIONS>
          Control connection slots only administrators may use, so that unprivileged group members cannot lock them out. Overrides `[control] admin_reserved_connections` (default 16)

      --idle-timeout-secs <IDLE_TIMEOUT_SECS>
          Seconds a request/response connection may stay silent before it is closed (event subscribers are exempt). Overrides `[control] idle_timeout_secs` (default 60)

      --event-queue <EVENT_QUEUE>
          Events queued per subscriber before the oldest are dropped. Overrides `[control] event_queue` (default 256)

      --control-threads <CONTROL_THREADS>
          Worker threads of the control-plane runtime. Overrides `[control] threads` (default 2)

      --rate-burst <RATE_BURST>
          Requests a peer may send back to back. Overrides `[control] rate_burst` (default 256)

      --rate-per-second <RATE_PER_SECOND>
          Requests per second a peer may sustain. Overrides `[control] rate_per_second` (default 128)

      --frame-timeout-secs <FRAME_TIMEOUT_SECS>
          Seconds a peer may take to finish a frame, counted from its first byte (a deadline: trickling bytes does not extend it). Overrides `[control] frame_timeout_secs` (default 10)

      --write-timeout-secs <WRITE_TIMEOUT_SECS>
          Seconds to wait for a peer that does not read its responses. Overrides `[control] write_timeout_secs` (default 5)

      --stream-write-timeout-secs <STREAM_WRITE_TIMEOUT_SECS>
          Seconds to wait for a stalled event subscriber before dropping it. Overrides `[control] stream_write_timeout_secs` (default 30)

      --persist-delay-ms <PERSIST_DELAY_MS>
          Debounce window for writing `state.toml`, milliseconds. Overrides `[daemon] persist_delay_ms` (default 500)

      --max-audio-clients <MAX_AUDIO_CLIENTS>
          Most simultaneous audio client connections. Overrides `[audio] max_clients` (default 64)

      --max-audio-clients-per-uid <MAX_AUDIO_CLIENTS_PER_UID>
          Most audio client connections one uid may hold. Overrides `[audio] max_clients_per_uid` (default 8)

      --audio-max-period-frames <AUDIO_MAX_PERIOD_FRAMES>
          Largest `max_period_frames` an audio client may ask for. Overrides `[audio] max_period_frames` (default 16384)

      --audio-ring-budget-mib <AUDIO_RING_BUDGET_MIB>
          Total ring memory all audio sessions together may reserve, MiB. Overrides `[audio] ring_budget_mib` (default 256)

      --shutdown-timeout-secs <SHUTDOWN_TIMEOUT_SECS>
          Seconds to allow the final state flush and shutdown; keep the service manager's stop timeout above this. Overrides `[daemon] shutdown_timeout_secs` (default 20)

      --tick-us <TICK_US>
          Engine tick (wake-up period of audio clients and of the receive drain), microseconds. Overrides `[engine] tick_us` (default auto, which is 1000)

      --rx-batch <RX_BATCH>
          Datagrams per `recvmmsg` call. Overrides `[engine] rx_batch` (default 32)

      --rx-max-batches <RX_MAX_BATCHES>
          Receive batches per socket per tick (bounds the work of one tick). Overrides `[engine] rx_max_batches` (default 8)

      --tx-batch <TX_BATCH>
          Datagrams per `sendmmsg` call. Overrides `[engine] tx_batch` (default 32)

      --tx-max-per-tick <TX_MAX_PER_TICK>
          Packets per transmit flow per tick (bounds the work of one tick). Overrides `[engine] tx_max_per_tick` (default 32)

      --socket-buffer-kib <SOCKET_BUFFER_KIB>
          Kernel socket buffer of media sockets, KiB. Overrides `[engine] socket_buffer_kib` (default 2048)

      --rx-margin-us <RX_MARGIN_US>
          Margin added to the receive latency published to audio clients on top of one engine tick (wake-up jitter allowance), microseconds. Overrides `[engine] rx_margin_us` (default 250)

      --rx-keepalive-ms <RX_KEEPALIVE_MS>
          Interval of the receiver keepalives every rx flow sends to the source of its media (a unicast transmitter stops without them), milliseconds; 0 disables them. PROVISIONAL. Overrides `[defaults.flow] rx_keepalive_ms` (default 250)

      --rx-keepalive-timeout-ms <RX_KEEPALIVE_TIMEOUT_MS>
          Stop an rx flow's keepalives after this long without valid media, milliseconds (they resume with the next packet); 0: never stop while the flow exists. Overrides `[defaults.flow] rx_keepalive_timeout_ms` (default 10000)

      --rx-keepalive-payload <RX_KEEPALIVE_PAYLOAD>
          What a receiver keepalive carries: `empty` (a zero-length datagram, as hardware receivers send) or `1337` (the bytes `13 37`). PROVISIONAL. Overrides `[defaults.flow] rx_keepalive_payload` (default empty)

          Possible values:
          - empty: A zero-length datagram
          - 1337:  The two bytes `13 37`

      --no-rt
          Never request real-time scheduling or lock memory, whatever `[engine]` says (disables real-time setup). Sets `[daemon] realtime` to false (default true)

      --no-ptp
          Do not run PTP: the media clock stays free-running. Sets `[clock] enabled` to false (default true)

      --no-discovery
          Do not advertise devices with mDNS, whatever the rest of `[discovery]` says. Sets `[discovery] enabled` to false (default true)

      --audio-max-ring-mib <AUDIO_MAX_RING_MIB>
          Largest single ring an audio client may request, MiB. Overrides `[audio] max_ring_mib` (default 64)

      --audio-reorder-window-us <AUDIO_REORDER_WINDOW_US>
          Reorder window added to audio ring capacity, microseconds. Overrides `[audio] reorder_window_us` (default 10000)

      --audio-default-period-frames <AUDIO_DEFAULT_PERIOD_FRAMES>
          Client period assumed when an audio client does not state one, frames. Overrides `[audio] default_period_frames` (default 1024)

      --log-level <LOG_LEVEL>
          Log filter (`error|warn|info|debug|trace` or `target=level,...`). Overrides `[daemon] log_level` (default info); the `RUST_LOG` environment variable overrides both

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

### Options by topic

| Topic | Options | Keys |
|-------|---------|------|
| Files | `--config-dir`, `--state-dir`, `--runtime-dir` (command line only), `--print-default-config` | |
| Sockets (read at start) | `--socket-group`, `--socket-mode`, `--audio-socket` | `[control] socket_group`, `socket_mode`, `[audio] socket` |
| Control socket limits | `--max-frame-bytes`, `--max-connections`, `--max-connections-per-uid`, `--admin-reserved-connections`, `--idle-timeout-secs`, `--event-queue`, `--rate-burst`, `--rate-per-second`, `--frame-timeout-secs`, `--write-timeout-secs`, `--stream-write-timeout-secs` | `[control]` |
| Audio socket limits | `--max-audio-clients`, `--max-audio-clients-per-uid`, `--audio-max-period-frames`, `--audio-ring-budget-mib`, `--audio-max-ring-mib`, `--audio-reorder-window-us`, `--audio-default-period-frames` | `[audio]` |
| Media engine | `--tick-us`, `--rx-batch`, `--rx-max-batches`, `--tx-batch`, `--tx-max-per-tick`, `--socket-buffer-kib`, `--rx-margin-us` | `[engine]` |
| Receiver keepalives (PROVISIONAL; reload: at the next flow (re)start) | `--rx-keepalive-ms`, `--rx-keepalive-timeout-ms`, `--rx-keepalive-payload` | `[defaults.flow]` (and per `[[flow]]`) |
| Switches (read at start) | `--no-rt`, `--no-ptp`, `--no-discovery` | `[daemon] realtime`, `[clock] enabled`, `[discovery] enabled` |
| Other | `--control-threads` (start), `--persist-delay-ms`, `--shutdown-timeout-secs`, `--log-level` (reload) | `[control] threads`, `[daemon]` |

The rate limits (`--rate-burst`, `--rate-per-second`) are kept per uid: "a
peer" in the help text means a uid, and all connections of one uid share one
bucket ([control-interface.md](control-interface.md#permissions-and-limits)).

### Receiver keepalives

A unicast transmitter may stop a flow whose receiver stays silent.
Every rx flow therefore sends a keepalive from its own socket (its local
address and `port`) to the address and port its media comes from. The target
is learned from the most recent datagram that decodes as media, and only
from that; datagrams dropped by the `peer` filter or not decodable never
redirect it.

| Key (`[defaults.flow]`, or per `[[flow]]`) | Option | Default | Meaning |
|--------|--------|---------|---------|
| `rx_keepalive_ms` | `--rx-keepalive-ms` | 250 | interval; `0` disables keepalives. The first one goes out on the first tick after the first media datagram; none before |
| `rx_keepalive_timeout_ms` | `--rx-keepalive-timeout-ms` | 10000 | stop after this long without valid media, resume with the next packet; `0` never stops while the flow exists |
| `rx_keepalive_payload` | `--rx-keepalive-payload` | `empty` | `empty`: a zero-length datagram, as hardware receivers send; `1337`: the two bytes `13 37` (alternative form, provisional) |

A changed value applies when a flow next (re)starts; it does not rebuild a running flow.

Counters per rx flow: `keepalives_sent`, `keepalive_failures`
(`tesseractl --json metrics`). The values are provisional.

```toml
# /etc/tessera/config.d/50-keepalive.toml
[defaults.flow]
rx_keepalive_payload = "1337"
```

### The sound-card bridge

There is no command-line option for the bridge. It is a **build** option (cargo
feature `alsa-bridge`, off by default; `--help` is the same either way) and is
configured with `[[bridge]]` entries ([configuration.md](configuration.md#hardware-card-bridges-bridge)).
A daemon built without the feature refuses a configuration that has any
`[[bridge]]`, at start and at reload, with an error that names the feature.
`--no-rt` also makes the bridge threads run at normal priority.

### Exit status and signals

| Event | Behaviour |
|-------|-----------|
| `SIGHUP` (`systemctl reload`) | re-read the configuration; a refused reload keeps the old one and reports it in the unit's `Status:` line (`RELOADING=1` / `READY=1` bracket it) |
| `SIGTERM`, `SIGINT` | stop: close the listeners and connections, detach audio clients, flush `state.toml` (bounded by `[daemon] shutdown_timeout_secs`), remove the sockets, exit 0 (1 if the state could not be saved or the flush ran out of time) |
| start-up failure (invalid configuration, socket or lock in use) | message on stderr, `STATUS=failed to start`, exit 1 |
| the state thread dies | exit 1, so the service manager restarts the daemon |
| bad command line | exit 2 (clap) |

Only one daemon may use a runtime directory (`<runtime-dir>/tesserad.lock`).
The service manager's watchdog (`WatchdogSec=10s`) is pinged on a timer; the
ping is withheld once the state thread has been stuck in one operation for
120 s.

---

## tessera-web

```text
The web portal of a tessera device

Usage: tessera-web [OPTIONS] [COMMAND]

Commands:
  serve         Serve the portal (the default)
  set-password  Set the operator password (read from standard input, never from the command line). Run as root
  check         Check the configuration file and exit
  help          Print this message or the help of the given subcommand(s)

Options:
      --config <CONFIG>  Configuration file [default: /etc/tessera/web.toml]
  -h, --help             Print help
  -V, --version          Print version
```

The subcommands' own help was not captured; from the source
(`crates/tessera-web/src/main.rs`; `--config FILE` goes before the subcommand):

| Command | Effect |
|---------|--------|
| `serve` (default) | serve the pages and the API on `listen` from the configuration file (default `127.0.0.1:8090`); warns when listening on a non-loopback address |
| `set-password` | read the operator password from standard input (twice, without echo, when it is a terminal; at least 8 characters) and write its Argon2id hash atomically to `password_file`, mode 0640, group `tessera-web` when that group exists. Run as root. Takes effect at the next login, no restart |
| `check` | parse the configuration file and print `listen ...; control socket ...; password file ...` |

A configuration file that cannot be read or parsed (unknown keys are errors)
makes every command exit with 2. Details: [web-portal.md](web-portal.md).

```sh
sudo tessera-web set-password
sudo tessera-web check
sudoedit /etc/tessera/web.toml        # e.g. uncomment and set: listen = "127.0.0.1:8091"
sudo tessera-web check && sudo systemctl restart tessera-web
```

---

## tessera-pw

`tessera-pw` parses its options by hand; this is its usage text, quoted from
the source (`crates/tessera-backend-pipewire/src/config.rs`, the `USAGE`
constant), not captured from a run:

```text
tessera-pw: PipeWire client of tesserad (interoperable with Dante(R) devices)

USAGE: tessera-pw [OPTIONS]

  --audio-socket PATH        daemon audio socket       [/run/tessera/audio.sock]
  --control-socket PATH      daemon control socket     [/run/tessera/control.sock]
  --device KEY               only expose this device (repeatable)
  --static-device K:RX:TX[:HZ] expose a device without the control socket (repeatable)
  --node-prefix STR          PipeWire node name prefix [tessera]
  --period FRAMES            requested graph period    [256]
  --max-period FRAMES        largest supported period  [2048]
  --max-node-channels N      channels per node (1..=64)[64]
  --max-device-channels N    reject devices with more   [4096]
  --positions auto|aux       channel position style    [auto]
  --priority-driver N        priority.driver           [500]
  --priority-session N       priority.session          [100]
  --rt-priority N            SCHED_FIFO priority of the control loop,
                             0 = normal priority        [20]
  --no-mlock                 do not mlockall at start-up
  --rx-offset FRAMES         extra RX read offset       [0]
  --tx-ahead-periods N       TX lead in periods         [1]
  --tx-ahead-margin FRAMES   extra TX lead              [64]
  --resampler Q              off|linear|cubic|sinc-fast|sinc-hq (follower mode;
                             default cubic on ARM, sinc-fast elsewhere)
  --follower-chunk FRAMES    resampler chunk            [64]
  --discover-interval SECS   device list refresh        [10]
  --missing-queries N        queries a device may be missing before its
                             nodes are removed          [3]
  --handshake-timeout MS     per handshake step         [2000]
  --overlay-stale MS         clock overlay silent this long = hung daemon,
                             0 = off                    [2000]
  --stable-after SECS        session age before the back-off resets [10]
  --reconnect-initial MS     first reconnect delay      [200]
  --reconnect-max MS         largest reconnect delay    [5000]
  --log-level LEVEL          error|warn|info|debug|trace[info]
  -h, --help                 this text
```

The unit starts it as `tessera-pw --audio-socket /run/tessera/audio.sock
--control-socket /run/tessera/control.sock`; change options with `systemctl
--user edit tessera-pw.service`. Behaviour: [audio-backends.md](audio-backends.md#pipewire-tessera-pw).
