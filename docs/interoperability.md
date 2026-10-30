# Interoperability and compatibility

Tessera implements a PTPv1 virtual media clock, configurable static UDP media
flows and, with the opt-in native device role, the device side of the vendor's
control protocols. The project targets interoperability with Dante® media
networks; it is not vendor-certified, affiliated with or endorsed by Audinate.

| Capability | Status in 0.5.2 |
|---|---|
| PTPv1 follower/leader and holdover | Implemented; drives media time, never the host's wall clock |
| PTPv1 election with Preferred Leader | Implemented: lower MAC decides between preferred leaders, stratum 0x2A to 0x2D, Sync every 250 ms, warning on a rival preferred leader; provisional values |
| Configured RX/TX UDP flows | Implemented; peers must agree on rate, encoding, channels, port and packet geometry |
| Local ALSA/PipeWire and optional hardware bridge | Implemented; installed backend/build features must match configuration |
| CamillaDSP equaliser stage for a network speaker | Implemented (`tessera-dsp`, [dsp.md](dsp.md)); CamillaDSP's resampler is the one clock crossing between the media clock and the sound card |
| Controller compatibility runtime (packaged, enabled with the daemon) | Implemented, the default; serves the first logical device's base control ports and submits flows and settings as typed commands; provisional wire values |
| Native device role in `tesserad` ([below](#native-device-role)) | Implemented, opt-in; first logical device only; replaces the compatibility runtime on that host |
| Receive flows requested from saved subscriptions | Implemented with the native device role: one flow per input, make before break, retry after 5 s |
| Unicast transmit flows on request | Implemented with the native device role: one channel may feed several inputs as independent sessions; empty positions sent as silence |
| Manual multicast transmit flows from a controller | Implemented with the native device role: create, delete (8 s grace), advertise, restore at start; no group address collision check |
| Multicast receive | Implemented with the native device role: matching rate and encoding, make before break, unicast fallback |
| Routing from the portal and `tesseractl routing` | Implemented: Connect, Replace, Disconnect from either host's portal; receivers on another host only with one page of inputs |
| Names, labels, subscriptions, device latency, encoding from a controller | Implemented with the native device role; applied as typed commands, so every lock applies |
| Sample rate from a controller | Partial: offered values and pin reasons are reported and every write is logged; a change from the vendor's controller is not confirmed (under investigation) |
| Latency and signal monitoring for a controller | Partial: heartbeat with lateness, late packets and signal peaks; display in the vendor's controller (latency graph) unverified |
| Identify and reboot from a controller | Implemented with the native device role; reboot restarts the daemon, not the host |
| Factory reset and clear configuration | Implemented; refused unless `[control] allow_factory_reset = true` |
| Device access lock (PIN) | Partial: set and cleared from the portal and `tesseractl`; a vendor controller sees it but cannot set or clear it |
| Preferred Leader from a controller | Not implemented as a verified feature; set it with `tesseractl clock set-priority` or the portal |
| Network settings from a controller | Not implemented; such writes are refused |
| Interface statistics for a controller | Not implemented; not answered |
| Newer flow-list queries of current controllers | Not implemented; not answered, and some tools refuse some operations because of it |
| Further logical devices' control ports | Not implemented; only the first logical device is served |
| External hardware audio content and sustained clock behavior | Not certified |
| Vendor-controller interoperability as a whole | Not certified |
| Hardware timestamping, including VLAN interfaces | Not certified; NIC/driver dependent |
| AES67 and JACK | Not implemented |
| Automatic latency negotiation/lip-sync | Not implemented |

An available network interface or running daemon does not prove the peer's
control protocol, subscription or audio content is compatible. Configure static
flows only when their parameters are known; use established vendor-controller
procedures where the external device requires them. Tessera does not replace
those procedures with automatic device setup.

The NDIMon plugin requires NDIMon 1.3.0 or newer and pins its own runtime bundle.
Consult its platform catalog/compatibility record rather than substituting an
unrelated package. Initial `integration-contract.json` metadata describes APIs
and features; it is not a certification matrix for every exact host build.

## Native device role

With `--native-device`, `tesserad` serves the device role of the first logical
device itself, in place of the packaged Controller compatibility runtime: the
control, flow-control and device-information ports (UDP 4440, 4455, 8700 and
8800). What it does:

* A controller's change (subscriptions, device and channel names, sample rate,
  encoding, device latency, identify, reboot, factory reset) enters the daemon
  as the same typed command that `tesseractl` and the portal send, so the
  configuration's `locked` lists and the device access lock apply. Saved
  subscriptions remain the only route store.
* As a receiver, the daemon resolves each saved subscription through discovery
  and requests a flow for that input from the transmitter (one flow per input,
  so a change on one input never interrupts another; make before break; a
  refused request is retried after 5 s). A channel advertised in a multicast
  flow of the same rate and encoding is taken from the group instead, with
  unicast as the fallback. Receive ports are UDP 14336 to 14589; a multicast
  group is received on UDP 4321.
* As a transmitter, it answers flow requests, updates and stops; one channel can
  feed several inputs as independent sessions, and sessions of a receiver that
  restarted or went silent are released. A controller can create and delete
  manual multicast flows.
* It sends the monitoring heartbeat (lateness, late packets, signal peaks) and
  shows a controller's identify request as a notice for
  `--native-identify-secs` seconds (default 10). Reboot restarts the daemon.

It is opt-in. Packages install and enable `tessera-controller@main` as before,
and nothing changes until an administrator switches a host. The two cannot run
together: both bind the same ports exclusively. A daemon that cannot bind them
logs `native device role cannot bind its ports ... is the compatibility runtime
still running?` and retries.

**Enable it on a packaged host.** Restarting `tesserad` interrupts audio.

```sh
# 1. Stop and disable the compatibility runtime (its ports must be free).
sudo systemctl disable --now tessera-controller@main

# 2. A drop-in for tesserad: the role, and the vendor token the runtime used
#    (the line after `--vendor` in /usr/lib/tessera/controller.args).
vendor=$(sed -n '/^--vendor$/{n;p;q}' /usr/lib/tessera/controller.args)
sudo install -d -m 0755 /etc/systemd/system/tesserad.service.d
printf '[Service]\nEnvironment=TESSERA_NATIVE_DEVICE=true\nEnvironment=TESSERA_NATIVE_VENDOR=%s\n' "$vendor" |
  sudo tee /etc/systemd/system/tesserad.service.d/70-native-device.conf >/dev/null

# 3. A receiver finds its transmitters through discovery browsing.
printf '[discovery]\nbrowse_devices = true\n' |
  sudo tee /etc/tessera/config.d/60-browse.toml >/dev/null

# 4. Apply.
sudo systemctl daemon-reload
sudo systemctl restart tesserad
journalctl -u tesserad -b --no-pager | grep 'native device role'
# ... native device role serving on 192.0.2.10 (eth0)
```

`TESSERA_NATIVE_DEVICE` takes `true` or `false` (`1` is refused). Add
`Environment=TESSERA_NATIVE_IDENTIFY_SECS=<seconds>` to change the identify
notice. Package upgrades leave the disabled runtime disabled and keep the
drop-in. Do the same on every host that should serve its own device role;
native hosts and hosts on the compatibility runtime can share a network.

**Go back to the compatibility runtime.**

```sh
sudo rm /etc/systemd/system/tesserad.service.d/70-native-device.conf
sudo systemctl daemon-reload
sudo systemctl restart tesserad
tesseractl flows show                 # the role's flows have ids starting with native-
sudo tesseractl flows clear --yes     # configured [[flow]] entries stay
sudo systemctl enable --now tessera-controller@main
```

The native role keeps its receive and transmit flows in the controller flow set
of `state.toml`, like any controller flows; without the role nobody maintains
them, so clear them before the runtime takes over (it then sets up the flows
controllers ask it for). Saved subscriptions, names and settings stay in
`state.toml`. Keep `60-browse.toml` if you use the Routing page; remove it
otherwise.

## Routing from the portal

The portal's **Routing** page ([web-portal.md](web-portal.md#routing)) and
`tesseractl routing` ([cli.md](cli.md#routing)) connect, replace and disconnect
receive inputs through the daemon. Each change is reviewed first; on
confirmation the daemon checks the receiver's current selection, the locks and
the source again and sends one command. It never retries: an operation whose
outcome is unknown blocks further changes on that receiver until it is
inspected and reconciled
([troubleshooting](troubleshooting.md#recovering-an-uncertain-routing-operation)).

* **This host's receiver.** With the native device role the daemon stores the
  selection as a subscription and requests the flow itself; one source channel
  may feed several inputs. With the compatibility runtime, the runtime carries
  the change out; there a source channel feeds only one input of the receiver,
  and the matrix disables the other cells with the reason.
* **Another host's receiver.** The change is written over that device's control
  protocol, so routing works from either host's portal. The receiving device
  applies its own locks; a receiver that reports the device access lock is shown
  read-only with the reason. Only receivers whose inputs fit on one page are
  offered for changes this way, and no local recovery covers a remote write.
* **Sources.** A source on another host must be listed in that sending host's
  `[control] routing_sources` (its 16-digit device ID,
  [configuration.md](configuration.md#receiver-routing)); this host's own
  transmitter needs no entry. The source must run the same sample rate and
  encoding as this host's first device, and discovery browsing
  (`[discovery] browse_devices = true`) must be on.
* **Locks.** `[control] locked = ["subscriptions"]` refuses routing changes on
  that host's receiver before anything is sent; `locked = ["flows"]` keeps the
  flow from being installed; the device access lock refuses both.

A receiver's confirmation does not verify audio: **Receiving** in the matrix
means local packet activity, not that the selected source is what is heard.
Allow about five seconds for audio to settle after a change. Routes made from
the vendor's controller and static `[[flow]]` entries keep working alongside.
