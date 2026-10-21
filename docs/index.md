# Tessera documentation

Tessera 0.2.0-beta is an experimental audio-over-IP runtime for Linux.
Start with the [README](../README.md) and [release notes](../CHANGELOG.md).

| Guide | Contents |
|---|---|
| [User guide](user-guide.md) | Packages, installation, users/groups, initial configuration, CLI recipes, upgrades and removal |
| [Configuration reference](configuration.md) | Every supported key, drop-ins, locks, flows, bridges, reload and persistence |
| [CLI reference](cli.md) | Commands, options, exit codes and examples |
| [Web portal](web-portal.md) | Authentication, pages/API, live updates, speaker/services/EQ and exposure |
| [Management/audio VLANs](network-vlans.md) | Supported backends, role assignments, safe apply, confirmation, recovery and NDIMon ownership |
| [Audio backends](audio-backends.md) | ALSA PCM, PipeWire and hardware-card bridge configuration |
| [Raspberry Pi speaker](raspberry-pi-speaker.md) | Sound-card/CamillaDSP speaker configuration |
| [Control interface](control-interface.md) | Local socket, typed commands/replies and integration semantics |
| [Architecture](architecture.md) | Processes, clocks, audio paths, privileges and configuration ownership |
| [Timing and privileges](rt-tuning.md) | Scheduling, memory, network and latency considerations |
| [Bundle guide](bundle.md) | Trusted installation, profiles, package build options, upgrades and purge |
| [Compatibility limits](interoperability.md) | Implemented behavior and external hardware/control limitations |
| [Troubleshooting](troubleshooting.md) | Service, clock, network, backend, configuration and package problems |

NDIMon host plugins and their authoring/installation guides live in
[NDIMon-Plugins](https://github.com/markfen88/NDIMon-Plugins).
