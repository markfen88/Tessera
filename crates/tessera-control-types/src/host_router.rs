//! The host router: one saved patch grid between local I/O and network
//! channels, port levels, and the host (speaker) volume (provisional 0.x).
//!
//! Tessera is an audio routing device; network routing interoperable with
//! Dante® is one of its capabilities. Two layers:
//!
//! * **Network channels.** The host's network device (by default one,
//!   `main`) and its RX (network → this host) and TX (this host → network)
//!   channels. Network controllers see these and nothing else.
//! * **Local I/O.** Sound-card inputs and outputs (opened by card bridges),
//!   what local applications play and capture, the speaker. A card never
//!   reaches the network unless a crosspoint patches it to a TX channel.
//!
//! The router connects the two: every [`Crosspoint`] takes one source port
//! to one destination port with its own gain and mute, and a destination
//! sums every crosspoint that feeds it. See
//! `docs/plans/2026-11-22-host-router.md` for the full model and ADR 0017.
//!
//! **Ports.** [`RouterPort`] names one channel of one port. Sources:
//! [`RouterPort::NetworkRx`], [`RouterPort::AppOut`], [`RouterPort::CardIn`].
//! Destinations: [`RouterPort::NetworkTx`], [`RouterPort::HostOut`],
//! [`RouterPort::CardOut`]. A destination can also be used as a source: it
//! then means what that destination carries after its own level and mute
//! (a tap, like a desk's direct out), which is how today's local monitoring
//! (`SetMonitor`) and playback bridges are expressed. Taps make loops
//! possible; the daemon refuses a grid with a loop.
//!
//! **Levels.** Every port has a level and a mute. Those of network channels
//! and of the host output are the existing channel controls: the level of
//! `NetworkRx { device, channel }` is the RX channel's gain
//! ([`Command::SetChannelGain`](crate::Command::SetChannelGain)), of
//! `NetworkTx` the TX channel's gain, of `HostOut` the RX channel's output
//! level ([`Command::SetOutputGain`](crate::Command::SetOutputGain)).
//! [`Command::SetPortGain`](crate::Command::SetPortGain) on those ports is
//! the same change (one stored value, the same events);
//! the other ports' levels are stored with the router.
//!
//! These types are provisional: fields may still be added before 1.0.

use crate::{Channel, DeviceKey, Gain};
use alloc::string::String;
use alloc::vec::Vec;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// One channel of one router port.
///
/// In JSON and TOML it is a table tagged by `kind`:
/// `{ kind = "card_in", card = "hw:CARD=CQ,DEV=0", channel = 15 }`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(tag = "kind", rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum RouterPort {
    /// Source: RX channel `channel` of network device `device` (audio
    /// received from the network), after the channel's own gain and mute.
    NetworkRx { device: DeviceKey, channel: Channel },
    /// Destination: TX channel `channel` of `device` (sent to the network).
    /// Its level is the TX channel's gain and mute.
    NetworkTx { device: DeviceKey, channel: Channel },
    /// Source: what local applications play into `device` on channel
    /// `channel` (the ALSA plugin's or PipeWire's playback; today the TX
    /// ring). One per TX channel of the device.
    AppOut { device: DeviceKey, channel: Channel },
    /// Destination: what local applications capture from `device` on
    /// channel `channel` (today the RX ring: `pcm.tessera`, which the
    /// speaker's DSP plays). One per RX channel of the device. Its level is
    /// the RX channel's output level and mute.
    HostOut { device: DeviceKey, channel: Channel },
    /// Source: input `channel` (1-based) of a sound card, available while
    /// an enabled capture bridge opens that card (`card` equal to the
    /// bridge's ALSA PCM, as written) and carries that card channel.
    CardIn { card: String, channel: Channel },
    /// Destination: output `channel` (1-based) of a sound card, available
    /// while an enabled playback bridge opens that card and carries it.
    CardOut { card: String, channel: Channel },
}

impl RouterPort {
    /// Is this a destination (a port a crosspoint can feed)? Sources are
    /// [`NetworkRx`](Self::NetworkRx), [`AppOut`](Self::AppOut) and
    /// [`CardIn`](Self::CardIn); every port can be a source (a destination
    /// as a source is a tap of what it carries).
    #[must_use]
    pub const fn is_destination(&self) -> bool {
        matches!(
            self,
            Self::NetworkTx { .. } | Self::HostOut { .. } | Self::CardOut { .. }
        )
    }

    /// The network device the port belongs to; `None` for card ports.
    #[must_use]
    pub const fn device(&self) -> Option<&DeviceKey> {
        match self {
            Self::NetworkRx { device, .. }
            | Self::NetworkTx { device, .. }
            | Self::AppOut { device, .. }
            | Self::HostOut { device, .. } => Some(device),
            Self::CardIn { .. } | Self::CardOut { .. } => None,
        }
    }

    /// The sound card (its ALSA PCM) of a card port; `None` otherwise.
    #[must_use]
    pub fn card(&self) -> Option<&str> {
        match self {
            Self::CardIn { card, .. } | Self::CardOut { card, .. } => Some(card),
            _ => None,
        }
    }

    /// The 1-based channel of the port.
    #[must_use]
    pub const fn channel(&self) -> Channel {
        match self {
            Self::NetworkRx { channel, .. }
            | Self::NetworkTx { channel, .. }
            | Self::AppOut { channel, .. }
            | Self::HostOut { channel, .. }
            | Self::CardIn { channel, .. }
            | Self::CardOut { channel, .. } => *channel,
        }
    }

    /// The snake-case kind, as in JSON (`network_rx`, `card_in`, ...).
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::NetworkRx { .. } => "network_rx",
            Self::NetworkTx { .. } => "network_tx",
            Self::AppOut { .. } => "app_out",
            Self::HostOut { .. } => "host_out",
            Self::CardIn { .. } => "card_in",
            Self::CardOut { .. } => "card_out",
        }
    }
}

/// Which crosspoint: a source port and a destination port.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct CrosspointKey {
    /// Any port; a destination port here is a tap of what it carries.
    pub source: RouterPort,
    /// A destination port ([`RouterPort::is_destination`]).
    pub destination: RouterPort,
}

impl CrosspointKey {
    /// The crosspoint from `source` to `destination`.
    #[must_use]
    pub const fn new(source: RouterPort, destination: RouterPort) -> Self {
        Self {
            source,
            destination,
        }
    }
}

/// One patched crosspoint: `source` mixed into `destination` with its own
/// gain and mute.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct Crosspoint {
    /// Any port; a destination port here is a tap of what it carries.
    pub source: RouterPort,
    /// A destination port ([`RouterPort::is_destination`]).
    pub destination: RouterPort,
    /// Gain of this crosspoint (unity when absent).
    #[cfg_attr(feature = "serde", serde(default))]
    pub gain: Gain,
    /// Muted: patched but silent (absent: false).
    #[cfg_attr(feature = "serde", serde(default))]
    pub muted: bool,
}

impl Crosspoint {
    /// A crosspoint at unity, not muted.
    #[must_use]
    pub fn new(source: RouterPort, destination: RouterPort) -> Self {
        Self {
            source,
            destination,
            gain: Gain::default(),
            muted: false,
        }
    }

    /// The same crosspoint with `gain`.
    #[must_use]
    pub fn with_gain(mut self, gain: Gain) -> Self {
        self.gain = gain;
        self
    }

    /// The same crosspoint, muted or not.
    #[must_use]
    pub fn with_muted(mut self, muted: bool) -> Self {
        self.muted = muted;
        self
    }

    /// Its source and destination.
    #[must_use]
    pub fn key(&self) -> CrosspointKey {
        CrosspointKey {
            source: self.source.clone(),
            destination: self.destination.clone(),
        }
    }
}

/// A user label of a local port (`CardIn` "CQ Main L"). Network channels
/// are labelled with `SetChannelLabel`, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct PortLabel {
    /// An `AppOut`, `HostOut`, `CardIn` or `CardOut` port.
    pub port: RouterPort,
    /// 1..=31 bytes, no control characters.
    pub label: String,
}

impl PortLabel {
    /// `label` on `port`.
    #[must_use]
    pub fn new(port: RouterPort, label: impl Into<String>) -> Self {
        Self {
            port,
            label: label.into(),
        }
    }
}

/// The saved router grid ([`Command::SetRouter`](crate::Command::SetRouter)):
/// every crosspoint the user wants, in any order. Crosspoints a legacy
/// (network) card bridge implies are not part of it (see
/// [`CrosspointOrigin::Bridge`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct RouterSpec {
    /// The complete grid; each source/destination pair at most once.
    pub crosspoints: Vec<Crosspoint>,
    /// Labels of local ports; replaces every saved label (absent: none).
    #[cfg_attr(feature = "serde", serde(default))]
    pub labels: Vec<PortLabel>,
}

impl RouterSpec {
    /// A grid of `crosspoints` with `labels`.
    #[must_use]
    pub const fn new(crosspoints: Vec<Crosspoint>, labels: Vec<PortLabel>) -> Self {
        Self {
            crosspoints,
            labels,
        }
    }
}

/// Where an effective crosspoint comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(tag = "kind", rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum CrosspointOrigin {
    /// The saved grid ([`Command::SetRouter`](crate::Command::SetRouter) and
    /// the commands that edit it).
    Saved,
    /// The default grid while none is saved: each RX channel to the host
    /// output of the same number, each application output to the TX
    /// channel of the same number (unless a capture bridge feeds it).
    Default,
    /// A `[[route]]` of the configuration files (part of the default grid
    /// while none is saved).
    Configured,
    /// Local monitoring (`monitor_rx_from_tx` / `SetMonitor`) while no grid
    /// is saved: a tap of a TX channel into a host output.
    Monitor,
    /// Implied by a card bridge that feeds network channels (a bridge
    /// without `local_only`): card inputs to its TX channels, or its RX
    /// channels' host outputs to card outputs. Always present while the
    /// bridge runs, saved grid or not; changed through the bridge.
    Bridge { name: String },
}

/// One crosspoint of the effective grid ([`RouterReport`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct CrosspointStatus {
    pub crosspoint: Crosspoint,
    pub origin: CrosspointOrigin,
    /// Both ports exist now (the device has the channels, the card's bridge
    /// is enabled and carries the channel). An inactive saved crosspoint is
    /// kept, not rewritten, and applies again when its ports come back.
    pub active: bool,
    /// Why it is inactive, in words.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub problem: Option<String>,
    /// The connected surface whose fader sets this crosspoint's gain
    /// (fader priority); other origins cannot change it.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub gain_owner: Option<String>,
}

impl CrosspointStatus {
    /// An active crosspoint with no owner.
    #[must_use]
    pub fn new(crosspoint: Crosspoint, origin: CrosspointOrigin) -> Self {
        Self {
            crosspoint,
            origin,
            active: true,
            problem: None,
            gain_owner: None,
        }
    }
}

/// One port of the router and its level ([`RouterReport`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct PortStatus {
    pub port: RouterPort,
    /// The channel label (network ports), the saved [`PortLabel`], or a
    /// default (`CQ in 15`, `App 1`, `Out 1`).
    pub label: String,
    /// `label` was set by the user: a saved [`PortLabel`] (local ports and
    /// host outputs) or a channel label (network ports); `false`: it is the
    /// default. A client renames one port with
    /// [`Command::SetPortLabel`](crate::Command::SetPortLabel) and never
    /// needs to send default labels back. Absent in older JSON: `false`.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "core::ops::Not::not")
    )]
    pub custom_label: bool,
    /// The port exists now. Ports named only by saved crosspoints or levels
    /// are listed with `false`.
    pub available: bool,
    /// Level (unity by default).
    #[cfg_attr(feature = "serde", serde(default))]
    pub gain: Gain,
    #[cfg_attr(feature = "serde", serde(default))]
    pub muted: bool,
    /// The connected surface whose fader sets this level.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub gain_owner: Option<String>,
}

impl PortStatus {
    /// An available port at unity with `label`.
    #[must_use]
    pub fn new(port: RouterPort, label: impl Into<String>) -> Self {
        Self {
            port,
            label: label.into(),
            custom_label: false,
            available: true,
            gain: Gain::default(),
            muted: false,
            gain_owner: None,
        }
    }
}

/// [`Query::Router`](crate::Query::Router): the effective grid.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct RouterReport {
    /// A grid is saved (`SetRouter` or an edit); `false`: the default grid
    /// (identity, `[[route]]`, monitoring) applies.
    pub saved: bool,
    /// Sources first then destinations; network ports by device and
    /// channel, then application ports, then card ports.
    pub ports: Vec<PortStatus>,
    /// Every crosspoint in effect or saved, bridge-implied ones included.
    pub crosspoints: Vec<CrosspointStatus>,
    /// The daemon cannot mix local I/O yet: the grid is shown (as the
    /// running daemon applies it) but cannot be changed beyond what
    /// monitoring, bridges and channel levels express.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "core::ops::Not::not")
    )]
    pub read_only: bool,
}

/// What the daemon last read from the host volume's mixer control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct HostVolumeReading {
    /// The control's first channel, rounded to milli-dB.
    pub gain: Gain,
    /// The control's playback switch is off.
    pub muted: bool,
}

impl HostVolumeReading {
    /// A reading.
    #[must_use]
    pub const fn new(gain: Gain, muted: bool) -> Self {
        Self { gain, muted }
    }
}

/// The host (speaker amplifier) volume: a hardware mixer control the
/// daemon sets (`[host_volume]` in the configuration), in
/// [`Status::host_volume`](crate::Status::host_volume).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct HostVolumeStatus {
    /// The ALSA card id (`amixer -c <card>`).
    pub card: String,
    /// The simple mixer control (`Digital`).
    pub control: String,
    /// Lowest value offered, milli-dB.
    pub min_mdb: i32,
    /// Highest value offered, milli-dB.
    pub max_mdb: i32,
    /// The saved volume ([`Command::SetHostVolume`](crate::Command::SetHostVolume));
    /// `None`: never set, the daemon leaves the control as it is.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub gain: Option<Gain>,
    /// The saved mute ([`Command::SetHostMute`](crate::Command::SetHostMute)).
    #[cfg_attr(feature = "serde", serde(default))]
    pub muted: bool,
    /// What the daemon last read from the control; `None` until read.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub reading: Option<HostVolumeReading>,
    /// The connected surface whose fader sets the volume (fader priority).
    /// Called `owner` in development builds before 0.6.0; still read.
    #[cfg_attr(
        feature = "serde",
        serde(default, alias = "owner", skip_serializing_if = "Option::is_none")
    )]
    pub gain_owner: Option<String>,
    /// Why the daemon cannot read or set the control (card missing, ...).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub problem: Option<String>,
}

impl HostVolumeStatus {
    /// A configured control with nothing saved or read.
    #[must_use]
    pub fn new(
        card: impl Into<String>,
        control: impl Into<String>,
        min_mdb: i32,
        max_mdb: i32,
    ) -> Self {
        Self {
            card: card.into(),
            control: control.into(),
            min_mdb,
            max_mdb,
            gain: None,
            muted: false,
            reading: None,
            gain_owner: None,
            problem: None,
        }
    }
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;

    fn ch(n: u16) -> Channel {
        Channel::new(n).unwrap()
    }

    #[test]
    fn ports_are_tagged_by_kind() {
        let p = RouterPort::CardIn {
            card: "hw:CARD=CQ,DEV=0".into(),
            channel: ch(15),
        };
        let j = serde_json::to_string(&p).unwrap();
        assert_eq!(
            j,
            "{\"kind\":\"card_in\",\"card\":\"hw:CARD=CQ,DEV=0\",\"channel\":15}"
        );
        assert_eq!(serde_json::from_str::<RouterPort>(&j).unwrap(), p);
        assert_eq!(p.kind(), "card_in");
        assert!(!p.is_destination());
        let h = RouterPort::HostOut {
            device: DeviceKey("main".into()),
            channel: ch(1),
        };
        assert!(h.is_destination());
        assert_eq!(h.device().unwrap().0, "main");
        assert_eq!(
            serde_json::to_string(&h).unwrap(),
            "{\"kind\":\"host_out\",\"device\":\"main\",\"channel\":1}"
        );
    }

    #[test]
    fn a_crosspoint_without_gain_or_mute_is_unity_and_live() {
        let j = "{\"source\":{\"kind\":\"network_rx\",\"device\":\"main\",\"channel\":1},\
                 \"destination\":{\"kind\":\"host_out\",\"device\":\"main\",\"channel\":1}}";
        let x: Crosspoint = serde_json::from_str(j).unwrap();
        assert_eq!(x.gain, Gain::MilliDb(0));
        assert!(!x.muted);
        let spec: RouterSpec = serde_json::from_str(&format!("{{\"crosspoints\":[{j}]}}")).unwrap();
        assert!(spec.labels.is_empty());
        assert_eq!(spec.crosspoints, [x]);
    }

    #[test]
    fn origins_and_reports_round_trip() {
        let o = CrosspointOrigin::Bridge { name: "cq".into() };
        let j = serde_json::to_string(&o).unwrap();
        assert_eq!(j, "{\"kind\":\"bridge\",\"name\":\"cq\"}");
        assert_eq!(serde_json::from_str::<CrosspointOrigin>(&j).unwrap(), o);
        let r: RouterReport =
            serde_json::from_str("{\"saved\":false,\"ports\":[],\"crosspoints\":[]}").unwrap();
        assert!(!r.read_only);
        let mut v = HostVolumeStatus::new("Headphones", "Digital", -60_000, 0);
        v.reading = Some(HostVolumeReading::new(Gain::MilliDb(-12_000), true));
        let j = serde_json::to_string(&v).unwrap();
        assert_eq!(serde_json::from_str::<HostVolumeStatus>(&j).unwrap(), v);
        let old: HostVolumeStatus =
            serde_json::from_str("{\"card\":\"x\",\"control\":\"y\",\"min_mdb\":-1,\"max_mdb\":0}")
                .unwrap();
        assert_eq!(old.gain, None);
    }

    #[test]
    fn custom_labels_and_the_volume_owner_in_json() {
        let mut p = PortStatus::new(
            RouterPort::AppOut {
                device: DeviceKey("main".into()),
                channel: ch(1),
            },
            "App 1",
        );
        let j = serde_json::to_string(&p).unwrap();
        assert!(!j.contains("custom_label"), "{j}");
        p.custom_label = true;
        p.label = "Player".into();
        let j = serde_json::to_string(&p).unwrap();
        assert!(j.contains("\"custom_label\":true"), "{j}");
        assert_eq!(serde_json::from_str::<PortStatus>(&j).unwrap(), p);
        let mut v = HostVolumeStatus::new("Headphones", "Digital", -60_000, 0);
        v.gain_owner = Some("cq".into());
        let j = serde_json::to_string(&v).unwrap();
        assert!(j.contains("\"gain_owner\":\"cq\""), "{j}");
        // The name used by development builds is still read.
        let old: HostVolumeStatus = serde_json::from_str(
            "{\"card\":\"x\",\"control\":\"y\",\"min_mdb\":-1,\"max_mdb\":0,\"owner\":\"cq\"}",
        )
        .unwrap();
        assert_eq!(old.gain_owner.as_deref(), Some("cq"));
    }
}
