# Interoperability and compatibility

Tessera implements a PTPv1 virtual media clock and configurable static UDP media
flows. The project targets interoperability with Dante® media networks; it is
not vendor-certified, affiliated with or endorsed by Audinate.

| Capability | Beta status |
|---|---|
| PTPv1 follower/leader and holdover | Implemented; drives media time, never the host's wall clock |
| Configured RX/TX UDP flows | Implemented; peers must agree on rate, encoding, channels, port and packet geometry |
| Local ALSA/PipeWire and optional hardware bridge | Implemented; installed backend/build features must match configuration |
| External hardware audio content and sustained clock behavior | Not certified |
| Hardware timestamping, including VLAN interfaces | NIC/driver dependent; not certified on every platform |
| Vendor-controller subscriptions, routing and device management | Not implemented |
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
