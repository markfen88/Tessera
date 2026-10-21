# Timing, scheduling and privileges

Tessera's media clock is a virtual affine clock driven from monotonic time.
PTP adjusts that clock, not CLOCK_REALTIME. The daemon never needs CAP_SYS_TIME.

## Runtime privileges

The shipped daemon runs as user tessera with bounded capabilities for network
ports, scheduling and memory locking. Hardware timestamping can require extra
CAP_NET_RAW/CAP_NET_ADMIN privileges and suitable NIC/driver support. Grant these
only through an intentional systemd override; hardware support is not guaranteed.
The separate standalone network helper owns OS network changes.

## Media scheduling

Configuration controls real-time priority, CPU affinity, locking, engine ticks,
packet sizes and latency. Match the selected device/flow/backend rates and
channel counts. Leave headroom for IRQs, device drivers and the rest of the host.
Raising a priority or shrinking a tick does not fix sender discontinuities or
an incompatible peer configuration.

Monitor `tesseractl metrics`, the portal and the service journal. Scheduling or
memory-lock refusals, xruns, late packets, underruns and drift clamps are
operational signals to investigate. Configured latency is a buffering target;
it is not a measurement of end-to-end loudspeaker/video delay.

## Network and hardware

Prefer wired native Ethernet. Configure VLANs, IGMP snooping/querier and switch
power-saving behavior deliberately. Put media peers on the intended media
network. Hardware timestamps require a suitable interface; a VLAN interface does
not automatically inherit every hardware timestamping capability.

## Latency and clock domains

Choose a latency that covers packet duration, expected network variation,
scheduler wake-up and backend periods. Start conservatively before reducing it.
The ALSA hardware bridge compensates between card and media clocks. A downstream
processor can have its own clock correction; avoid blindly combining aggressive
servos. Persistent compensation at a configured limit needs investigation.

See [configuration](configuration.md), [audio backends](audio-backends.md) and
[troubleshooting](troubleshooting.md) for supported settings and recovery.
