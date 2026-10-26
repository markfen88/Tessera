# Timing, scheduling and privileges

Tessera's media clock is a virtual affine clock driven from monotonic time.
PTP adjusts that clock, not CLOCK_REALTIME. The daemon never needs CAP_SYS_TIME.

## Runtime privileges

The shipped daemon runs as user tessera with bounded capabilities for network
ports, scheduling and memory locking. Hardware timestamping can require extra
CAP_NET_RAW/CAP_NET_ADMIN privileges and suitable NIC/driver support. Grant these
only through an intentional systemd override; hardware support is not guaranteed.
The separate standalone network helper owns OS network changes.

The package raises the host's socket-buffer ceilings (`net.core.rmem_max` and
`wmem_max` to 8 MiB, in `/usr/lib/sysctl.d/60-tessera.conf`) because the
unprivileged daemon cannot force larger buffers; the daemon reports capped
buffers in `tesseractl metrics`. The package records the values it replaced and
puts them back when it is removed.

On a CamillaDSP speaker the engine (`camilladsp.service`) runs with `SCHED_FIFO`
priority 60 and locked memory, below the daemon's media worker (default 80). It is
the one clock crossing between the media clock and the sound card; do not add a
second rate servo in front of it ([dsp.md](dsp.md)).

## Media scheduling

Configuration controls real-time priority, CPU affinity, locking, engine ticks,
packet sizes and latency. Match the selected device/flow/backend rates and
channel counts. Leave headroom for IRQs, device drivers and the rest of the host.
Raising a priority or shrinking a tick does not fix sender discontinuities or
an incompatible peer configuration.

A transmitted packet is sent at most `max_lag_us` after its release time (half
the device latency by default, at least one packet period: 2 ms at 4 ms). When
the engine wakes later than that the flow skips the packets it missed and counts
a `late_skip`; it never sends them late and never in a burst (ADR 0006). So the
audio is only as free of dropouts as the host's wake-ups are punctual: read
`engine.wake_lateness_us{...}`, `engine.tick_skips` and the flows' `late_skips`
in `tesseractl metrics`. Real-time priority and locked memory do not bound the
lateness on every kernel: on a virtual machine or a kernel that is not fully
preemptible a `SCHED_FIFO` thread has been seen to wake 2 to 3 ms late on an idle
machine, and tens of milliseconds late while the machine compiles. Such a host
either drops audio now and then or needs a larger budget, as far as the
receivers' latency allows ([configuration](configuration.md), `max_lag_us`). The
network-namespace fault suite accepts the host's lateness only as far as the
daemons' counters show it and has a strict form for a host that meets the budget.

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
