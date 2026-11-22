//! Mixer panels: user-built desks of faders and buttons (provisional 0.x).
//!
//! A panel ([`MixerSpec`]) is saved intent like a surface, changed with
//! [`Command::SetMixer`](crate::Command::SetMixer) and
//! [`Command::RemoveMixer`](crate::Command::RemoveMixer) and read with
//! [`Query::Mixers`](crate::Query::Mixers). It changes no audio: every
//! strip control sends the ordinary level and mute commands of its action
//! (`SetChannelGain`, `SetOutputGain`, `SetHostVolume`, `SetPortGain`,
//! `SetCrosspointGain`, and the mute counterparts).
//!
//! **Links are derived, not stored.** A strip is linked to a hardware
//! control when a surface has a binding with the same action and target
//! ([`StripControl::links_to`]). The editor links a control by sending
//! `SetSurface` (the binding) and then `SetMixer` (the strip); `SetMixer`
//! never creates bindings. See `docs/plans/2026-11-22-mixer-panel.md`.
//!
//! **Default panel.** When no panel is saved the portal shows a default it
//! derives from the status (channels in use, then Main); the daemon does not
//! synthesise or save it (`docs/plans/2026-11-22-host-router.md` §8).

use crate::{BindingAction, ButtonMode, ChannelSet, CrosspointKey, RouterPort, Taper};
use alloc::string::String;
use alloc::vec::Vec;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// A fader's travel, milli-dB at the bottom (just above off) and the top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct FaderRange {
    /// Gain at the bottom of travel (just above off), milli-dB.
    pub min_mdb: i32,
    /// Gain at the top of travel, milli-dB; above `min_mdb`.
    pub max_mdb: i32,
    /// How position maps to gain (absent: even dB per travel).
    #[cfg_attr(feature = "serde", serde(default))]
    pub taper: Taper,
}

impl FaderRange {
    /// `min_mdb..max_mdb` with the dB taper.
    #[must_use]
    pub const fn new(min_mdb: i32, max_mdb: i32) -> Self {
        Self {
            min_mdb,
            max_mdb,
            taper: Taper::Db,
        }
    }

    /// The same range with `taper`.
    #[must_use]
    pub const fn with_taper(mut self, taper: Taper) -> Self {
        self.taper = taper;
        self
    }
}

/// What one strip control does. The action decides which target field is
/// used; the others stay empty:
///
/// | action | field |
/// |---|---|
/// | `gain`, `mute`, `output_gain`, `output_mute` | `target` (channels; output actions RX only) |
/// | `host_volume`, `host_mute` | none (the host volume) |
/// | `port_gain`, `port_mute` | `ports` |
/// | `crosspoint_gain`, `crosspoint_mute` | `crosspoints` |
///
/// Faders take the level actions (`gain`, `output_gain`, `host_volume`,
/// `port_gain`, `crosspoint_gain`), buttons the mute actions. `follow` is a
/// surface-to-surface binding and is not a strip action.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(tag = "kind", rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum StripControl {
    /// A fader.
    Fader {
        action: BindingAction,
        /// Channel actions: the channels (one strip may drive several, a
        /// stereo pair).
        #[cfg_attr(
            feature = "serde",
            serde(default, skip_serializing_if = "Option::is_none")
        )]
        target: Option<ChannelSet>,
        /// Port actions: the router ports.
        #[cfg_attr(
            feature = "serde",
            serde(default, skip_serializing_if = "Vec::is_empty")
        )]
        ports: Vec<RouterPort>,
        /// Crosspoint actions: the crosspoints.
        #[cfg_attr(
            feature = "serde",
            serde(default, skip_serializing_if = "Vec::is_empty")
        )]
        crosspoints: Vec<CrosspointKey>,
        /// The fader's travel; `None`: off..+12 dB for gains, the
        /// configured range for the host volume.
        #[cfg_attr(
            feature = "serde",
            serde(default, skip_serializing_if = "Option::is_none")
        )]
        range: Option<FaderRange>,
    },
    /// A button.
    Button {
        action: BindingAction,
        /// Channel actions: the channels.
        #[cfg_attr(
            feature = "serde",
            serde(default, skip_serializing_if = "Option::is_none")
        )]
        target: Option<ChannelSet>,
        /// Port actions: the router ports.
        #[cfg_attr(
            feature = "serde",
            serde(default, skip_serializing_if = "Vec::is_empty")
        )]
        ports: Vec<RouterPort>,
        /// Crosspoint actions: the crosspoints.
        #[cfg_attr(
            feature = "serde",
            serde(default, skip_serializing_if = "Vec::is_empty")
        )]
        crosspoints: Vec<CrosspointKey>,
        /// How a press drives mute (absent: toggle).
        #[cfg_attr(feature = "serde", serde(default))]
        mode: ButtonMode,
    },
}

impl StripControl {
    /// A fader on channels (`Gain` or `OutputGain`) with the default range.
    #[must_use]
    pub fn channel_fader(action: BindingAction, target: ChannelSet) -> Self {
        Self::Fader {
            action,
            target: Some(target),
            ports: Vec::new(),
            crosspoints: Vec::new(),
            range: None,
        }
    }

    /// A toggle button on channels (`Mute` or `OutputMute`).
    #[must_use]
    pub fn channel_button(action: BindingAction, target: ChannelSet) -> Self {
        Self::Button {
            action,
            target: Some(target),
            ports: Vec::new(),
            crosspoints: Vec::new(),
            mode: ButtonMode::Toggle,
        }
    }

    /// The host volume fader.
    #[must_use]
    pub fn host_volume() -> Self {
        Self::Fader {
            action: BindingAction::HostVolume,
            target: None,
            ports: Vec::new(),
            crosspoints: Vec::new(),
            range: None,
        }
    }

    /// The host mute button (toggle).
    #[must_use]
    pub fn host_mute() -> Self {
        Self::Button {
            action: BindingAction::HostMute,
            target: None,
            ports: Vec::new(),
            crosspoints: Vec::new(),
            mode: ButtonMode::Toggle,
        }
    }

    /// The action.
    #[must_use]
    pub const fn action(&self) -> BindingAction {
        match self {
            Self::Fader { action, .. } | Self::Button { action, .. } => *action,
        }
    }

    /// The channel target, if any.
    #[must_use]
    pub const fn target(&self) -> Option<&ChannelSet> {
        match self {
            Self::Fader { target, .. } | Self::Button { target, .. } => target.as_ref(),
        }
    }

    /// The port targets.
    #[must_use]
    pub fn ports(&self) -> &[RouterPort] {
        match self {
            Self::Fader { ports, .. } | Self::Button { ports, .. } => ports,
        }
    }

    /// The crosspoint targets.
    #[must_use]
    pub fn crosspoints(&self) -> &[CrosspointKey] {
        match self {
            Self::Fader { crosspoints, .. } | Self::Button { crosspoints, .. } => crosspoints,
        }
    }

    /// Is `binding` a link of this control: the same action and a target
    /// that overlaps (one channel, port or crosspoint in common; any host
    /// binding for the host actions)? This is how the portal finds the
    /// hardware controls that drive a strip.
    #[must_use]
    pub fn links_to(&self, binding: &crate::SurfaceBinding) -> bool {
        if binding.action != self.action() {
            return false;
        }
        match self.action() {
            BindingAction::HostVolume | BindingAction::HostMute => true,
            BindingAction::PortGain | BindingAction::PortMute => {
                self.ports().iter().any(|p| binding.ports.contains(p))
            }
            BindingAction::CrosspointGain | BindingAction::CrosspointMute => self
                .crosspoints()
                .iter()
                .any(|x| binding.crosspoints.contains(x)),
            BindingAction::Follow => false,
            _ => self.target().is_some_and(|t| {
                t.device == binding.target.device
                    && t.direction == binding.target.direction
                    && t.channels
                        .iter()
                        .any(|c| binding.target.channels.contains(c))
            }),
        }
    }
}

/// One channel strip: a fader or a button, its scribble-strip label and its
/// place on the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct MixerStrip {
    /// Unique within the panel: 1..=64 letters, digits, `-`, `_`.
    pub id: String,
    /// The scribble strip, 0..=31 bytes; empty: the target's labels.
    #[cfg_attr(feature = "serde", serde(default))]
    pub label: String,
    /// Starts a new group with this heading (1..=31 bytes); `None`: the
    /// strip stays in the group of the strip before it (the first group
    /// may be unnamed).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub group: Option<String>,
    /// A master strip: drawn apart (its own column, at the end on a phone).
    #[cfg_attr(feature = "serde", serde(default))]
    pub master: bool,
    pub control: StripControl,
}

impl MixerStrip {
    /// A strip with no label, group or master flag.
    #[must_use]
    pub fn new(id: impl Into<String>, control: StripControl) -> Self {
        Self {
            id: id.into(),
            label: String::new(),
            group: None,
            master: false,
            control,
        }
    }
}

/// A saved mixer panel. Several are allowed; they are listed in id order.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct MixerSpec {
    /// Unique: 1..=64 letters, digits, `-`, `_`.
    pub id: String,
    /// Shown as the panel's title, 1..=63 bytes.
    pub title: String,
    /// In order; reordering is a `SetMixer` with the new order.
    #[cfg_attr(feature = "serde", serde(default))]
    pub strips: Vec<MixerStrip>,
}

impl MixerSpec {
    /// Panel `id` titled `title` with `strips`.
    #[must_use]
    pub fn new(id: impl Into<String>, title: impl Into<String>, strips: Vec<MixerStrip>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            strips,
        }
    }
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;
    use crate::{Channel, DeviceKey, Direction, SurfaceBinding};

    fn set(direction: Direction, channels: &[u16]) -> ChannelSet {
        ChannelSet {
            device: DeviceKey("main".into()),
            direction,
            channels: channels.iter().map(|c| Channel::new(*c).unwrap()).collect(),
        }
    }

    #[test]
    fn a_panel_round_trips_and_defaults_its_optional_fields() {
        let mut main = MixerStrip::new("main", StripControl::host_volume());
        main.master = true;
        main.label = "Main L/R".into();
        let spec = MixerSpec {
            id: "main".into(),
            title: "Speaker".into(),
            strips: vec![
                MixerStrip::new(
                    "cq",
                    StripControl::channel_fader(BindingAction::Gain, set(Direction::Tx, &[1, 2])),
                ),
                main,
            ],
        };
        let j = serde_json::to_string(&spec).unwrap();
        assert!(j.contains("\"kind\":\"fader\""), "{j}");
        assert!(!j.contains("\"ports\""), "{j}");
        assert_eq!(serde_json::from_str::<MixerSpec>(&j).unwrap(), spec);
        let small: MixerSpec = serde_json::from_str(
            "{\"id\":\"p\",\"title\":\"P\",\"strips\":[{\"id\":\"m\",\"control\":\
             {\"kind\":\"button\",\"action\":\"host_mute\"}}]}",
        )
        .unwrap();
        assert_eq!(small.strips[0].control, StripControl::host_mute());
        assert!(small.strips[0].label.is_empty());
    }

    #[test]
    fn links_are_the_same_action_on_an_overlapping_target() {
        let fader = StripControl::channel_fader(BindingAction::Gain, set(Direction::Rx, &[1, 2]));
        let b = SurfaceBinding::new("f1", BindingAction::Gain, set(Direction::Rx, &[2]));
        assert!(fader.links_to(&b));
        let other = SurfaceBinding::new("f2", BindingAction::Gain, set(Direction::Tx, &[2]));
        assert!(!fader.links_to(&other));
        let out = SurfaceBinding::new("f3", BindingAction::OutputGain, set(Direction::Rx, &[1]));
        assert!(!fader.links_to(&out));
        let mut host = SurfaceBinding::new("f4", BindingAction::HostVolume, ChannelSet::default());
        assert!(StripControl::host_volume().links_to(&host));
        host.action = BindingAction::HostMute;
        assert!(StripControl::host_mute().links_to(&host));
    }
}
