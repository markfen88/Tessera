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

Install the matching `tessera` and `tessera-web` packages. The web package ships
the Rust network helper, its recovery units and dependencies. The portal must
listen on `0.0.0.0:<port>` in `/etc/tessera/web.toml` for direct new-address
confirmation. Its configured port is then filtered to management. Keep ordinary
password/session protection enabled. TLS proxies need their own management-only
binding; proxy administration, SSH and switch configuration are outside this
feature. Uninstallation preserves committed network configuration.

Native systemd-networkd and Netplan with its networkd renderer are supported.
The helper adopts the active wired configuration on first use. It refuses
mixed DHCP/static overrides, multiple addresses, custom/policy routes and
existing unowned VLANs instead of silently losing those settings. Other
backends remain read-only. Hardware timestamping through VLAN interfaces still
requires hardware validation; use software PTP unless the NIC/VLAN path supports the required hardware timestamps.

When Tessera is installed with NDIMon, this page reports **Networking managed
by NDIMon**. Define networks and management there, and choose audio inside its
Tessera plugin. Standalone controls do not overwrite NDIMon's configuration.

The helper's state lives in root-only `/var/lib/tessera-network`. Its owned
`inet tessera_network` firewall table protects management, prevents forwarding
between configured networks and pins audio service sockets to the audio network.
It does not modify unrelated firewall tables. The daemon's existing locked
interface configuration provides an additional binding guard.
