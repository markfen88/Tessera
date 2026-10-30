# Web portal (`tessera-web`)

`tessera-web` shows a tessera device in a browser and changes what `tesseractl`
can change. It is a separate service and a client of the daemon's
control socket, like `tesseractl` (it uses the same client library). It never
touches the daemon's state directly. Standalone host networking uses the
separate typed Rust helper described in [network-vlans.md](network-vlans.md);
NDIMon remains the network owner when it hosts Tessera. Code:
`crates/tessera-web`.

The **Routing** page connects, replaces and disconnects receive inputs: a
matrix of transmit channels and receive inputs, a review step, then one command
through the daemon ([below](#routing)). It routes this host's receiver and other
devices' receivers, so either host's portal can route a pair. The Device page
also keeps a plain-HTML picker for saved subscription intent: choose the
receiving input (RX), transmitting device (TX), output channel, then review the
names before saving. Lists use the daemon's paged inventory; expired selections
require choosing again. Ambiguous names cannot be selected. Advanced manual
entry remains available for offline sources. Network browsing is opt-in through
`[discovery] browse_devices = true`; it defaults to off, and the Routing page
needs it.

After an Avahi resolver failure, a returning source can remain absent until a
new discovery session if Avahi keeps its cached service pointer without a new
announcement. Discovery stays reconciling in that case. Prefer advanced manual
entry for saved intent when a source is not listed. Toggling `browse_devices`
off and on with configuration reloads starts a new discovery session;
restarting tesserad also does so but interrupts audio.
The built-in discovery backend cannot identify the interface, and an empty
available cache never proves that the LAN has no devices.

What saving a subscription does depends on who serves the device role
([interoperability.md](interoperability.md#native-device-role)). With the
native device role, the saved subscriptions are what the daemon requests from
transmitters: saving, replacing or removing one connects, changes or
disconnects that input, although the picker's own messages still say that
audio flows are unchanged. With the compatibility runtime, saving intent does
not connect or disconnect audio, and the daemon refuses it for the first
device while the runtime is registered; use the Routing page there. Existing
configured flows and routes established through Dante Controller continue to
carry audio; intent, carrier ownership and packet observations are shown
separately. Packet activity does not verify the selected source.

---

## Contents

- [Install and start](#install-and-start)
- [Pages](#pages)
- [JSON API](#json-api)
- [Authentication and sessions](#authentication-and-sessions)
- [Configuration (`web.toml`)](#configuration-webtoml)
- [Network exposure](#network-exposure)
- [Rights](#rights)
- [Service unit](#service-unit)
- [Appliance mode](#appliance-mode)
- [Limits](#limits)

---

## Install and start

```sh
sudo apt install ./tessera-web_<version>_<arch>.deb
sudo tessera-web set-password                 # optional: replace the initial tessera password
xdg-open http://127.0.0.1:8090                # or, from another machine, an SSH tunnel:
ssh -L 8090:127.0.0.1:8090 user@device        # then open http://127.0.0.1:8090 there
```

The package creates the user `tessera-web` (a member of groups `tessera` and `tessera-admin`),
installs `/etc/tessera/web.toml` (a conffile with every key commented out) and
the unit, enables and starts the portal on a fresh install, and initializes
the password to **`tessera`** only if no password file exists. Upgrades preserve
an existing password and restart a running portal. Purging
the package deletes `web.toml` and the password file.

If `tessera` is refused after an upgrade, an existing password was preserved.
Use `sudo tessera-web set-password` on the device to choose a replacement of
at least eight characters. The command prompts privately; do not put the
password in command arguments. The new password applies at the next login
without restarting the portal or audio services.

---

## Pages

The **Services** navigation entry (`/services`) is present by default, even
without Speaker mode. It lists the audio daemon, Controller compatibility
(`tessera-controller@main`), portal, network configuration and Avahi discovery.
Status, enable/disable and restart use the existing service allowlist, session,
CSRF and essential-service confirmation checks. A configured
`[[appliance.service]]` list replaces these defaults, including on the Services
page. Package installation generates the matching polkit rules automatically;
regenerate them with `tessera-web appliance polkit` after editing the list.

Server-rendered HTML with forms. Status pages use a small same-origin browser
script (`/live.js`) to read fresh HTML in the background and update values in
place, without reloading the document. Focused or edited forms, file
selections, expanded details and scroll position are preserved. Hidden tabs
pause polling; failed reads retain the last values and retry with backoff.
The same script posts the Speaker page's sliders and service buttons in
place: the same forms and token, with the header `X-Tessera-Live: 1`, which
makes the handler answer JSON (`{"ok": …, "text": …}` and a few fields of
its own) instead of a redirect and a one-shot message; every other check
(session, token, origin, allowlists, confirmations) is the same. Without
JavaScript, pages and forms still work (the sliders keep an **Apply**
button); status changes need a manual reload. Every page except the login
page needs a session.

Every page of a session has the same frame: a top bar with the brand, a host
chip (the device's name and the primary interface's address, once a status
was seen), the navigation pills (the current page marked), the theme control
and **Log out**; a footer with the portal's version, the page's refresh
interval and the UTC time. Without a session (the login page, a missing
page) only the brand is shown. Content is in cards on a twelve-column grid that stacks on
a phone (under 720 px).

| Path | Page | Refreshes | Changes |
|------|------|-----------|---------|
| `/` | Overview: the **signal path** (network, flows, device, audio clients; a path is highlighted while packets move on it, judged from two metric samples), devices, network, clock, warnings | every 5 s | |
| `/network` | standalone VLAN profiles (addressing, and management's DNS servers and search domains) and management/audio assignments; legacy interface selection where the helper is absent | | edit a draft, review/apply, explicitly keep or revert |
| `/devices` | Device: per device, the access lock, settings, channel labels, the receiver's routes, subscriptions | | lock and unlock (see below); name, rate, encoding, latency; channel counts (with a confirmation step, see below); labels; subscribe and unsubscribe (what that does: above the table) |
| `/routing` | Routing: the matrix of transmit channels and receive inputs, or a list ([below](#routing)); `/routing/review`, `/routing/recovery`, `/routing/carrier-recovery` | every 10 s | Connect, Replace, Disconnect after a review; recovery of an uncertain operation |
| `/clock` | clock settings and the clock table | every 10 s | role and priority |
| `/metrics` | daemon counters (with the audio clients), media engine counters, per-flow counters | every 5 s | |
| `/events` | the latest events since the portal started (it keeps 200), newest first, UTC | every 5 s | |
| `/config` | the configuration in effect (TOML) | | reload the configuration |
| `/login`, `/logout` | log in (a centred card) and out | | |
| `/theme` (POST only) | | | choose the appearance, see below |
| `/speaker` | **appliance mode only** ([below](#appliance-mode)): the control panel: volume and signal boost, warnings, host vitals and the daemon summary, the clock, one switch per configured service, sources, the signal path, devices and subscriptions, EQ files, network, links | every 5 s | volume and boost (live sliders), enable, disable or restart an allowlisted service (an essential one asks first), EQ files |
| `/device`, `/overview` | appliance mode only: the overview (`/` redirects to `/speaker`) | every 5 s | |
| `/assets/<hash>/style.css` | the stylesheet, at a path that changes with its content (cached for a year); `/style.css` and `/static/style.css` serve it uncached | | |

### Routing

`/routing` shows a matrix: transmit channels (TX, application to network) down
the left, grouped by device, and receive inputs (RX, network to application)
across the top, grouped by device. Device bands can be collapsed (a collapsed
band shows read-only route counts), both axes can be filtered, and **List**
shows the same routes receiver by receiver (the view on a narrow screen). The
first four devices' inputs are read when the page loads; further devices have a
**Read inputs** link. Transmit channels come in pages of 16. A column's title
names the device, marks this host, gives its 16-digit device ID (the value for
`[control] routing_sources`, [configuration.md](configuration.md#receiver-routing))
and the reason when its inputs cannot be changed. Each cell shows the state of
the route: receiving, receiver-confirmed, no media, pending, empty or
unavailable.

A cell opens a change: **Connect** an empty input, **Replace** its source, or
**Disconnect** the current one. The review page shows the change and the current
source; **Confirm change** sends one command, and the result page says whether
the receiver confirmed the selection, refused it, had changed in the meantime,
or whether the outcome is unknown (then further changes on that receiver wait
for **Recover uncertain operation**,
[troubleshooting](troubleshooting.md#recovering-an-uncertain-routing-operation)).
Nothing is retried. The daemon checks the receiver's selection, the source and
the locks again before it sends anything. Allow about five seconds for audio to
settle.

A cell is disabled when the receiver cannot be changed (the reason is in its
column title: a `subscriptions` lock, the device access lock, an uncertain
operation, a remote receiver with more than one page of inputs, this host's
receiver with neither the native device role nor a running compatibility
runtime), when the
source's name is ambiguous, when the source is the receiving device itself, and,
on a receiver still on the compatibility runtime, when the source channel
already feeds another input of that receiver. A receiver on another device is
changed over that device's control protocol and applies its own locks. A
confirmed selection does not verify audio: "receiving" means packets arrive.
**Review restart recovery** is for hosts on the compatibility runtime
([troubleshooting](troubleshooting.md#reviewing-carriers-after-a-runtime-restart)).

### Device access lock

Each device card on `/devices` starts with **Access lock: open** or **locked**,
and the overview's device table has an **Access** column. An open device offers
**Lock**: the PIN (four digits) typed twice; the portal refuses a mismatch or
another form before asking the daemon. A locked device says what is refused
and offers **Unlock** with the PIN. The commands are the same `LockAccess` and
`UnlockAccess` that `tesseractl access` sends, and the daemon decides
everything ([what a lock refuses](cli.md#access), [where it is stored and how
the PIN is protected](configuration.md#device-access-lock)). The PIN fields are
password fields that the browser is asked not to fill in; the portal never
logs, shows or keeps a PIN. Wrong PINs from the portal are counted apart from
the command line: after five, unlocking from the portal pauses for 60 s while
`tesseractl access unlock` still works.

There is no reset in the portal: a forgotten PIN is reset by an administrator
on the device (`sudo tesseractl access reset`). The daemon refuses a reset sent
by the portal's account, and `POST /api/v1/commands` refuses
`reset_access_lock` itself (403) without forwarding it.

### Appearance

The pages have a dark and a light theme. The segmented control in the top
bar (**Auto**, **Light**, **Dark**) is a form that posts to `/theme` with the
session's CSRF token; the choice is kept in the cookie `tessera_theme`
(`light` or `dark`; `HttpOnly`, `SameSite=Strict`, `Secure` with
`cookie_secure = true`, one year; **Auto** deletes it) and the browser is sent
back to the page it was on (a page of the navigation; otherwise the
overview). The login page has no theme control (it has no session) but
follows the cookie. **Auto** follows the browser's
`prefers-color-scheme`. The theme is rendered into `<html data-theme=…>` by
the server, so a page never shows the wrong colours while it loads, and
nothing in this needs JavaScript. Animations and transitions are off for
browsers that ask for reduced motion. The content security policy refuses
inline `style` attributes, so values the stylesheet needs from the server
(the filled part of a range input) are classes (`fill-0` … `fill-100`, in
5 % steps).

Subscriptions on `/devices` are stored by name. Their state follows the media of the
static `[[flow]]` that carries the rx channel: "in progress" until it does (and for
the first seconds of a flow), then "receiving" while packets arrive or "no media" (about 2 to 3 s after the last packet),
and the "N of M receiving" count uses it ([control-interface.md](control-interface.md#status)).
Who sets up those flows is the device role: the daemon itself with the native device role, otherwise the
packaged Controller compatibility runtime, which submits the flows a controller sets up
([interoperability.md](interoperability.md)).

**Changes that restart something.** The channel counts form on `/devices`
first runs a dry run and shows a confirmation page: a warning that applying
restarts the device (its flows and audio clients; for a daemon restart, the
whole service), the daemon's description of the impact, **Confirm and apply**
and **Cancel** (changes nothing). Confirm sends the impact's digest back; if the
situation changed meanwhile, nothing is applied (409) and the new impact is
shown (reloading that page only repeats the refusal with the current impact;
nothing is applied until Confirm is pressed on a page that shows it). Errors (locked, invalid, over the memory budget, not a number) appear
above the form again. The other restarting forms (rate, encoding, latency,
primary interface, reload) use the same page; the JSON API has no such step yet.

**Rate and encoding.** The two lists on `/devices` hold exactly the values the
daemon accepts now, the values a network controller is offered too
(`DeviceStatus.formats`). When nothing but the current value is accepted, the
list is disabled and a line under the form says why: ``Sample rate fixed at 48
kHz: bridge `speaker` runs its card at 48000 Hz (`sample_rate` in its
configuration).`` (an enabled `[[bridge]]` pins it), ``... locked by the
configuration (`locked` of the device).`` or ``... no other value is supported.``
A daemon that does not report the lists gets the documented rates and
encodings.

The sound-card bridges and the receiver keepalive counters are not shown on
the pages yet; `GET /api/v1/status` and `GET /api/v1/metrics` include them.
A refused change is shown as a message on the next page. Without the admin
group the message says so.

---

## JSON API

The contract for any other user interface. Bodies are the types of
`tessera-control-types`, serialised as described in
[control-interface.md](control-interface.md).

| Method and path | Body | Answer |
|-----------------|------|--------|
| `POST /api/v1/session` | `{"password": "..."}` | `{"csrf": "<token>"}` and a `Set-Cookie: tessera_session=...` |
| `DELETE /api/v1/session` | (header `X-CSRF-Token`) | 204, cookie cleared |
| `GET /api/v1/status` | | `Status` |
| `GET /api/v1/metrics` | | `Metrics` |
| `GET /api/v1/config` | | the effective configuration, `application/toml` |
| `GET /api/v1/devices/{key}` | | `DeviceStatus` |
| `GET /api/v1/routing/devices?cursor=…` | optional opaque continuation | `RoutingDevicePage` (provisional 0.x) |
| `GET /api/v1/routing/channels?endpoint=…&generation=…&direction=rx\|tx&cursor=…` | current daemon endpoint reference | `RoutingChannelPage` (provisional 0.x) |
| `GET /api/v1/routing/receiver?endpoint=…&generation=…&first=…` | current daemon endpoint reference | `RoutingReceiverPage`: the receiver's selections, with a single-use expectation per changeable input for a `route_receiver` command (provisional 0.x) |
| `GET /api/v1/routing/operation?operation=…` | optional operation ID | the inspection of an uncertain receiver operation, with its review token |
| `GET /api/v1/routing/carrier-recovery` | | the carrier-recovery inspection of a host on the compatibility runtime |
| `POST /api/v1/commands` | a `Command`, e.g. `{"set_latency":{"device":"main","latency":6000}}`; header `X-CSRF-Token` | `{"ok": true, "reply": <Reply>}`, e.g. `{"ok": true, "reply": "done"}` |
| `GET /api/v1/events` | | Server-Sent Events: `event` messages (an `Event` as JSON) and `resync` (events were lost: re-read the status); keep-alive comments every 15 s |

Errors are JSON objects with an `error`: `{"error":{"message":"..."}}` for the
portal's own refusals, or `{"error": <CommandError>, "message": "..."}` for
the daemon's. Two exceptions are not JSON: the cross-origin refusal (403,
plain text `cross-origin request refused`) and the HTTP framework's own
rejection of a `POST /api/v1/session` body that is not a JSON
`{"password": ...}`.

| HTTP | When |
|------|------|
| 400 | `invalid` from the daemon, or a body that is not a command |
| 401 | no session (`log in first`), or a wrong password |
| 403 | missing or wrong `X-CSRF-Token`, a cross-origin request, `permission_denied` or `incorrect_pin` from the daemon, or a `reset_access_lock` command (refused by the portal) |
| 404 | `not_found` |
| 409 | `locked`, `access_locked` (the device access lock refuses the change), or `confirmation_required`: the command restarts something and was sent unconfirmed; nothing changed, and the error carries the `impact` (there is no confirmed form in the API yet) |
| 429 | `rate_limited`, or too many failed logins |
| 500, 501 | `internal`, `unsupported` |
| 502 | the daemon is not reachable or answered unexpectedly, or refused to send a reply above its frame limit (`response_too_large`; `[control] max_frame_bytes`) |
| 503 | no password is set yet |

Example with `curl` (illustrative; not run as part of this documentation):

```bash
B=http://127.0.0.1:8090/api/v1
read -rs -p 'Password: ' PW; echo               # read and printf are shell built-ins: the password stays out of argv
CSRF=$(printf '%s\n' "$PW" | jq -Rc '{password: .}' |
       curl -s -c jar -H 'Content-Type: application/json' -d @- $B/session | jq -r .csrf)
unset PW
curl -s -b jar $B/status | jq '.clocks'
curl -s -b jar -H "X-CSRF-Token: $CSRF" -H 'Content-Type: application/json' \
     -d '{"set_primary_interface":{"interface":"enp4s0"}}' $B/commands
curl -s -N -b jar $B/events                      # stream events
curl -s -b jar -X DELETE -H "X-CSRF-Token: $CSRF" $B/session
```

The API follows the semver rules of `tessera-control-types`: changes are
additive; a breaking one would be `/api/v2`.

---

## Authentication and sessions

| Rule | Detail |
|------|--------|
| Password always required | also on loopback: the portal can hold the daemon's admin rights, so an open portal would make every local user an administrator |
| Password storage | an Argon2id hash in `/etc/tessera/web-password` (mode 0640, group `tessera-web`), written atomically by `tessera-web set-password`, which reads standard input (never the command line). The file is read at every login, so a new password applies without a restart |
| No password file | the portal starts, and every login answers how to set one (503 in the API) |
| Session | a random token in the cookie `tessera_session` (`HttpOnly`, `SameSite=Strict`, `Secure` with `cookie_secure = true`, `Path=/`), held in memory: a restart logs everyone out. At most 64 sessions; the oldest is dropped. Ends after `session_idle_secs` without use (8 h) |
| CSRF | every state-changing request (form or API) must carry the session's token (a form field, or the `X-CSRF-Token` header) |
| Origin | a non-GET request whose `Origin` header names another host than `Host` is refused (403) |
| Login rate limit | at most 5 failed logins per client address and 30 overall per minute |
| Response headers | `Content-Security-Policy: default-src 'none'; style-src 'self'; script-src 'self'; connect-src 'self'; img-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'`, `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: same-origin`, `Cache-Control: no-store` (the content-hashed stylesheet alone is `public, max-age=31536000, immutable`) |

`Referrer-Policy: same-origin` preserves the same-origin form behavior required
by the origin check.

---

## Configuration (`web.toml`)

`/etc/tessera/web.toml` (or `tessera-web --config FILE`). Every key is
optional; a missing file means the defaults; an unknown key is an error.

| Key | Default | Meaning |
|-----|---------|---------|
| `listen` | `"127.0.0.1:8090"` | address and port |
| `control_socket` | `"/run/tessera/control.sock"` | the daemon's control socket |
| `password_file` | `"/etc/tessera/web-password"` | the Argon2id hash |
| `cookie_secure` | `false` | mark the session cookie `Secure` (behind a TLS reverse proxy) |
| `session_idle_secs` | `28800` | a session unused this long ends (at least 1) |
| `daemon_timeout_secs` | `5` | how long one request to the daemon may take (at least 1) |
| `[appliance]`, `[[appliance.service]]`, `[appliance.camilladsp]`, `[appliance.volume]` | off | appliance mode: see [below](#appliance-mode) |
| `[network]` `confirm`, `interface` | `"require_management_address"`, automatic | read by the network helper, not the portal: `confirm = "token_only"` lets Keep arrive from any reachable address, `interface` names the wired port for the first adoption ([network-vlans.md](network-vlans.md#confirmation-policy)) |

```sh
sudo tessera-web check      # listen 127.0.0.1:8090; control socket /run/tessera/control.sock; password file /etc/tessera/web-password
```

**Drop-ins.** `web.toml` is a package-owned configuration file, so site and profile settings go in
`/etc/tessera/web.d/*.toml` instead (the directory is named after the file, `web.toml` → `web.d`).
The files are read after `web.toml`, in name order, and a later file wins. Tables merge key by key;
any other value replaces the earlier one, and a list in a later file replaces the whole list, so
a drop-in with `[[appliance.service]]` entries defines the complete service list. Hidden files and
files that do not end in `.toml` are ignored; an unknown key or a syntax error in any file is
an error that names the file. `tessera-web check` and `tessera-web appliance polkit` read the same
files, and so does the network helper (`tessera-network`) for the `listen` port and the `[network]`
policy, under the owner checks that already apply to `web.toml` (a root-owned regular file nobody
else can write). The speaker profile of the bundle writes `web.d/60-speaker.toml`
([dsp.md](dsp.md)).

---

## Network exposure

The portal speaks plain HTTP. By default it answers on loopback only.

| Way | How | Note |
|-----|-----|------|
| SSH tunnel | `ssh -L 8090:127.0.0.1:8090 user@device` | nothing to configure |
| TLS reverse proxy | proxy to `127.0.0.1:8090`, pass the original `Host` header, set `cookie_secure = true` | the origin check compares `Origin` with `Host` |
| listen on the network | `listen = "0.0.0.0:8090"` | the password and the cookie cross the network unencrypted; the portal logs a warning at start. Only on a trusted, dedicated network |

---

## Rights

The service runs as `tessera-web`, a member of group `tessera`, so it may read
everything. It may change settings only if the operator also adds it to the
admin group (`[daemon] admin_group`, default `tessera-admin`):

```sh
sudo usermod -aG tessera-admin tessera-web && sudo systemctl restart tessera-web
```

Without that the portal is read-only and says so when a change is refused.
The daemon records the portal's commands as coming from the CLI with the
portal's uid (`origin: {"cli":{"uid":N}}`), except the device access lock
commands, which it records as `{"api":{"principal":"portal"}}` so that the
portal cannot reset a lock without its PIN; the portal's log
(`journalctl -u tessera-web`) names the session that sent each command kind.
The daemon currently identifies the portal process rather than individual web users.

---

## Service unit

`tessera-web.service` (`crates/tessera-web/dist/tessera-web.service`):
`User=tessera-web`, `Restart=on-failure`, no capabilities, `PrivateDevices`,
`ProtectSystem=strict` with `ReadWritePaths=-/run/tessera` (connecting to a
socket needs write permission on it) and `-/var/lib/tessera-rate` (the rate
follower's file, see [Rate follower](#rate-follower)), `RestrictRealtime=yes`, address
families `AF_UNIX AF_INET AF_INET6`, `SystemCallFilter=@system-service`. Two
runtime worker threads.

---

## Appliance mode

A device built around tessera (a speaker, a streamer) can make the portal
its control panel: with `[appliance] enabled = true` in `web.toml`, `/` is
the **Speaker** page (`/speaker`) and the overview moves to `/device` (alias
`/overview`); the nav gains a **Speaker** entry. Off (the default) nothing
of this exists: the paths are 404 and `/` is the overview. It is runtime
configuration, not a build option.

**The page** (refreshes every 5 s) is the device's control panel, in this
order: the **master card** at the top, with the amplifier's **Volume** (with
`[appliance.volume]`) and the EQ engine's **Signal boost** (with
`[appliance.camilladsp] enabled = true`) side by side, each a large readout
and a large slider (see "EQ engine" below), and under them, with
`[appliance.rate_follow]`, the line **Sample rate follows the network device:
<Hz>** with the follower's last problem (see "Rate follower"; the card is
absent when none of these is configured); the **EQ engine input rate mismatch** notice (only while the
engine's ALSA capture rate differs from the Tessera device's rate, with a
one-click fix when the configuration allows it and the rate follower is off; see
[the Raspberry Pi speaker guide](raspberry-pi-speaker.md)); the **EQ engine
output format not supported** notice (only while the engine's stop reason is
an ALSA `snd_pcm_hw_params_set_format` failure: it quotes the engine, names
the playback device and `format` of the active configuration and says where
to change it, with no button; an input-side failure says *input*); the daemon's
**Warnings** (only when there are any); a hero
card with the device name (or `hostname_label`), the daemon's summary
(device, channels, rate, encoding, latency) and chips for the clock
(locked, unlocked, none), the temperature
(`/sys/class/thermal/thermal_zone0/temp`, only if the host has one) and the
uptime (`/proc/uptime`); the **Clock** (role, priority, one line per clock
with state, offset and leader); the **Services** card; a **Sources** card
(the connected Bluetooth devices and the daemon's subscriptions); the
overview's **Signal path** diagram; the **Devices** table with every
subscription and its state; the EQ files; the **Network** card (interfaces
and addresses from the daemon's status, the primary, the PTP leader) with
links to the overview page and, if `gui_url` is set, the EQ engine's own
GUI.

**The EQ engine's GUI.** With `[appliance.camilladsp] gui_url` set, the
button **EQ engine (CamillaDSP)** is under the master card's sliders, in
the hero's chips and in the Links; it opens the engine's own GUI in a new
tab (`rel="noopener noreferrer"`), with the engine's state beside it
(**engine off** while the engine cannot be asked: the GUI will not load
then). The URL may contain `{host}` in place of the host, e.g.
`http://{host}:5005/gui/index.html`: the page puts there the name or
address the browser used to reach the portal (the request's `Host` header
without its port; an IPv6 literal keeps its brackets), so the link follows
the device's address. Only letters, digits, `.`, `-`, `_` and a bracketed
IPv6 literal are accepted from the header, with at most a numeric port;
anything else falls back to the device's host name, and when that is not
usable either the button is left out. A `gui_url` without the placeholder
is used as it is. The configuration accepts `{host}` once, only before
the path, and still requires `http://` or `https://`. The GUI has no login
and listens on one address (`bind_address` in `/etc/camilladsp/camillagui.yml`:
loopback by default, the speaker's management address after the bundle
installer): the speaker profile writes `gui_url` with that address, not
`{host}`, and writes none when the GUI is off or listens on loopback only, so
the button is left out ([dsp.md](dsp.md)).

Bluetooth is asked from `bluetoothctl devices Connected` only while
the `bluetooth` unit is active (stopped, the command blocks for seconds); a
failed command shows **unavailable**, an empty answer **nothing
connected**. The `Connected` filter needs BlueZ 5.65 or newer; older
versions ignore it and list every known device. The overview itself stays
at `/device`.

**Live controls.** With JavaScript the sliders and the service buttons act
in place and report next to the control ("applying…", "applied", "failed:
…"); the background refresh never overwrites a slider that is being dragged
or has the focus, and values changed elsewhere (another browser, `amixer`)
appear at the next refresh once the slider is left. Sliders take the arrow
keys and announce their value (`aria-valuetext`). Without JavaScript each
slider has an **Apply** button and each action a redirect with a message.

### Services

Each `[[appliance.service]]` entry is one row: a switch, the label and
description, a status pill and a restart button.

| Key | Meaning |
|-----|---------|
| `unit = "name"` | one service (`name` or `name.service`; only services, no timers or sockets) |
| `units = ["a", "b"]` | a group: one switch acts on every unit, in this order; "active" only if all run; greyed if any is masked or missing |
| `label` | required, shown bold |
| `description` | shown small; default: the unit names |
| `essential = true` | disabling or restarting first shows a confirmation page. Entries with `tesserad` or `tessera-web` are essential whatever the file says |

Either `unit` or `units`, not both; a unit may appear in one entry only
(`a` and `a.service` are the same unit); names are letters, digits, `_`,
`@`, `-`, not starting with `-`, with no suffix or `.service`. Errors name
the entry: `appliance.service[2].units: ...`.

The page reads every unit's state in one `systemctl show -p
Id,LoadState,UnitFileState,ActiveState ...` call (no privilege needed; if
`systemctl` does not answer, every row is greyed with the reason). The switch
follows `UnitFileState`: ON for `enabled`, `enabled-runtime`, `alias`,
`generated`, `transient`, `linked`; OFF for `disabled` and `indirect`;
**static** units have no switch (they are started by what needs them) and
show their status; greyed with the reason for `masked`, a missing unit, or
when `systemctl` could not be asked. The pill appears when the switch is ON
(or the unit is static): **active** (`ActiveState` active or reloading) or
**stopped**. Restart is offered unless a unit is missing or masked. In a
group, enable and disable leave static units alone; restart touches all.

What a press runs, per unit, as the portal's own user (`<name>` is the
service without its suffix; `tessera-web-unitfile@` is the root helper
described under the security model):

| Press | The portal runs | Then, as root, the helper runs |
|-------|-----------------|--------------------------------|
| switch OFF → ON | `/usr/bin/systemctl start tessera-web-unitfile@enable-<name>.service` | `/usr/bin/systemctl enable --now <name>.service` |
| switch ON → OFF | `/usr/bin/systemctl start tessera-web-unitfile@disable-<name>.service` | `/usr/bin/systemctl disable --now <name>.service` |
| restart | `/usr/bin/systemctl start tessera-web-unitfile@restart-<name>.service` | `/usr/bin/systemctl restart <name>.service` |

Enable and disable let `systemctl` reload systemd afterwards (no
`--no-reload`): the units that want a service are read from the links the
command changes, and without the reload a service that was switched off is
brought back by the next start of the unit that wanted it (the EQ engine by
`tesserad`, which `tessera-network` stops and starts on every network change),
and one that was switched on is not started with it. For the portal's own unit,
`disable` and `restart` get `--no-block`, so the answer reaches the browser
before the portal stops; the message of that action is lost with the
portal, and the page comes back without it. A `@` in a service name
(`getty@tty1`) is written `\x40` in the helper's instance, as
`systemd-escape` writes it.

Every action is `POST /speaker/services` with `unit` (the entry's unit, or
the group's units joined with `+`), `action` and the session's `csrf`
token, answered with a redirect to `/speaker#services` and a one-shot
message (success, or the sanitised `systemctl` error, at most 300
characters, naming for a group the units already done; the helper's own
error is in the journal, which the message names: a failed helper start is
reported under the action asked for, e.g. `systemctl enable x.service: Job
for tessera-web-unitfile@enable-x.service failed ...`). Posted by the
page's script (header `X-Tessera-Live: 1`) the same request is answered
with JSON instead, `{"ok": true|false, "text": "<the message>", "unit":
"<key>", "action": "<action>"}`, shown under the row, and the row is
re-read right after. A unit file installed
after PID 1 last looked it up (`not-found` in its cache) may need
`systemctl daemon-reload` before `--now` can start it. For the portal's own
unit the message only says the action was requested: the portal does not
wait for it and its outcome is not known.
The portal stops waiting for a command after 10 s; the action may still
complete. The portal logs every action with the session's short id (never
the cookie), the entry, the action and the result; a refused `unit` value
is logged quoted.

**Confirmation for essential entries.** Disabling or restarting an entry
with `essential = true` (always: the daemon, this portal) answers the first
request with a page that says what the unit is and what happens, and offers
the action and **Cancel**. The action button is the same form again with
`confirm=yes` and the session's CSRF token; only then does anything run.
Enable never asks. With JavaScript the row asks instead, in place, with the
same words (the server renders them into the form's `data-confirm`), and
only the answer posts `confirm=yes`; a request posted in place without it
is refused with 409 and `{"ok": false, "confirm": true, "text": …}`, so the
server asks either way and nothing runs unconfirmed. (The daemon's own
confirmation step for restarting changes uses an impact token from the
daemon; these are host services, so a plain confirmed re-submit is used.)

### Security model

- **No setuid, the portal never runs as root, its unit's hardening
  unchanged.** The portal runs `systemctl` as its own user; `systemctl`
  talks to PID 1 over D-Bus (`AF_UNIX`, which the unit allows), and PID 1
  asks polkit. The unit keeps `NoNewPrivileges=yes`, the empty capability
  bounding set and every other restriction.
- **One polkit action, one verb, the listed helper instances.**
  `tessera-web appliance polkit` prints a rules file from the
  `[[appliance.service]]` entries. For the user `tessera-web` and the action
  `org.freedesktop.systemd1.manage-units` only (PID 1 passes it the unit and
  the verb), it answers yes when the unit is one of the listed helper
  instances `tessera-web-unitfile@enable-<name>.service`,
  `...@disable-<name>.service`, `...@restart-<name>.service` and the verb is
  `start`. One array lookup, nothing else; no wildcard; no other action or
  verb. The portal is never allowed to name a service itself: a grant on
  `<name>.service` would also allow `systemd-run --unit=<name>.service ...`
  as root while that unit is not loaded (not installed, or not yet looked
  up). Install it:

  ```sh
  f=$(mktemp) && tessera-web appliance polkit > "$f" \
    && sudo install -m 0644 -o root -g root "$f" /etc/polkit-1/rules.d/49-tessera-web-appliance.rules \
    && rm -f "$f"
  ```

  Re-run it after changing the service entries (polkit reads the directory
  on change; no restart). Without the rule every action is refused by
  polkit ("interactive authentication required") and the page says so.
- **Why a helper.** `systemctl enable` and `disable` are a different polkit
  action,
  `org.freedesktop.systemd1.manage-unit-files`, for which PID 1 passes no
  details (systemd 257, `src/core/dbus-util.c`): a rule could not tell
  `tesserad` from `debug-shell`, so that action is never granted. Instead
  the package ships the root oneshot template
  `tessera-web-unitfile@.service` (`crates/tessera-web/dist/`), whose
  instance names the verb and the service, and restart goes the same way
  (see above). The portal may only *start* the listed instances; the helper
  (`tessera-web appliance unitfile <instance>`, run with
  `--config /etc/tessera/web.toml`; a portal with another `--config` needs
  a drop-in with the same path, as the template's comment shows) re-reads
  the file, refuses an instance that is malformed or names a service
  outside the configured service allowlist, and then runs
  exactly `systemctl enable|disable --now <name>.service` or
  `systemctl restart <name>.service` as root, exiting with its code. The
  helper unit is hardened as far as a unit that runs `systemctl` allows:
  `CapabilityBoundingSet=CAP_SYS_ADMIN` (the symlinks are written by PID 1
  over D-Bus, but PID 1 trusts a caller without asking polkit only if it
  holds `CAP_SYS_ADMIN`; with none, a non-interactive enable is refused),
  `NoNewPrivileges`, `ProtectSystem`, private network (`AF_UNIX` only),
  the system call filter.
- **The residual.** A process running as `tessera-web` can trigger enable,
  disable or restart of the listed services; nothing else, and nothing
  about any other unit.
- **The allowlist is checked before anything runs**, twice. In the portal:
  a `unit` field that is not a configured entry key is answered 400 and no
  process is started; the key must match exactly (`tesserad`, not
  `tesserad.service`; a group only as a whole, in its order). In the
  helper: the instance is parsed strictly and checked against the file
  again. Unit names are validated when the file is read (service names
  only), so nothing but a service name reaches `systemctl`, the rules file
  or an instance name, and commands never go through a shell.
- **Same session rules as every page**: a session, the CSRF token, the
  same-origin check; the confirmation page for essential entries.

### Configuration

```toml
[appliance]
enabled = true
hostname_label = "speaker"     # optional; default: the host name

[[appliance.service]]
unit = "tesserad"
label = "Network audio"
description = "tesserad · interoperable with Dante®"

[[appliance.service]]
unit = "tessera-web"
label = "This portal"

[[appliance.service]]
unit = "shairport-sync"
label = "AirPlay"

[[appliance.service]]
label = "Bluetooth"
units = ["bluetooth", "bluealsa", "bluealsa-aplay"]

[appliance.camilladsp]           # optional: the EQ engine, see "EQ engine" below
enabled = true
gui_url = "http://{host}:5005/gui/index.html"   # {host}: as the browser reached the portal

[appliance.volume]               # optional: the amplifier's mixer control
card = "amp"

[appliance.rate_follow]          # optional: one sample rate for the speaker chain
enabled = true
bus_hw_params = "/proc/asound/Loopback/pcm0p/sub0/hw_params"
restart = ["shairport-sync"]
```

`[appliance.camilladsp]` (`enabled`, `websocket` (a `ws://` address),
`configs_dir`, `coeffs_dir`, `statefile`, `binary`, `gain_filter`,
`gui_url`, `max_upload_bytes`) and `[appliance.volume]` (`card`,
`control`, `min_db`, `max_db`, `state_file`) are described under "EQ engine",
`[appliance.rate_follow]` under "Rate follower"; the
commented example in `crates/tessera-web/dist/web.toml` lists the
defaults. `tessera-web check` reports whether appliance mode is on and how
many entries it has.

Setting up a device, and checking it (the package installs the helper
template):

```sh
sudoedit /etc/tessera/web.toml                 # [appliance] enabled = true and the entries
sudo tessera-web check
f=$(mktemp) && tessera-web appliance polkit > "$f" \
  && sudo install -m 0644 -o root -g root "$f" /etc/polkit-1/rules.d/49-tessera-web-appliance.rules \
  && rm -f "$f"
sudo systemctl restart tessera-web
systemctl show -p NoNewPrivileges tessera-web                     # stays NoNewPrivileges=yes
sudo runuser -u tessera-web -- systemctl start tessera-web-unitfile@restart-shairport-sync.service   # listed: succeeds
sudo runuser -u tessera-web -- systemctl restart shairport-sync   # direct: refused by polkit (only the helper is allowed)
sudo runuser -u tessera-web -- systemctl start tessera-web-unitfile@disable-shairport-sync.service
systemctl is-enabled shairport-sync                               # disabled; journalctl -u 'tessera-web-unitfile@*' shows the run
sudo runuser -u tessera-web -- systemctl start tessera-web-unitfile@enable-cron.service   # not listed: refused by polkit
sudo runuser -u tessera-web -- systemd-run --unit=tessera-web-unitfile@restart-shairport-sync.service /bin/true   # must fail: the template instance already exists
# That refusal depends on the template file being installed (the instance name is then taken); when the package is
# removed, remove the rules file too, or the grant would allow starting a transient unit of that name.
# The rule itself, without touching anything (expect exit 0, then non-zero):
sudo runuser -u tessera-web -- sh -c 'pkcheck --action-id org.freedesktop.systemd1.manage-units --process $$ \
  --detail unit tessera-web-unitfile@restart-shairport-sync.service --detail verb start; echo $?'
sudo runuser -u tessera-web -- sh -c 'pkcheck --action-id org.freedesktop.systemd1.manage-units --process $$ \
  --detail unit shairport-sync.service --detail verb restart; echo $?'
```

### EQ engine

With `[appliance.camilladsp] enabled = true` the Speaker page controls a
CamillaDSP instance on the same host. The portal itself runs nothing as
root: it talks to the engine over its websocket, edits the engine's files
as its own user (see "Files and permissions") and runs the engine's
binary only to check a configuration. The one privileged step, restarting
the engine when its websocket is down, goes through the same root helper
as the Services card (`tessera-web-unitfile@restart-camilladsp.service`)
and only if `camilladsp` is an allowlisted entry. Changes to the engine's
files are serialised: one upload, activation, deletion or boost edit at a
time.

**The websocket.** `websocket` (default `ws://127.0.0.1:1234`; plain
`ws://`, no TLS) is CamillaDSP's own control socket. The portal sends one
text frame with a JSON command, a bare string for a command without an
argument (`"GetState"`, `"Reload"`, `"GetConfigFilePath"`, `"GetVersion"`,
`"GetVolume"`) or a one-key object with it (`{"SetConfigFilePath":
"/path"}`, `{"SetVolume": -12.0}`), and reads one reply of the form
`{"<Command>": {"result": "Ok", "value": ...}}`. A refusal is another
`result`: an error name (`"InvalidFaderError"`) or a one-key object with the
message (`{"ConfigValidationError": "..."}`), as CamillaDSP 4.1.3 answers;
`"Error"` with the message in `value` (earlier engines) reads the same. The
rate follower also sends `"Stop"`, `"GetConfigJson"` (the running
configuration as a JSON text) and `{"SetConfigJson": "<JSON text>"}`. Every command is its own
short-lived connection: connect, send, read the reply (the engine's pings
are answered meanwhile), close, with 4 s for the whole exchange (1 s for
the reads a page render makes, so a hung engine does not hold every page)
and messages and frames of at most 2 MiB.
The page does not use the engine's volume (`GetVolume`/`SetVolume` exist
in the client for completeness); the Volume card is the amplifier's mixer.

**Playing / Idle.** The hero's ring and word come from `GetState`:
`Running` is **Playing** (a pulsing green ring); `Paused`, `Inactive`,
`Stalled` and `Starting` are **Idle** (grey); no connection or no reply is
**EQ engine off** (grey, the reason in the ring's tooltip). Under it: the
device name, how many of the daemon's subscriptions are receiving and the
daemon's audio session gauge (from its metrics). The chip `EQ <name>` is
the active configuration as the engine reports it (`GetConfigFilePath`);
when the engine cannot be asked, as its state file names it (`statefile`,
default `/opt/camilladsp/statefile.yml`, key `config_path`). It shows the
file name when the path is in `configs_dir`, else the whole path; `none`
when there is no path or no state file; `unknown` when the engine is down
and the state file exists but cannot be read, and then no configuration
is deleted or activated over (fail closed). Whether a listed file is the
active one is decided by comparing the files themselves (device and
inode, else canonical paths), not the strings.

**Volume** (the master card's left pane; only with an `[appliance.volume]`
table; `card` is required, `control` defaults to `Digital`, the range
`min_db`..`max_db` to −60..0). The pane reads the control with
`/usr/bin/amixer -c <card> sget <control>` (the first `[... dB]` value) and
shows it as a large readout and a range input (0.5 dB steps, the filled
part of the track computed by the server). `POST /speaker/volume` with `db`
rounds to 0.5, clamps into the range and runs
`/usr/bin/amixer -c <card> -- sset <control> <db>dB`, then, for a commit,
saves the value. The field `commit` chooses: absent, `1` or `yes` (the plain
form, and the script on release) sets and saves; `0` or `no` (the script
while the slider moves, at most every 150 ms) only sets the control, so the
save happens once per drag; anything else is refused (400). With
JavaScript the readout follows the thumb, the value is heard while
dragging and the release commits; the script's requests are answered with
JSON (`db` is the value as applied, `commit` what was done, `muted` whether the
card's switch is off).

**The playback switch.** A value in dB does not touch the control's hardware
switch (`[on]`/`[off]` in `amixer sget`), and a card can come up with it off (the
distribution's restore service never ran for it): the slider then moves and
nothing is heard. The pane reads the switch with the value; when a playback channel is
`[off]` it says "The output is muted: the card's playback switch is off, so
nothing is heard at any volume" under the slider and offers an **Unmute** button.
`POST /speaker/volume/unmute` (session and token as every post) runs
`/usr/bin/amixer -c <card> -- sset <control> unmute` and nothing else; the saved
volume is not touched. A value set on a muted card is set, and the answer says it
is muted ("Volume −24.0 dB, but the output is muted: switch it on with Unmute").
The portal never unmutes by itself: a muted card may have been muted on purpose. The
start restore logs a warning when the card it set is muted.

The portal owns the setting; it does not rely on `alsactl store`, which
needs root and which the distribution's restore service does not run for
every card (a USB or HAT amplifier came back at its default level after a
reboot). A commit writes `{"card":…,"control":…,"db":…}` atomically
(a temporary file, synced, renamed) to `state_file`, by default
`/var/lib/tessera-web/volume.json` in the unit's `StateDirectory=tessera-web`
(0700, owned by the portal's user, removed by a package purge). A preview is
not saved: only the value the operator settled on survives. At start the
portal sets the saved value again with `amixer`, clamped into the current
range, as soon as the card answers; the card may enumerate after the portal
(USB audio), so it looks every 5 seconds for up to 10 minutes and logs once
that it is waiting. Anyone who posts a volume in the meantime keeps their
value and the restore stands down. A value saved for another card or control
is ignored. The unit starts after `alsa-restore.service` where the
distribution restores mixer state at boot, so the saved value is applied last.
If the value cannot be saved (a manual install without the state directory),
the commit says so: the volume is set but will not survive a restart. The
signal boost needs no such file: it is the `gain` line of the engine's active
configuration, which persists with it.

**The card is named, not numbered.** Use the card's name (`aplay -l` lists
`card 1: Dummy [Dummy]`: the name is the word in brackets) in
`[appliance.volume] card`; the installer does. `amixer` finds a card by its
name at every call, so a card that takes another index after a reboot is still
found. The unit therefore lets the portal open the control device of every
card index (see "Files and permissions"). If `amixer` cannot find or open the
card, the pane says "The sound card `<name>` was not found" and lists the
causes: it is not connected, `card` does not name it, or the unit may not
open its control device.

**A card that appears after the portal started** (a USB DAC plugged in later, a
HAT that enumerates late, a card that takes another index while the system
runs) is out of reach of the running unit: systemd binds `/dev/snd` and turns
every `DeviceAllow=` path into a device number when the unit starts, and a node
that does not exist then is dropped. The package therefore ships a udev rule
(`/usr/lib/udev/rules.d/60-tessera-web-sound.rules`) that starts
`tessera-web-sound.service` when a sound card's control device is added. The
oneshot waits 5 seconds (one card's events, and a flapping one's, become one
restart, clear of the portal's start limit), then runs
`systemctl try-restart tessera-web.service`; its `ExecCondition` skips it when no
drop-in of `/etc/systemd/system/tessera-web.service.d` lists a `/dev/snd/` device,
that is, when the portal has no Volume pane. The portal's sessions are in memory,
so the operator signs in again after such a restart; nothing else is lost (the
saved volume is set again by the restore). Alternatives weighed, and why not:
`DeviceAllow=char-alsa rw` would open the PCM, sequencer and timer nodes as well
(the portal could play and record); `systemctl set-property --runtime
tessera-web.service DevicePolicy=closed` makes systemd resolve the paths again
without a restart (checked on systemd 257) but cannot create the `/dev/snd` bind
mount of a unit that started when no card existed. Hardware check: unplug and
plug a USB sound card while the portal runs and watch
`journalctl -u tessera-web-sound -u tessera-web`.

**Signal boost** (the master card's right pane). The page reads the active
configuration file and shows the `gain` of the Gain filter named by
`gain_filter` (default `dashboard_signal_boost`;
`filters.<name>.parameters.gain`), 0..+30 dB in 0.5 steps; above +12 dB the
pane is shown in the warning colour with a note that most sources clip.
Without an active configuration or that filter the pane says so and has no
slider. `POST /speaker/boost` with `db` edits exactly that line (the file
is not re-serialised: comments, order and formatting stay; a comment after
the value stays), writes the file atomically and asks the engine to
`Reload`. Every post rewrites the file and reloads the engine, so the
script posts only on release (and coalesces quick keyboard steps), never
while the slider moves, and shows "applying…" until the engine has
answered; the JSON answer carries `db` and `hot` (above the threshold).
When the websocket is not reachable the portal restarts the engine's unit
instead, but only if `camilladsp` is one of the `[[appliance.service]]`
entries (a start of the root helper `tessera-web-unitfile@restart-camilladsp.service`, as the Services card does); otherwise the
message says **saved; restart the EQ engine to apply it**. A configuration
without that filter is reported and left as it is.

**EQ configs and Filter files.** The card lists the regular files of
`configs_dir` (default `/opt/camilladsp/configs`; `.yml`, `.yaml`) and
`coeffs_dir` (`/opt/camilladsp/coeffs`; `.wav`, `.txt`, `.raw`, `.dbl`,
`.pcm`) whose names are accepted (below), with their size; the active
configuration carries an **active** tag and has neither Activate nor
Delete. Actions:

| Press | Does |
|-------|------|
| Upload (`POST /speaker/eq/upload`, multipart: `csrf`, `kind`, `file`, in that order) | stores the file under its own name, **a new name only** (an existing file, or link, of that name is refused: delete it first), at most `max_upload_bytes` (default 32 MiB; the route's body limit is that plus 64 KiB for the framing, a larger declared `Content-Length` is refused after the token check without reading the file, a longer body while reading; nothing is written either way). The token field is read first, bounded to 256 bytes; a file part before it is refused unread; one file per request. A configuration is only stored, not activated |
| Activate (`POST /speaker/eq/activate`, `name`) | runs `<binary> -c <path>` (the engine's check; exit code 0 required, else its message is shown and nothing changes), then `SetConfigFilePath` (the engine persists the path in its own state file) and `Reload` over the websocket; a refused reload puts the previous path back. Only when the websocket is not reachable **and** `systemctl is-active camilladsp.service` says the unit is `inactive`, `failed` or unknown (`active`, `activating`, `deactivating`, `reloading` all count as running: a running engine that merely does not answer still owns the file and would write it back) does the portal write `config_path` into the state file itself (in place, see below) and start the engine's unit through the helper if it is allowlisted; a running but silent engine is refused with "restart it from the Services card and try again"; a stopped engine whose unit is not allowlisted gets the file written and the message that it must be restarted. The lock, the check and the whole switch run on a task of their own holding the lock's owned guard, so a browser that goes away mid-way neither cancels the switch nor lets another change interleave with it |
| Delete (`POST /speaker/eq/delete`, `kind`, `name`) | a confirmation page first (`confirm=yes` on the re-submit); the active configuration is refused outright, and no configuration is deleted while the active one is unknown |
| Download (`GET /speaker/eq/download/{kind}/{name}`) | the file as an attachment: `Content-Type: application/octet-stream`, `Content-Disposition: attachment; filename="<name>"`, `X-Content-Type-Options: nosniff` |

Every form redirects to `/speaker#eq` with a one-shot message; every
action is logged with the session's short id and the name.

**Names and paths.** A name must match `^[A-Za-z0-9._-]{1,64}$`, not start
with `.`, and end in one of the kind's extensions. Paths are only ever
`<dir>/<name>` for a name that passed: `..`, separators, NUL, spaces and
non-ASCII are refused (400) before anything is read or written, and
hidden files, other extensions and symbolic links in the directories are
neither listed, served, edited, activated nor deleted (downloads open the
file without following a link and check that it is a regular file; the
boost edits only a regular file inside `configs_dir`). Uploaded and edited
files are written to a temporary file in the same directory
(`.<name>.tmp-<pid>-<n>`), synced and renamed over the target; a file
being replaced keeps its permission bits and group, a new one gets mode
0664 (set explicitly, whatever the unit's `UMask`) and the directory's
group. The state file is the exception: it is the engine's own (the
engine rewrites it for its volume and mute, so two writers would race),
the portal touches it only on the restart fallback, and then in place
(open without creating, write the new content, truncate to its length,
sync: the inode stays, there is no moment with an empty file, and no
chmod or chown is attempted on a file the engine's user owns; only a
state file the portal has to create gets 0664 and the directory's
group), because the drop-in opens that single file and a rename next to
it is not possible. A state file that exists but cannot be read stops the
fallback.

**Files and permissions.** The unit's `ProtectSystem=strict` leaves the
portal no writable path but `/run/tessera`, the rate follower's
`/var/lib/tessera-rate` and its own state directory
`/var/lib/tessera-web`, and its `PrivateDevices=yes` hides the sound card
from `amixer`; the EQ pages need the two directories and the state file, the
Volume card the card's ALSA control device. The package ships a drop-in,
`crates/tessera-web/dist/appliance-paths.conf`: the `ReadWritePaths=` lines
(each with the `-` prefix, so a missing path does not stop the portal) and,
for the Volume card, device lines that keep `PrivateDevices=yes`:
`BindPaths=/dev/snd` (the host's `/dev/snd` appears in the private `/dev`),
`DeviceAllow=/dev/snd/controlC<n> rw` for every `n` from 0 to 31 (the cgroup
opens exactly those nodes: the control devices of any card, nothing else under
`/dev/snd`, so no PCM, sequencer or timer device is reachable) and
`SupplementaryGroups=audio` (the group owning the nodes). The control devices
are listed for every index, not pinned to the card's current one, because
`amixer` finds a card by its name by opening the control devices in turn and a
unit that may open one only fails with "Invalid card number" as soon as the
card has another index (the card order changes at a reboot when modules load
in another order). `DeviceAllow` takes no wildcard in a path and ignores a node
that does not exist, so 32 lines (the kernel's limit on cards) cost nothing.
`DeviceAllow=char-alsa rw` would be shorter but exposes every ALSA device
(PCM capture and playback included) to the portal; `PrivateDevices=no`
would also drop the implied `CapabilityBoundingSet=~CAP_MKNOD
CAP_SYS_RAWIO` and `SystemCallFilter=~@raw-io`, which would then have to
be restated. Verified on systemd 257 in a transient unit: a listed node
opens, one that is not listed is refused with EPERM, a listed node that does
not exist does not stop the unit. Drop the device lines without an
`[appliance.volume]` table:

```sh
sudo install -d /etc/systemd/system/tessera-web.service.d
sudo install -m 0644 /usr/share/doc/tessera-web/examples/appliance-paths.conf \
  /etc/systemd/system/tessera-web.service.d/appliance-paths.conf   # adjust the paths to web.toml
sudo systemctl daemon-reload && sudo systemctl restart tessera-web
```

That opens the paths inside the sandbox; the file system still decides.
Give the group that owns the engine's files write access and add the
portal's user to it (the group is whatever the engine's installation
uses; `audio` here as an example):

```sh
sudo chgrp -R audio /opt/camilladsp/configs /opt/camilladsp/coeffs /opt/camilladsp/statefile.yml
sudo chmod 2775 /opt/camilladsp/configs /opt/camilladsp/coeffs   # setgid: new files take the group
sudo chmod 0664 /opt/camilladsp/configs/* /opt/camilladsp/statefile.yml
sudo usermod -aG audio tessera-web && sudo systemctl restart tessera-web
sudo runuser -u tessera-web -- touch /opt/camilladsp/configs/.probe \
  && sudo rm /opt/camilladsp/configs/.probe                          # writable: no error
```

### Rate follower

With `[appliance.rate_follow] enabled = true` (appliance mode only; off by
default) the tessera device's sample rate is the speaker's only rate: a shared
ALSA bus, its players and the EQ engine follow it, and the engine never keeps a
rate of its own. The setup is in
[raspberry-pi-speaker.md](raspberry-pi-speaker.md#shared-sources-and-one-sample-rate).

| Key | Default | Meaning |
|-----|---------|---------|
| `enabled` | `false` | Run the follower. |
| `device` | the daemon's only device | The tessera device key to follow. With several devices and no key the follower does nothing and says why. |
| `rate_file` | `/var/lib/tessera-rate/alsa.conf` | The ALSA snippet the bus reads its rate from, written as exactly `tessera.rate <Hz>` (a temporary file renamed over it, mode 0644: every ALSA client reads it). The package creates the directory (0755, owned by `tessera-web`) and seeds the file with 48000; the unit opens it (`ReadWritePaths=-/var/lib/tessera-rate`). The state directory `/var/lib/tessera-web` is 0700, so the file cannot live there. |
| `bus_hw_params` | none | The bus's `hw_params` in `/proc/asound` (`rate: 48000 (48000/1)` while open, `closed` otherwise). Without it the bus is not watched. |
| `restart` | `[]` | Bus clients restarted, if active, while the bus runs at another rate. Each must be an `[[appliance.service]]` unit (a member of a group counts: it is restarted alone through `tessera-web-unitfile@restart-<unit>.service`, which the generated polkit rule already allows); `tesserad` and `tessera-web` are refused. |

Every 2 s, and at start, it reads the device's rate from the daemon, the rate
file, the bus, the active configuration file and the running engine
(`GetConfigJson`), and acts:

| Observed | Done |
|----------|------|
| the rate file does not hold the rate | it is written |
| the bus is open at another rate | `Stop` (the engine's capture closes, so the cable can change rate), the `restart` units that are active are restarted, and the engine is reloaded below |
| the active file's `samplerate`, or a set `capture_samplerate`, differs | those scalars are set (nothing else in the file changes, as with the boost) and the engine reloaded (`SetConfigFilePath` first after a `Stop`) |
| only the running engine differs (a configuration applied in the GUI, not saved) | `SetConfigJson` with the running configuration, its rates corrected |
| the engine stopped with an error after the follower acted | `Reload` again |

A new rate is acted on at once; while the chain does not settle (a card that
refuses the rate, a player outside `restart` holding the bus) the next attempt
waits 2 s, doubling to 60 s. Actions are logged when they first run, failures
when they change. The engine's files are changed with the same lock as the
boost, and only a regular file in `configs_dir`; another file is reported, not
edited. Without the websocket, an allowlisted engine unit that runs is
restarted; a stopped engine is left stopped and reads the file when it starts.
Without `[appliance.camilladsp] enabled` only the rate file and the bus follow.
While the follower runs, the capture-rate notice offers no button.

---

## Limits

- No built-in TLS: use a tunnel or a proxy.
- No live push in the pages; they refresh. The API has Server-Sent Events.
- Bridges and keepalive counters are only in the JSON.
- One operator password, no user accounts; commands are attributed to the
  portal's uid in the daemon.
- The routing matrix reads at most four receivers per page load and offers
  changes to another device's receiver only when its inputs fit on one page;
  "receiving" is packet activity, not verified audio.
