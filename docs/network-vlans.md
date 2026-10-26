# Standalone management and audio VLANs

The Tessera portal's **Network** page can define named networks on the wired
management port. A blank tag means untagged (the switch port's native VLAN);
tagged VLAN IDs are 1–4094. Configure the switch trunk first. Tessera lists
configured appliance networks and cannot discover every VLAN on the switch.

Choose **Management** for the web interface and **Audio network** for discovery,
controller traffic, audio and PTP. They can use the same network or separate
VLANs. A typical arrangement keeps management untagged and selects a tagged
audio network. There is no automatic controller subscription setup.

Each network uses DHCP or one static IPv4 address with prefix 8–30. Only
management can supply a default gateway and DNS. Audio DHCP ignores router,
DNS, NTP and hostname options. Use non-overlapping IPv4 subnets. Configure
static settings inside each network's expandable section. DHCP changes can
require discovering the new lease through the DHCP server.

Management can also carry up to six **search domains** (host names such as
`lan.example`, comma separated, kept in the order and case entered). They apply
to DHCP and static management alike and are the configured list, not the
domains a DHCP lease hands out. Only the management network may have them;
when moving management, clear the old network's list in the same draft. A
trailing dot, a `~` routing prefix or a wildcard is refused. An empty list is
omitted from the stored document and from the OS configuration.

The page stores edits as a session draft. Save the networks and role assignments,
then choose **Apply network settings**. Review the complete candidate before
starting the change. Assigned networks cannot be deleted without replacements;
removing a network from a draft does not immediately change the appliance.

Applying may interrupt network access and audio. The appliance journals the
previous configuration and arms its own recovery timers. Once activation is
ready, the confirmation page shows 60 seconds and the new management address.
Open that address directly, then click **Keep changes**. Page loads and status
polls never confirm. **Revert**, failure or expiry restores the previous settings.
The recovery token authorizes only that pending network transaction and works
without the old address's login cookie. Proxy/forwarded headers cannot prove
that the new management address works.

## Confirmation policy

The standalone default remains `require_management_address`: Keep must arrive
at the new management address on the actual HTTP socket. An administrator can
instead allow the pending link to work from any reachable address:

```toml
# /etc/tessera/web.toml
[network]
confirm = "token_only"
```

Set `confirm = "require_management_address"` to explicitly retain the default.
Both policies require the pending change's 64-character lowercase hexadecimal
token, a ready change and an unexpired window. Page loads never confirm.
`token_only` does not alter the management firewall or make an otherwise
unreachable portal address reachable.

The setting lives alongside the listener configuration that `tessera-network`
already reads. It is administrator intent, outside the browser-editable profiles
document. The helper validates the owner-controlled file and enforces the
policy captured when staging the transaction; changing the setting during a
pending change cannot weaken its Keep policy. Neither browser commands nor the
audio daemon write it. Keep the file owned by root without group/world write permission to
lock this choice. The helper reports the effective policy to the portal.

Confirmation refusals carry a stable kind as well as human-readable text:
`not_pending` and `expired` (HTTP 410), `still_applying` and `wrong_address`
(409), `token_mismatch` (403), `rate_limited` (429), or `invalid` (400) for a
malformed token. Only token mismatches and unclassified helper failures count
toward the per-client limit of five failures per minute. Early Keep presses,
ended transactions and address-policy refusals never count.

A Revert pressed while the apply worker still holds the helper's mutation lock
waits up to five seconds for it. If the worker is still busy it is answered
`still_applying` (409, "The network change is still being applied or
recovered. Try again in a moment."), the same retryable refusal as an early
Keep, never an internal error. The helper service allows `AF_PACKET` for the
`arping` duplicate-address check that a Revert, a recovery and the boot recovery
run for a static address.

The existing token-scoped pending/keep/revert routes also accept
`Accept: application/json`. Refusals use the counterpart envelope
`{"error": <kind>, "message": "..."}`; the human message remains separate
from the typed kind. Older helper replies without a kind remain readable and
their failures are conservatively counted as unclassified. Browser failures
show the typed reason in the existing page frame. Keep attempts are separate
from login limits. Pending, Keep and Revert share one failure budget, including
their JSON aliases. Five counted failures return their normal error; the next
token request from that address returns 429 until a failure ages out of the
rolling 60-second window. IPv4-mapped IPv6 uses the same client budget. Keep
stays enabled while a change is pending; the helper returns a typed reason if
it is still applying or the request arrived through the wrong address.
Unclassified helper and portal failures use `internal` (HTTP 500).

Clients can use `POST /api/net-pending`, `POST /api/net-confirm` and
`POST /api/net-revert` with JSON `{"token":"<transaction token>"}`. These aliases
always return JSON, use the same transaction scope as the forms, and require
neither the previous origin's cookie nor its CSRF token. Confirm also accepts
the compatibility field `"profiles":true`. A poll never keeps a change. Tokens
belong in request bodies and must not be placed in diagnostic logs.

NDIMon-R uses `token_only`. Standalone Tessera deliberately keeps the stricter
default; hosted networking continues to be owned by NDIMon.

Install the matching `tessera` and `tessera-web` packages. The web package ships
the Rust network helper, its recovery units and dependencies. The portal must
listen on `0.0.0.0:<port>` in `/etc/tessera/web.toml` for direct new-address
confirmation. Its configured port is then filtered to management. Keep ordinary
password/session protection enabled. TLS proxies need their own management-only
binding; proxy administration, SSH and switch configuration are outside this
feature. Uninstallation preserves committed network configuration.

The engine has adapters for native systemd-networkd, Netplan/networkd,
NetworkManager (including Netplan's NetworkManager renderer), and ifupdown. The
first two are the backends of earlier releases; the NetworkManager and ifupdown
adapters and the ownership handoff below are new in 0.3.0. They have been
exercised with rooted command fixtures and in isolated test guests, but they have
not been accepted on live hosts, and fixture coverage alone does not certify a
backend for deployment: try them first on a machine you can reach otherwise.
The adapter follows from what manages the wired port: NetworkManager when it
owns the port, otherwise networkd (Netplan when Netplan generated its
configuration), otherwise ifupdown. A port that NetworkManager and networkd both
claim is refused (`ambiguous_backend`), as is one that no supported manager
claims (`unknown_backend`); nothing is guessed.

On first use the helper adopts one active wired Ethernet parent. When the
default route does not identify one unambiguous port, an administrator must
set `[network] interface = "eth0"` in `web.toml` or a file in `/etc/tessera/web.d/`.
A speaker with Ethernet and Wi-Fi has a default route on both, and the Network page
then says:

> Network configuration refused: ambiguous_management_port. The default routes use more
> than one port (eth0, wlan0), so the management port cannot be chosen. Name the wired
> management port: set `interface = "eth0"` under `[network]` in /etc/tessera/web.toml
> or a file in /etc/tessera/web.d/ (wired candidates: eth0).

The refusals about the port (`no_wired_interface`, `ambiguous_management_port`,
`wired_port_required`, `unsupported_link_kind`, `attached_interfaces`,
`unknown_backend`) all end with that remedy and the candidates found; the typed
`what` is unchanged. Wi-Fi, bonds, bridges, enslaved
ports, foreign VLANs (including dormant NetworkManager connections), explicit
IPv6 configuration, mixed DHCP/static overrides, multiple addresses and
custom/policy routes are refused because the profile cannot preserve them.
Unsupported configurations remain under administrator control.

IPv6 privacy-extension settings are not IPv6 configuration and never make the
adoption refuse: Netplan `ipv6-privacy`, networkd `IPv6PrivacyExtensions=`,
NetworkManager `ipv6.ip6-privacy` and ifupdown `privext`. The managed profiles
have IPv6 off, so the setting has no effect there: Netplan's source definition
loses the key with the other IP keys, the files the helper writes carry none, and
NetworkManager resets the level when the profile's IPv6 goes to `disabled`. The
before-image is recorded (the source file's text, the connection's `ipv6.ip6-privacy`),
and Revert, expiry or a failed apply put the exact level back. Stock Armbian Netplan
(`ipv6-privacy: true`) and Raspberry Pi OS profiles (`ip6-privacy`) therefore adopt.

A bare DHCPv6 switch is a distribution default as well, and adopts: Netplan `dhcp6: true`
(stock Armbian sets it on every Ethernet definition) and networkd `DHCP=yes` or `DHCP=ipv6`
(what Netplan generates for it). The IPv4 side is judged as before (`DHCP=ipv6` with one static
address is a static IPv4 network; `DHCP=yes` with an address is `mixed_dhcp_static`). Managed
output turns it off: the definitions the helper writes say `dhcp6: false` and the pin says
`DHCP=ipv4`; Netplan's source definition loses the key with the other IP keys, and the
before-image, restored by Revert, expiry or a failed apply, brings it back. Anything more is
still IPv6 configuration and refuses as `ipv6_configuration`: `dhcp6-overrides` or a `[DHCPv6]`
section with settings, `gateway6`, `accept-ra: true` or `IPv6AcceptRA=yes`, IPv6 addresses or
DNS servers.

NetworkManager's `ipv4.route-metric -1` is its own default for an Ethernet port, which is
**100**; every stock profile has it. Adoption accepts it after checking the default route that
NetworkManager installed on the port: if that route has another metric (a connection default set
in `conf.d`, for example), adoption refuses, naming the connection and the property
(`unrepresentable_configuration (NetworkManager connection 'netplan-eth0': ipv4.route-metric is -1
but the default route of eth0 has metric 600, not NetworkManager's Ethernet default 100)`). Only
the route of the managed port counts, so the Wi-Fi route of a dual-homed speaker does not matter
once the port is named. A number is the profile's own metric and is kept; anything else refuses
by name. Revert puts `-1` back.

Adoption keeps the parent's configured search domains, so a stock cloud-image
Netplan file (a MAC `match`, `set-name`, one static address, a default route and
`nameservers: {addresses, search}`) or a Debian-installer `dns-search` line is
adopted rather than refused. They are read from the same key each backend
renders: Netplan `nameservers.search`, networkd `Domains=` in `[Network]`
(repeated lines append, an empty one resets), NetworkManager `ipv4.dns-search`
(the connection's configured property, never `IP4.DOMAIN` lease data) and
ifupdown `dns-search` (one line). More than six domains, an invalid name
(including `~` routing domains) or search domains on a definition other than
the management one are refused as `unrepresentable_configuration`; the message
names the file or connection and the key, not the value.

networkd and Netplan/networkd use `00-tessera-<interface>.network` pins that
sort before distribution defaults. The engine reports competing files and
selectors as drift, preserves the inherited route metric, and treats failure
to select a pin as an apply failure requiring recovery. Netplan source edits
clear only the adopted parent's IPv4 keys and retain unrelated definitions.
NetworkManager adopts its wired parent by UUID and creates explicitly bound,
autoconnecting `tessera-vlan-<interface>` connections. The helper edits no Netplan
file or keyfile itself; `nmcli` saves the parent's changes. Changed properties and
original activation state are snapshotted without credentials. So are the files that
persist the parent: the Netplan YAML and keyfiles that name its UUID, read from
`/etc/netplan`, `/run/netplan` and the two `NetworkManager/system-connections`
directories. `nmcli` rewrites them (a Netplan profile gains passthrough keys, a keyfile
is normalized), so a failed apply, a Revert or an expiry puts them back byte for byte,
removes a `90-NM-<uuid>.yaml` the apply created, reloads NetworkManager and compares
them again; a file that does not match fails the recovery
(`profile_file_restore_mismatch`). NetworkManager's own writers change a profile after
a reload without being asked: its Netplan writer puts `ipv6.ip6-privacy: "-1"` (the
default level) into the passthrough keys within a second, and every activation refreshes
`connection.timestamp`. The comparison leaves those two lines (and the `#Netplan:` marker
a keyfile has above them) out on both sides; a comment, an edit, or any other line is
still a difference. The same comparison decides whether a profile changed between the
preview and the apply. A recovery also waits up to ten seconds for NetworkManager to
settle before it judges the properties and the activation of the connections, and the
error names the connection, each property and its two values, for example
`verify_backend: connection_restore_mismatch (connection 'netplan-eth0':
ipv4.dns-search is 'lab.example', was '')`. A change to one of these files between the preview
and the apply is refused before anything is touched. A parent profile that holds a
credential (an `802-1x` section, a password or a key) is refused as
`unsafe_configuration` rather than copied into the journal. Removed owned connections
are deleted only after Keep commits the candidate.

A Netplan-generated parent, the Raspberry Pi OS default (`netplan-eth0`), is supported.
NetworkManager's Netplan plugin stores it as a definition that matches every device
(`match: {}`) and keeps no `connection.interface-name`: it acknowledges the property
and reads it back empty. For such an unbound parent the helper does not write the
binding and checks instead that the port's active connection is the parent. A native
keyfile profile is still bound to the port and the binding is checked. When a check
fails the error names the connection and the property, for example
`verify_backend: foreign_configuration_selected (connection 'netplan-eth0':
ipv4.route-metric)`, or `device eth0 did not select connection 'netplan-eth0'`.

ifupdown requires the sole parent stanza to be in
`/etc/network/interfaces.d/tessera-<interface>`, explicitly handed off with
`# Handed off to tessera-network.` (or already managed by Tessera). Move a
foreign or main-file stanza there explicitly, preserve its activation line,
and remove its old definition before adoption. The include tree must actually
load both this file and `/etc/network/interfaces.d/tessera-vlans`. Duplicate
stanzas, custom hooks and unsupported include/activation patterns are refused.
DHCP requires verified ISC dhclient selection; other clients are refused.
The helper prepares DHCP links without waiting for a lease, then starts the
proven ISC client with its native nowait option. Required management/audio
leases still gate activation; optional or restored offline media DHCP does not
block recovery of management and static networks.
The engine renders its option-suppression enter-hook as a backend artifact,
and snapshots/restores it with the interface files. Revert force-deconfigures
the attempted candidate before restoring the old stanzas.

Hardware timestamping through VLAN interfaces still
requires hardware validation; use software PTP unless the NIC/VLAN path supports the required hardware timestamps.

When Tessera is installed with NDIMon, this page reports **Networking managed
by NDIMon**. Define networks and management there, and choose audio inside its
Tessera plugin. Standalone controls do not overwrite NDIMon's configuration.

Removing or purging the packages leaves the committed network configuration in
place (the Netplan or networkd files, the VLAN links they define and
`/etc/modules-load.d/tessera-8021q.conf`): the host keeps its addressing and its
management connection. The firewall table `inet tessera_network` does not stay; it is
deleted with the helper, so the host does not keep rejecting forwarding and the portal
port after Tessera is gone.

The helper's state lives in `/var/lib/tessera-network`, owned by root:root
with mode 0700. Private journal, lock and JSON records require root:root 0600
and one hard link. Unsafe existing ownership or permissions are refused; the
helper does not silently repair or discard transaction records. Its owned
`inet tessera_network` firewall table protects management, prevents forwarding
between configured networks and pins audio service sockets to the audio network.
Protecting management means rejecting the portal's port and TCP port 5005, the
speaker's EQ engine GUI (`tessera-dsp`, [dsp.md](dsp.md)), on every link except
management and loopback: that GUI has no login, so a second interface never reaches it.
It does not modify unrelated firewall tables. The daemon's existing locked
interface configuration provides an additional binding guard.

## When a recovery cannot finish

Expiry, an explicit Revert and a failed apply all go back to the previous configuration
and check that they got there. If the check fails, the change is not closed: the Network
page shows the recovery as pending, Keep is unavailable, and the helper tries again every
ten seconds on its own, independent of the portal.

* **The speaker plays meanwhile.** The recovery stopped `tesserad` (and with it the EQ
  engine and the controller) to change the network under them. When the recovery does not
  verify, the helper starts `tesserad` again at once, so the engine and the controller
  follow; it does not stop it a second time for a retry. The pending recovery is about
  the network settings, not the audio.
* **An explicit Revert that does not verify** answers `apply_failed` with `step` `recover`
  and the detail in `reason` (HTTP 409 from the portal, not 500), and the helper's log
  (`journalctl -t tessera-network`: a revert, a retry and the expiry timer run in their
  own `tessera-network-*` units, so `-u tessera-network` does not show them) says why.
* **An administrator can accept the network as it is.** If the cause is something the
  helper cannot settle (a profile edited by hand, a connection that does not take its old
  properties), run on the speaker as root:

  ```
  tessera-network accept-current          # reports the reason and the files that differ
  tessera-network accept-current --yes    # keeps the network as it is
  ```

  With `--yes` the helper puts its firewall table back as it was before the change, closes
  the pending recovery, starts the audio services and answers the old token with "the
  administrator kept the network". It does not undo anything: check the addresses and the
  connections (`ip address`, `nmcli connection show`) and, if a VLAN connection of the
  change is still there, remove it with `nmcli connection delete`. An ownership handoff
  cannot be closed this way.

## Versioned documents and ownership handoff

The helper socket keeps API 1 for legacy numeric profile revisions up to
9007199254740991. API 2 uses the common semantic document with canonical decimal
string revisions across the full unsigned 64-bit range. Preview/apply commands
carry a document; status and token-command replies return that document without
a numeric profile view. Stale candidates receive `revision_conflict` with the
current revision as a string. The portal uses API 2 and never rounds revisions
through a JavaScript number. A candidate's confirmation policy must match the
administrator policy captured in its activation plan; the document cannot
change that policy.

Root can transfer ownership explicitly between Tessera and NDIMon-R using a
schema-1 exchange of offer, receipt and artifact files. Each engine keeps its own
code and private journal. These operations use the existing root helper and
control socket; the portal's layout and routes are unchanged.

To release standalone Tessera networking:

```sh
sudo tessera-network handoff prepare ndimon
sudo tessera-network handoff status
# Until the receiver reserves its commit:
sudo tessera-network handoff cancel <id>
```

Prepare requires committed networking and no pending/applying/recovery
transaction. It returns the same document while quiesced. The offer is
`/var/lib/tessera-network/handoff/<id>.json`, root:root 0600 in a 0700 directory.
Every other network stage refuses with `locked {field:"network",by:"handoff"}`
until cancellation or transfer recovery. Status remains readable. Prepare
alone changes no addresses, backend configuration, service or firewall table.

To receive networking released by NDIMon-R:

```sh
sudo tessera-network handoff import ndimon <id>
# Review files, tables, stop_services, restart_services and impact.token first.
sudo tessera-network handoff commit ndimon <id> <impact.token>
```

Import changes nothing. Commit recomputes the plan; a changed impact requires a
new review. `--yes` in place of the impact token provides the contract's blind
confirmation for root scripts. Neither form keeps the resulting transaction.
The source's addresses, VLANs, management and audio attachment are preserved.
A separate NDI attachment is unrepresentable by standalone Tessera and is
refused; NDIMon-R's NDI role must share its audio attachment. Its legacy
`tessera` runtime binding corresponds to the semantic `audio` assignment.
NetworkManager VLAN UUIDs and the adopted parent UUID are retained. Artifact
hash drift aborts before configuration changes; arbitrary paths and incomplete
offers are refused. The receiver uses Tessera's administrator confirmation
policy, captured with the journal, including the default address requirement.

Keep or Revert uses the same API-2 token commands as an ordinary network
transaction. Root can submit one JSON line through private stdin, keeping the
secret token out of process arguments and shell history:

```json
{"api":2,"command":{"kind":"confirm","token":"<transaction token>","local_address":"192.0.2.10"}}
{"api":2,"command":{"kind":"revert","token":"<transaction token>"}}
```

Feed the chosen line privately to `sudo tessera-network request`, or send it to
the existing protected `/run/tessera-network/control.sock`. Handoff socket
commands use API 2 with kinds `handoff_prepare` (`receiver`), `handoff_cancel`
(`id`), `handoff_import` (`product`, `id`), `handoff_commit` (`product`, `id`,
`expect_impact`, optional `confirm`) and `handoff_status`. Refusals include the
typed counterpart `error`, `message` and `http_status` for an HTTP adapter;
no new web route is installed.

The independent recovery timer restores source files, table and markers on
Revert, failure or expiry, and removes the source quiesce marker. A kept receive
writes the matching source `.done` and records its receipt. On the releasing
engine's next request/start it archives private records under
`/var/lib/tessera-network/archive/<id>/`, clears quiesce and reports external
ownership. A lone or mismatched `.done` is ignored and reported by handoff
status. Never simulate a transfer by copying journals or editing owner markers.
Real cross-product backend certification remains pending; `tested_with_ndimon`
in the [integration contract](integration-contract.json) is unchanged.
