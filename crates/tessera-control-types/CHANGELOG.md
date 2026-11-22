# Changelog

## 0.6.0

Additive wire changes (envelope version remains 1), provisional 0.x: the host
router, port levels, the host volume, mixer panels and control routing
(`docs/plans/2026-11-22-host-router.md`, ADR 0017).

* Types: `RouterPort`, `CrosspointKey`, `Crosspoint`, `PortLabel`,
  `RouterSpec`, `CrosspointOrigin`, `CrosspointStatus`, `PortStatus`,
  `RouterReport`, `HostVolumeReading`, `HostVolumeStatus`, `MixerSpec`,
  `MixerStrip`, `StripControl`, `FaderRange`, `ControlRef`,
  `CQ_TCP_MIDI_PORT`.
* `SurfaceSource::TcpMidi`; `BindingAction::{HostVolume, HostMute, PortGain,
  PortMute, CrosspointGain, CrosspointMute, Follow}`; `SurfaceBinding` gains
  `ports`, `crosspoints`, `follow`, and its `target` may be empty (left out
  of JSON) for actions without channels.
* Fields (serde defaults, absent in older JSON): `CardBridgeSpec::local_only`,
  `BridgeStatus::local_only`, `ChannelStatus::output_gain_owner`,
  `Status::host_volume`, `PortStatus::custom_label` (the label was set by the
  user; `false`: the default).
* `HostVolumeStatus::gain_owner` (named `owner` in development builds; JSON
  with `owner` is still read).
* `SurfaceBinding::target` is read leniently: absent, `null`, `{}` or a table
  that lists no channel is no target (`ChannelSet::default`).
* Commands: `SetRouter`, `ResetRouter`, `SetCrosspoint`, `RemoveCrosspoint`,
  `SetCrosspointGain`, `SetCrosspointMute`, `SetPortGain`, `SetPortMute`,
  `SetPortLabel` (one port's label, or its default with `label: null`),
  `SetHostVolume`, `SetHostMute`, `SetMixer`, `RemoveMixer`.
* Queries and replies: `Query::Router` → `Reply::Router`, `Query::Mixers` →
  `Reply::Mixers`.
* Events: `RouterChanged`, `CrosspointGainChanged`, `CrosspointMuteChanged`,
  `PortGainChanged`, `PortMuteChanged`, `HostVolumeChanged`, `MixerChanged`.
* Errors: `LevelOwned`, `InvalidMixer`.

Source-breaking for Rust users who build these values with struct literals
(the wire format is unchanged): the new structs `RouterSpec`, `Crosspoint`,
`CrosspointKey`, `PortLabel`, `MixerSpec`, `MixerStrip`, `FaderRange`,
`ControlRef` and `MidiAddress`, and `CardBridgeSpec` and `SurfaceBinding`
(which gained fields: `card_first_channel`, `local_only`; `ports`,
`crosspoints`, `follow`), are `#[non_exhaustive]`, so later fields can be
added without breaking again. Build them with their constructors and set
fields afterwards: `RouterSpec::new`, `Crosspoint::new` (`with_gain`,
`with_muted`), `CrosspointKey::new`, `PortLabel::new`, `MixerSpec::new`,
`MixerStrip::new`, `FaderRange::new` (`with_taper`), `ControlRef::new`,
`MidiAddress::new`, `CardBridgeSpec::new`, `SurfaceBinding::new` (`host`,
`on_ports`, `on_crosspoints`, `following`).

Behaviour of the state engine that clients see: the access lock covers a
whole-grid router change by the devices whose effective crosspoints or
labels it changes (an empty `SetRouter` on the default grid is refused while
the device it routes is locked), and a surface whose bindings drive the host
volume or mute (directly or through `follow`) while any device is locked;
fader priority covers `SetRouter`, `ResetRouter`, `SetCrosspoint` and
`SetMonitor` where they would change an owned crosspoint gain.

## 0.3.0

The 0.2.2 release (published from the public branch) had no wire changes; everything
below is new in 0.3.0.

Additive wire changes (envelope version remains 1): `FlowSpec`,
`Command::SetProtocolFlows`, the admin-only `RequestBody::ProtocolCommand`,
`Event::ProtocolFlowsChanged`, `LockedValueRefused`, `RuntimeValueChanged`,
`SubscriptionChangedWithOrigin` and `ConfigReloadedWithOrigin`. Existing variants retain their shapes.
The origin-bearing companions accompany the legacy subscription and reload events.
Runtime change events precede the debounced state write, as specified by ADR 0008.
See [the control interface](../../docs/control-interface.md).
