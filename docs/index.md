# Tessera documentation

Tessera 0.5.1 is an experimental audio-over-IP runtime for Linux, interoperable with Dante® media
networks in the areas its [compatibility limits](interoperability.md) list; it is not certified.
Start with the [README](../README.md) and [release notes](../CHANGELOG.md).

| Guide | Contents |
|---|---|
| [User guide](user-guide.md) | Packages, installation, users/groups, initial configuration, CLI recipes, upgrades and removal |
| [Configuration reference](configuration.md) | Every supported key, drop-ins, locks, flows, bridges, reload and persistence |
| [CLI reference](cli.md) | `tesseractl`, `tesserad`, `tessera-web`, `tessera-pw`, `tessera-network` and `tessera-bundle`: commands, options, exit codes and examples |
| [Web portal](web-portal.md) | Authentication, pages/API, routing matrix, device access lock, live updates, speaker/services/EQ and exposure |
| [Management/audio VLANs](network-vlans.md) | Supported backends (networkd, Netplan, NetworkManager, ifupdown), role assignments, DNS search domains, safe apply, confirmation, recovery and NDIMon ownership and handoff |
| [Audio backends](audio-backends.md) | ALSA PCM, PipeWire and hardware-card bridge configuration |
| [Raspberry Pi speaker](raspberry-pi-speaker.md) | Sound-card/CamillaDSP speaker configuration |
| [CamillaDSP speaker DSP](dsp.md) | The `tessera-dsp` package, what the speaker profile sets up, upgrades and purge, and how the pinned CamillaDSP archives are built |
| [Control interface](control-interface.md) | Local socket, typed commands/replies and integration semantics |
| [Architecture](architecture.md) | Processes, clocks, audio paths, privileges and configuration ownership |
| [Timing and privileges](rt-tuning.md) | Scheduling, memory, network and latency considerations |
| [Bundle guide](bundle.md) | `tessera-bundle`: verification, trusted release keys and unsigned builds, profiles, upgrades, purge and package build options |
| [Compatibility limits](interoperability.md) | Implemented behavior, the opt-in native device role, routing from the portal and external hardware/control limitations |
| [Troubleshooting](troubleshooting.md) | Service, clock, network, backend, speaker and portal, installer, Network page, configuration and package problems |

NDIMon host plugins and their authoring/installation guides live in
[NDIMon-Plugins](https://github.com/markfen88/NDIMon-Plugins).
