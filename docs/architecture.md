# Architecture

Tessera separates the audio runtime, local clients, portal and privileged network
configuration. The Rust daemon owns logical devices and the media clock;
applications attach to it through local sockets.

## Processes and ownership

| Component | Identity | Responsibility |
|---|---|---|
| `tesserad` | system service, `tessera` user | Logical devices, PTP, static media flows, runtime state, control/audio sockets and optional hardware bridges |
| `tesseractl` | operator | Typed commands and queries over `control.sock` |
| `tessera-web` | system service, `tessera-web` user | Authenticated pages/API; daemon control client; standalone network-helper client |
| `tessera-network` | separate privileged helper | Supported OS networking, firewall isolation, transaction journal and recovery |
| `tessera-pw` | desktop user service | PipeWire source/sink nodes using the audio and control sockets |
| `pcm.tessera` | inside an ALSA application | Capture/playback using shared-memory audio rings |
| NDIMon Tessera plugin | NDIMon's host/plugin framework | Host settings, routing, lifecycle hooks and an explicitly pinned Tessera runtime |

```text
 tesseractl ──────┐
 tessera-web ─────┼── control.sock ── tesserad state engine ── state.toml
 NDIMon plugin ──┘                         │
                                  media worker / PTP
                                          │
 local audio clients ── audio.sock ── shared rings ── network flows

 tessera-web ── separate helper socket ── tessera-network ── OS networking
```

The device persists independently of its clients. Client disconnects detach
that client's audio session without removing the logical device. NDIMon owns
OS networking on an NDIMon appliance; standalone Tessera networking becomes
read-only when that ownership is present.

## Control, state and configuration

The state engine validates typed commands, computes disruptive-change impact,
applies commands, persists runtime values and publishes events. The CLI and
portal share these semantics. Changes that restart devices can require an impact
token; a stale token cannot confirm a changed operation.

Configuration comes from built-in defaults, `config.toml`, ordered drop-ins,
persisted runtime values and command-line overrides. Bounds and locks determine
which runtime changes are accepted. The daemon owns `state.toml`; operators edit
configuration drop-ins and request reload. See [configuration](configuration.md)
and [control interface](control-interface.md).

## Threads and audio paths

The normal-priority control runtime handles sockets, discovery and supervision.
A separate state thread handles validation/persistence. Media control allocates
flows, sockets and sessions before handing them to the media worker through
bounded queues. The worker processes packets, advances flow engines and wakes
audio clients. PTP has its own runner per configured interface. Optional ALSA
bridges have a manager and a thread per active card path.

The worker and clock/bridge threads can request real-time scheduling, CPU affinity
and locked memory. Setup failures appear in diagnostics. The media path uses
preallocated buffers and bounded work; card-error backoff is configurable.
Privileges come from service limits/capabilities rather than running the audio
daemon as root. See [timing and privileges](rt-tuning.md).

## Clocks, transport and drift

PTPv1 disciplines a virtual media clock derived from monotonic time. It never
sets the host wall clock. Logical devices share the selected primary interface's
clock. Roles include following a leader and leading; holdover preserves a clock
mapping while synchronization recovers. A clock epoch change tells audio clients
to re-anchor instead of replaying stale data.

Static UDP flows explicitly define peer, direction, device/channel mapping and
packet settings. Receive buffering is indexed by media time. Transmit scheduling
uses the configured packet geometry and offsets. Receiver keepalives maintain
already established peer flows; they do not negotiate new vendor subscriptions.
See [compatibility limits](interoperability.md).

Local clients receive shared-memory rings, a read-only clock overlay and a wake
socket through `audio.sock`. The ALSA PCM follows media time. PipeWire can drive
its graph from media time or absorb drift while following another graph driver.
A hardware bridge compensates drift between the sound-card and media clocks;
nominal sample-rate conversion is a separate concern. See
[audio backends](audio-backends.md).

## Standalone networking and recovery

The network helper accepts a typed model of named untagged/tagged networks,
IPv4 configuration and management/audio role assignments. It supports one wired
port with native networkd or Netplan/networkd and refuses unsupported complex
configurations. Only owned files/rules are changed.

Apply journals the previous state before activating a new configuration.
Management changes require an explicit Keep from the new management destination
within 60 seconds after readiness. A device-side timer handles expiry; revert,
failure and boot recovery restore prior state. Browser polling cannot confirm a
transaction. See [VLAN configuration](network-vlans.md).

## Security and diagnostics

Local socket permissions and peer credentials gate reads and administration.
Audio-group membership allows attaching audio endpoints; admin-group membership
adds settings changes. Connection counts, frame sizes, timeouts, event queues
and audio-ring memory are bounded. Network media/control protocols are not an
authentication boundary; isolate trusted media networks.

The portal has sessions, password verification, CSRF/origin checks and bounded
login/session state. TLS exposure is configured by the operator. Network helper
privileges remain separate from the portal and audio daemon.

Status, events, structured metrics and the service journal expose clock state,
packet counters, scheduling failures, bridge xruns/reopens and configuration
errors. See [troubleshooting](troubleshooting.md).

## Source and releases

Runtime crates and packaging live here. NDIMon host plugin source, compatibility
metadata, platform catalogs and archive assembly live in
[NDIMon-Plugins](https://github.com/markfen88/NDIMon-Plugins). Each plugin release
pins a native runtime bundle and checksum. Updating runtime source requires a
new runtime build and plugin release before installed NDIMon devices receive it.
