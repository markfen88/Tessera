# Tessera changelog

## 0.2.0-beta

### Standalone network configuration

- Added a modular privileged `tessera-network` helper and portal Network editor
  for named management/audio networks on one wired port.
- Supports untagged or VLAN 1–4094 networks with DHCP/static IPv4, management-only
  default gateway/DNS and explicit audio interface locking.
- Native networkd and Netplan/networkd are supported. Unsupported complex
  configurations are refused instead of silently losing settings.
- Applies journal the previous state and arm independent device-side recovery.
  Once ready, explicitly Keep through the new management address within 60
  seconds. Revert, failure, expiry or boot recovery restores the previous state.
- Management/audio traffic uses owned firewall isolation. NDIMon ownership
  disables standalone mutations so there is one OS network manager.

### Daemon, control and portal

- Added configuration-first controls, drop-ins/locks, runtime channel counts,
  impact-token confirmation and a generated configuration reference.
- Improved subscription receive-state reporting and live portal updates.
- Added shared appearance controls and optional Speaker/service/EQ integration.
- Retained the PTPv1 virtual media clock, static UDP flows, ALSA/PipeWire clients
  and optional sound-card bridge with drift compensation.

### Packaging and NDIMon integration

- Added profile-based bundles for speakers and NDIMon runtime provisioning,
  with configuration preservation and deliberate restart/removal behavior.
- Added separate experimental Ubuntu 24.04 arm64 bundle support with explicit
  OS opt-in. It cannot install Debian binaries onto Ubuntu or vice versa.
- Updated source/workspace versions to 0.2.0-beta. Debian packages use
  0.2.0~beta-* so they sort before the eventual final 0.2.0 packages.
- Plugin sources and installable host archives are published separately in
  [NDIMon-Plugins](https://github.com/markfen88/NDIMon-Plugins). The Rust runtime
  remains here, and each plugin pins an exact runtime bundle/checksum.
- Added initial integration feature/API metadata. The host does not yet offer
  automatic warnings for every unverified exact-build combination.

### Limitations

This beta does not implement vendor-controller subscriptions/device control,
AES67, JACK, Bluetooth calibration, HDMI ARC/eARC or automatic lip-sync. Static
wire/flow parameters remain provisional for external hardware. Audio content
interoperability and sustained clock/hardware PTP behavior are not certified.
Source publication does not deploy to an existing appliance.
