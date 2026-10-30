# Control interface

How to script tessera or integrate it with other software. There are three
ways in, all backed by the same typed model:

| Way | For | Reference |
|-----|-----|-----------|
| `tesseractl --json ...` | shell scripts on the device | [cli.md](cli.md) |
| the control socket `/run/tessera/control.sock` | programs on the device | this page |
| the portal's JSON API `/api/v1/` | programs on other machines (through an SSH tunnel or a TLS proxy) | [web-portal.md](web-portal.md#json-api) |

Every change, from any of them, is a `Command` of `tessera-control-types`
that the daemon's state engine validates, applies, persists and announces as
an `Event`. Nothing changes daemon state any other way. The
configuration files are the other input: a reload is itself a command
(`reload_config`), and its events carry the origin `config`.

---

## Contents

- [Stability](#stability)
- [The socket](#the-socket)
- [Permissions and limits](#permissions-and-limits)
- [Envelopes](#envelopes)
- [Commands](#commands)
- [Queries and replies](#queries-and-replies)
- [Events](#events)
- [Errors](#errors)
- [Status](#status)
- [Metrics](#metrics)
- [Bridges](#bridges)
- [Examples](#examples)

---

## Stability

`tessera-control-types` (`crates/tessera-control-types`, version 0.5.1) is the
typed public integration contract. Its rules:

- Changes are **additive**. Most enums and structs are `#[non_exhaustive]`
  (not `Direction`, the `Request`/`Response` envelopes or the newtypes such
  as `DeviceKey`); new fields carry `#[serde(default)]`, so an older reply
  still parses. Removing
  or renaming anything is a semver-breaking change.
- Clients must **tolerate unknown fields and unknown enum variants**, because
  new ones appear (bridges, network status, keepalive counters were all added
  this way). The envelope version `v` is `1` (`WIRE_VERSION`).
- The default features are `std` and `serde`; without default features the
  crate builds `no_std` with `alloc` (serde support is the feature `serde`).
- Before the crate reaches 1.0, consult the release notes and [integration contract](integration-contract.json)
  before updating an independently versioned client.

`tesseractl`'s human-readable output is **not** a stable interface; its
`--json` output is the serialised control types, without the socket's reply
wrapper (and a few small objects of its own, listed under
[Examples](#examples)).

---

## The socket

| Property | Value |
|----------|-------|
| Path | `/run/tessera/control.sock` (`<runtime-dir>/control.sock`; `tesserad --runtime-dir`, `[daemon] runtime_dir`) |
| Type | Unix stream socket, mode 0660 (`[control] socket_mode`), group `tessera` (`[control] socket_group`) |
| Framing | each message is a `u32` big-endian payload length, then that many bytes of UTF-8 JSON |
| Largest frame | 1 MiB (`[control] max_frame_bytes`, at least 64 KiB; `Status.limits.max_frame_bytes` shows the value that applies to the connection asking), in both directions; an oversized request header gets one error and the connection is closed; a response that would be larger is replaced by the error `response_too_large` (`size`, `limit`, `key`) for the same request id, and the connection goes on. The frame size and the timeouts are per connection: a connection keeps the values in effect when it was opened, and a reload applies to new connections |
| Connection model | strictly sequential: one request, one response, in order. Many requests per connection are fine |
| Event stream | after `subscribe_events` the connection carries only events until it is closed; a request sent on it is answered with `invalid` (id 0) |

A frame must be completed within `[control] frame_timeout_secs` (10 s) from its first
byte. A request/response connection that is silent for `[control] idle_timeout_secs`
(60 s) is closed; event subscribers are exempt. A frame that is not a valid
request gets an `invalid` error and the connection continues; 32 in a row
close it.

`audio.sock` next to it is a different protocol (binary handshake and shared
memory`tessera-audio-ipc`) for audio clients only.

---

## Permissions and limits

The daemon identifies the peer with `SO_PEERCRED` (uid, gid) and
`SO_PEERGROUPS` (supplementary groups) at connect time.

| Caller | May |
|--------|-----|
| not in group `tessera` | nothing: the kernel refuses `connect` (mode 0660) |
| group `tessera` | every query, `subscribe_events` and dry runs (`set_device_channels` with `dry_run: true`: it changes nothing, writes nothing and publishes no event; its impact also shows the ring memory a change would cost) |
| root, or group `tessera` **and** the admin group (`[daemon] admin_group`, default `tessera-admin`) | also commands, plain or confirmed |

Access is checked per request against the current admin group, so a reload
that changes `admin_group` applies to open connections. A missing admin group
means only root may change things. A command without the rights is answered
with the error `permission_denied`.

| Limit | Default | Key (`tesserad` option that overrides it) |
|-------|---------|--------|
| connections in total | 256 | `[control] max_connections` (`--max-connections`) |
| connections per uid | 16 | `[control] max_connections_per_uid` (`--max-connections-per-uid`) |
| slots only administrators may use | 16 | `[control] admin_reserved_connections` (`--admin-reserved-connections`) |
| requests back to back, per uid | 256 | `[control] rate_burst` (`--rate-burst`) |
| requests per second, per uid | 128 | `[control] rate_per_second` (`--rate-per-second`) |
| events queued per subscriber | 256 (the oldest are dropped) | `[control] event_queue` (`--event-queue`) |
| write to a peer that does not read | 5 s | `[control] write_timeout_secs` (`--write-timeout-secs`) |
| a stalled event subscriber | 30 s, then dropped | `[control] stream_write_timeout_secs` (`--stream-write-timeout-secs`) |

All of them are apply class `reload`. The connection caps and the rate limits are global or per uid and change at once on a `SIGHUP`
(connections already over a lowered cap stay open); the timeouts and the frame size are per connection and apply to connections opened
afterwards.

When the per-uid bucket is empty the daemon answers once with
`rate_limited` (id 0, the frame is not parsed) and stops reading from that
connection until a token is back. The state engine also limits commands per
principal.

---

## Envelopes

Request:

```json
{"v":1,"id":7,"body":{"command":{"subscribe":{"device":"main","rx_channel":1,"tx_channel":"Out 1","tx_device":"stagebox-a"}}}}
```

`body` is one of:

| `body` | Meaning |
|--------|---------|
| `{"command": <Command>}` | change something; a change that restarts a device is refused with `confirmation_required` (below) |
| `{"confirmed_command": {"command": <Command>, "expect": <token or null>}}` | change something the user has confirmed after seeing its impact |
| `{"protocol_command": {"peer": "<controller identity>", "command": <SetProtocolFlows>}}` | trusted local protocol adapter, admin-only; preserves protocol origin and its confirmation exemption |
| `{"query": <Query>}` | read something |
| `"subscribe_events"` | turn this connection into an event stream |

**Confirmation**. A command whose impact restarts a device (`set_sample_rate`, `set_encoding`, `set_latency`,
`set_device_channels`, `set_primary_interface`, a `reload_config` whose edited files restart a device) sent as `command` is answered
`{"err":{"confirmation_required":{"impact":<Impact>}}}` and nothing changes. Show `impact` (what restarts and why, what drops, roughly
how long, the subscriptions deleted, the ring memory, the `restart_keys` waiting for the next service restart) and send the same command
as `confirmed_command` with `"expect": impact.token`. If the impact changed meanwhile the daemon asks again with the new impact;
`"expect": null` confirms whatever the impact is. The answer to a confirmed command is `{"ok":{"impact":<Impact>}}` (what it did); a plain
command that needed no confirmation still answers `"done"`. `set_device_channels` with `"dry_run": true` answers `{"ok":{"impact":...}}`
and changes nothing. Network controllers, `SIGHUP` and the daemon itself are never asked.

Response, success and failure:

```json
{"v":1,"id":7,"result":{"ok":"done"}}
{"v":1,"id":3,"result":{"err":{"locked":{"field":"sample_rate"}}}}
{"v":1,"id":0,"result":{"err":"rate_limited"}}
```

The response carries the request's `id` (0 for the rate-limit answer). Enums
are serialised the serde way: a variant without data is a string
(`"done"`, `"status"`), a variant with data an object with one key
(`{"locked":{"field":"..."}}`). Names are `snake_case`. Channels are numbered
from 1; 0 is refused.

The shapes above are the `tessera-control-types` `Request` and `Response` envelopes.

---

## Commands

| Command | Fields | `tesseractl` |
|---------|--------|--------------|
| `set_protocol_flows` | `flows` (complete array of `FlowSpec`, the `[[flow]]` fields with identical defaults; empty clears); retains host flows, refused by `[control] locked = ["flows"]` | `flows set FILE`, `flows clear` |
| `set_device_name` | `device`, `name` (at most 31 bytes, no control characters, unique ignoring ASCII case) | `set-name` |
| `set_channel_label` | `device`, `direction` (`rx`/`tx`), `channel`, `label` (`null` restores the default) | `set-label` |
| `subscribe` | `device`, `rx_channel`, `tx_channel` (name), `tx_device` (name) | `subscribe` |
| `unsubscribe` | `device`, `rx_channel` | `unsubscribe` |
| `set_sample_rate` | `device`, `rate` (Hz) | `set-rate` |
| `set_encoding` | `device`, `encoding` (16, 24, 32) | `set-encoding` |
| `set_latency` | `device`, `latency` (us) | `set-latency` |
| `set_clock_role` | `role` (`follower`, `leader`, `auto`) | `clock set-role` |
| `set_clock_priority` | `priority` (0 to 255, lower wins) | `clock set-priority` |
| `set_primary_interface` | `interface` (a name, or `null` to clear) | `network primary` |
| `reload_config` | none | `config reload` |
| `clear_configuration` | `device`, `keep_network` (default `false`); every value set at run time for the device returns to the configuration (name, labels, subscriptions, rate, encoding, latency, clock role and priority, its controller flows, and the primary interface selection unless `keep_network`); refused unless `[control] allow_factory_reset = true` | none (network controllers' factory reset and clear configuration) |
| `lock_access` | `device`, `pin` (exactly four digits); refused if already locked | `access lock` |
| `unlock_access` | `device`, `pin`; a wrong PIN is `incorrect_pin`; after five, unlocking from that kind of client pauses for 60 s | `access unlock` |
| `reset_access_lock` | `device`; unlock without the PIN, only from a local administrator on the control socket (the portal and network controllers get `permission_denied`) | `access reset` |
| `route_receiver` | `expectation` (from a receiver page), `change` (`connect`, `replace` or `disconnect`, with source references from the routing inventory); one receiver change, checked again and never retried | `routing execute` |

`device` is the device key (`[[device]] id`), not its name. Examples:

```json
{"set_latency":{"device":"main","latency":6000}}
{"set_channel_label":{"device":"main","direction":"rx","channel":3,"label":null}}
{"set_primary_interface":{"interface":"enp4s0"}}
"reload_config"
```

A key listed in `locked` in the configuration is refused with `locked`. A
value outside its bounds is refused with `invalid` and the field and reason.
Subscriptions are stored by name and kept while they do not resolve. Without
the native device role the daemon has no flow control: a subscription is
`in_progress` until a static `[[flow]]` or a controller flow carries its rx
channel, then `receiving` or `no_media` according to the media that arrives.
With the native device role (`--native-device`) the daemon requests a flow for
each subscription itself (see Status below and
[interoperability.md](interoperability.md#native-device-role)).

There is no command for host flows or bridges: they are configuration, and
`reload_config` applies them. The flows a controller sets up are the exception:
`set_protocol_flows` replaces that set as a whole.

---

## Queries and replies

| Query | Reply | `tesseractl` |
|-------|-------|--------------|
| `"status"` | `{"status": Status}` | `status` |
| `{"device":{"device":"main"}}` | `{"device": DeviceStatus}` | `status --device main` |
| `"metrics"` | `{"metrics": Metrics}` | `metrics` |
| `"effective_config"` | `{"effective_config": "<TOML text>"}`: the merged files with the values stored at run time applied | `config show --effective` |
| `"bridges"` | `{"bridges": [BridgeStatus, ...]}` | `bridges` |

Other replies: `"done"` (a command succeeded; also the acknowledgement of
`subscribe_events`), `{"event": Event}` and `{"lagged":{"dropped":N}}` on an
event stream.

---

## Events

After `subscribe_events` the daemon answers `{"ok":"done"}` and then sends one
response per event, all with the subscription request's `id`:

| Event | Fields |
|-------|--------|
| `device_renamed` | `device`, `name`, `origin` |
| `channel_label_changed` | `device`, `direction`, `channel`, `label`, `origin` |
| `subscription_changed` | `device`, `status` (a `SubscriptionStatus`) |
| `subscription_removed` | `device`, `rx_channel`, `origin` |
| `device_settings_changed` | `device`, `sample_rate`, `encoding`, `latency`, `origin` |
| `clock_state_changed` | `status` (a `ClockStatus`) |
| `clock_settings_changed` | `role`, `priority`, `origin` |
| `network_settings_changed` | `selected` (or `null`), `origin` |
| `config_reloaded` | none (legacy; implicit origin `config`) |
| `config_reloaded_with_origin` | `origin` (`config`) |
| `subscription_changed_with_origin` | `device`, `status`, `origin` (companion to the legacy event) |
| `protocol_flows_changed` | `flows` (effective controller IDs), `origin` |
| `locked_value_refused` | `field` (full path), `origin`, `attempted` (small value or `null`) |
| `runtime_value_changed` | `field` (full path), `origin`; runtime intent queued for `state.toml` |
| `access_lock_changed` | `device`, `locked`, `origin`; carries no PIN |
| `receiver_operation_changed` | `result`: progress of a `route_receiver` operation (no audio assertion) |

`origin` is `{"cli":{"uid":N}}`, `"config"`, `"internal"`,
`{"protocol":{"peer":"..."}}` or `{"api":{"principal":"..."}}`. The compatibility
adapter uses peer `controller-compatibility`: its historical command supplies
no remote controller address. Other socket commands, including the portal's,
arrive as `cli` with the caller's uid. `protocol_command` is accepted only for
`set_protocol_flows`, with ordinary admin authorization; audit metadata never
grants permission or exempts other commands from confirmation.

**Who owns the controller flow set.** There is one controller set in `state.toml`
(`[[protocol_flows]]`), replaced as a whole by `set_protocol_flows` from any
origin: `tesseractl flows set|clear`, a host, or the Controller compatibility
runtime through its adapter. The set survives a restart of the daemon, and the
compatibility runtime restarts with the daemon (`PartOf=`). The runtime knows no
flows when it starts and reports an empty set. The adapter attempts to filter
that startup report using a recorded run ID:

* The adapter reads `INVOCATION_ID`, defaulting to `""` when absent, and keeps
  the ID of its last applied report in `/var/lib/tessera-controller/flows-run`.
  This is intended to identify a systemd invocation of
  `tessera-controller@.service`; forwarding that ID from the real runtime to
  its adapter child remains unverified after a non-empty controller report.
* A report that names flows is applied: it replaces the set as a whole, like any
  `set_protocol_flows`.
* An empty report is applied only when the received ID equals the recorded ID.
  It clears the current controller set, including flows another writer may
  have stored since the earlier report. A missing or different recorded ID
  makes the adapter answer success without changing that set.
* Explicit commands are never filtered: `tesseractl flows clear` and a
  `set_protocol_flows` from another origin always apply, and a host's
  `[control] locked = ["flows"]` still refuses every one of them.

Startup filtering preserves a set across restart only when the received ID
differs from the previous marker, or no marker exists. If both invocations
receive the empty fallback and the marker is already `""`, their empty reports
are indistinguishable. A runtime-only restart (`Restart=on-failure`) has the
same ownership limitation.

Even with distinct IDs, a later valid non-empty controller report replaces the
whole set and can omit unrelated flows restored after restart or written by
CLI, host or the generic portal command API. A failed marker write also does
not fail an applied report; a later empty report is ignored if the old marker
does not match. Activating the flows lock separately clears stored controller
intent, and unlocking does not restore it. These are limits of the current
adapter/state contract, not changes made by the saved-intent picker.

```json
{"v":1,"id":5,"result":{"ok":{"event":{"clock_settings_changed":{"role":"leader","priority":64,"origin":{"cli":{"uid":1000}}}}}}}
{"v":1,"id":5,"result":{"ok":{"lagged":{"dropped":12}}}}
```

Events of commands are published after the change is applied in memory and
before the debounced write of `state.toml` (500 ms, `[daemon] persist_delay_ms`), so
a crash inside that window can lose a change a subscriber saw.
`clock_state_changed` is a runtime observation and is never persisted.
`subscription_changed` is published when a `subscribe` command changes a
subscription (with the state `in_progress`) and whenever the observed state of a
subscription changes (`receiving`, `no_media`, back to `in_progress`; see below). A subscriber that falls behind loses the oldest events
and gets `lagged`; re-read `status` then.

---

## Errors

| `err` | Meaning | HTTP status in the portal API |
|-------|---------|-------------------------------|
| `{"invalid":{"field":F,"reason":R}}` | bad value or bad request | 400 |
| `{"locked":{"field":F}}` | pinned by `locked` in the configuration | 409 |
| `{"not_found":{"what":W}}` | no such device, or no such network interface (`set_primary_interface`) | 404 |
| `"permission_denied"` | needs root or the admin group | 403 |
| `"rate_limited"` | too many requests; retry later | 429 |
| `{"unsupported":{"what":W}}` | not supported by this daemon | 501 |
| `{"internal":{"message":M}}` | a bug or an internal failure | 500 |
| `{"confirmation_required":{"impact":I}}` | the change restarts something: confirm it (see Envelopes) | 409 |
| `{"response_too_large":{"size":N,"limit":L,"key":"control.max_frame_bytes"}}` | the reply did not fit the daemon's frame limit; raise the key | 502 |

`tesseractl` prints the error's text form (`sample_rate is locked by the
configuration`) and exits with 1, or with 4 for `permission_denied`.

---

## Status

`Status` (reply to `status`):

| Field | Content |
|-------|---------|
| `version` | daemon version |
| `devices` | `DeviceStatus` per device: `key`, `name`, `sample_rate`, `encoding`, `latency`, `rx_channels` and `tx_channels` (`channel`, `label`), `subscriptions`, `locked`, `access_locked` (`true` while the device access lock is set, absent when open), and `formats` (absent from older daemons): `sample_rates` and `encodings`, the values the device can be set to now (exactly what `set_sample_rate` and `set_encoding` accept, the current value included, ascending; a network controller is offered the same lists), and `sample_rate_pin` / `encoding_pin`, null while another value is accepted, else why not: `"configuration"` (`locked`), `{"bridge":{"name":N,"sample_rate":HZ}}` (an enabled `[[bridge]]` pins the rate) or `"unsupported"` |
| `clocks` | `ClockStatus` per interface: `interface`, `state` (`free_running`, `listening`, `acquiring`, `locked`, `holdover`, `leader`), `timestamping` (`hardware`/`software`), `offset_ns`, `rate_ppb`, `grandmaster`, `epoch`, `runner_starts`, `commands_applied`, `commands_rejected` |
| `clock_settings` | `role`, `priority`, `locked` (may be absent from older daemons) |
| `network` | `NetworkStatus`: `primary`, `source` (`locked`, `selected`, `configured`, `automatic`, `fallback`), `selected`, `locked`, `allow_wireless`, `interfaces` (`name`, `mac`, `ipv4`, `carrier`, `wireless`, `is_virtual`, `usable`, `reason`); may be absent |
| `warnings` | human-readable health warnings |
| `bridges` | `BridgeStatus` per `[[bridge]]` (empty without bridges) |

`SubscriptionStatus`: `rx_channel`, `tx_channel`, `tx_device`, `state`,
`effective_latency`. The type defines these states:

| `state` | Produced by the current daemon |
|---------|--------------------------------|
| `in_progress` | **yes**: a stored subscription whose rx channel no `[[flow]]` carries, or whose flow has not yet received a packet (first 5 s after the flow started) |
| `receiving` | **yes**: a rx `[[flow]]` of the device carries the channel and its packets are arriving (the flow's packet count grew at one of the last two one-second samples) |
| `no_media` | **yes**: the flow carries the channel but its packets stopped (shown about 2 to 3 s after the last packet), or none arrived in the 5 s after the flow started |
| `unresolved`, `{"flow_refused":{"code":N}}`, `{"rate_mismatch":{"ours":R,"theirs":R}}`, `{"encoding_mismatch":{...}}` | only with the native device role: the transmitter or its channel was not found in discovery (`unresolved`), the transmitter refused the flow request with that code, or it runs another rate or encoding; without the role nothing produces them |

The state comes from the media layer, not from the subscription's names: once a
second the daemon reads the packet counter of every rx flow (the same counters
as `metrics`) and sets the state of each rx channel the flow carries. A stop
shows as `no_media` about 2 to 3 s after the last packet (measured from the sample
that saw the counter grow; a sample skipped under load delays it), a restart as
`receiving` within about a second. A flow that is stopped, removed or rebuilt by a
reload does not leave a stale state: a rebuilt flow starts again at `in_progress`,
a removed one returns its channels to `in_progress`. Static flows do not say where
their media comes from, so `receiving` means that the channel is receiving media, not that the
media comes from the `tx_channel` and `tx_device` named in the subscription.

`effective_latency` is `null` today. Without the native device role `tesserad`
does not implement the vendor control plane itself; the packaged Controller
compatibility runtime submits the flows a controller sets up
([interoperability.md](interoperability.md)).

---

## Metrics

`Metrics` (reply to `metrics`). Counters only grow; gauges move; `max_*` are
peaks since the start.

| Part | Fields |
|------|--------|
| `daemon` | `uptime_ms`, `control_connections` (gauge), `control_connections_total`, `control_connections_refused`, `requests_total`, `requests_rate_limited`, `requests_denied`, `requests_invalid`, `subscribers` (gauge), `subscriber_lags`, `task_restarts` |
| `audio` | `sessions` (gauge), `sessions_total`, `rejects_total`, `ring_bytes` (gauge), `connections_refused` |
| `clocks` | as in `Status` |
| `engine` | `running`, `ticks`, `tick_skips`, `max_wake_lateness_us`, `max_tick_work_us`, `rx_datagrams`, `rx_batches`, `rx_filtered`, `rx_truncated`, `rx_batch_limit_hits`, `tx_datagrams`, `tx_batches`, `tx_send_failures`, `tx_suppressed_ticks`, `wake_signals`, `sessions`, `flows`, `rt_degraded`, `mlock_failed`, `events_dropped`, `rx_errors`, `retire_leaks`, `rx_transit_hist` (bucket 0 below 0, then 50 us buckets, the last 2000 us and up), `rx_transit_max_us`, `socket_buffer_min_bytes`, `socket_buffers_capped`, `wake_lateness_hist` (upper edges 25, 50, 100, 250, 500, 1000, 2000, 4000 us, then the rest) |
| `flows` | per `[[flow]]`: `id`, `device`, `direction`, `packets`, `frames`, `bytes`, and `rx` or `tx` |
| `flows[].rx` | `malformed_packets`, `early_packets`, `late_packets`, `late_frames`, `gap_events`, `gap_frames`, `out_of_order_packets`, `discontinuities`, `epoch_resets`, `hole_samples`, `unwritten_samples`, `overrun_samples`, `valid_samples`, `epoch_mismatch_samples`, `max_lateness_us`, `lateness_hist` (upper edges 0, 125, 250, 500, 1000, 2000, 4000, 8000, 16000, 32000 us, then the rest), `keepalives_sent`, `keepalive_failures` |
| `flows[].tx` | `underrun_samples`, `late_skips`, `skipped_packets`, `resyncs`, `max_send_lag_us`, `send_failures` |
| `bridges` | as in `Status` |

---

## Bridges

`BridgeStatus`, one per `[[bridge]]`, also in `status` and `metrics`. Gauges
are at most about a second old.

| Field | Meaning |
|-------|---------|
| `name`, `device`, `card`, `direction` (`playback`/`capture`), `first_channel`, `channels` | identity |
| `state` | `starting`, `running`, `reopening` (retrying with back-off, see `last_error`), `failed` (until the configuration changes), `disabled` |
| `fill_frames` | frames queued in the card |
| `position_error_frames` | the loop's error against the media clock (playback: positive is behind) |
| `correction_ppb`, `trend_ppb`, `ratio` | correction applied, offset learned (correction without its proportional term), resampling ratio |
| `time_constant_s`, `max_correction_ppm` | the loop tuning in effect |
| `added_latency_frames` | latency the bridge adds (its guard or lead) |
| `xruns`, `reopens`, `open_failures`, `generation`, `card_errors`, `chunks`, `silence_chunks`, `missing_frames`, `slips`, `short_frames` | counters; `generation` changes on every fresh start of the bridge, not on a live retune |
| `rt_degraded` | real-time scheduling was refused |
| `last_error` | the last open, card or configuration error |

---

## Examples

With `tesseractl --json` the output is pretty-printed. `tesseractl` strips the
reply wrapper of the socket (`{"status": ...}`, `{"device": ...}`,
`{"metrics": ...}`, `{"bridges": [...]}` in [Queries and replies](#queries-and-replies))
and prints the control type inside it; a few outputs are `tesseractl`'s own
small objects:

| Command | JSON printed |
|---------|--------------|
| `status` | `Status` (`{"version": ..., "devices": [...], ...}`) |
| `status --device KEY` | `DeviceStatus` (`{"key": ..., "latency": ..., ...}`) |
| `metrics` | `Metrics` (`{"daemon": ..., "engine": ..., "flows": [...], ...}`) |
| `bridges` | an array of `BridgeStatus` (`[]` without bridges) |
| a successful change | `{"ok":true}` (`tesseractl`'s own) |
| `clock show` | `{"settings": ClockSettings or null, "clocks": [ClockStatus, ...]}` (`tesseractl`'s own) |
| `network show` | `NetworkStatus` or `null` |
| `config show --effective` | `{"effective_config": "<TOML>"}` (`tesseractl`'s own, same shape as the reply) |
| `config show` (files) | the merged `Config` |
| `config check` | `{"ok":true,"files":N,"warnings":M}` (`tesseractl`'s own) |
| `watch` | one `Reply` per line: `{"event":{...}}` or `{"lagged":{"dropped":N}}` |

```sh
# Is the clock locked?
tesseractl --json status | jq -r '.clocks[] | "\(.interface) \(.state) \(.offset_ns)"'

# Keepalives of every rx flow
tesseractl --json metrics | jq '.flows[] | select(.rx) | {id, sent: .rx.keepalives_sent, failed: .rx.keepalive_failures}'

# Bridges that are not running
tesseractl --json bridges | jq -r '.[] | select(.state != "running") | "\(.name): \(.state) \(.last_error // "")"'

# React to renames
tesseractl --json watch | jq --unbuffered -r 'select(.event.device_renamed) | .event.device_renamed.name'

# Change the latency of device "main" and check the result
tesseractl --device main --json set-latency 6000 && tesseractl --json status --device main | jq .latency
```

A program that talks to the socket directly sends one frame (4-byte
big-endian length, then the JSON request) and reads one frame back. In Rust,
`tesseractl::client::Client` (`crates/tesseractl/src/client.rs`) does exactly
that and is what `tessera-web` uses; it is not a published API.
