//! Channel gain and mute, and hardware control surfaces (provisional 0.x).
//!
//! A control surface is a physical device (a USB streaming mixer, a MIDI
//! controller) whose buttons and faders drive channel mute and gain. Its
//! configuration ([`SurfaceSpec`]) is saved intent, changed with
//! [`Command::SetSurface`](crate::Command::SetSurface) and
//! [`Command::RemoveSurface`](crate::Command::RemoveSurface) like any other
//! setting. What the daemon detects and what a surface is doing right now
//! are read with [`Query::Surfaces`](crate::Query::Surfaces).
//!
//! **Fader priority.** While a surface is connected, the channels a
//! non-motorized fader is bound to take their gain from that fader only:
//! [`Command::SetChannelGain`](crate::Command::SetChannelGain) from any other
//! origin is refused with
//! [`CommandError::GainOwned`](crate::CommandError::GainOwned), and
//! [`ChannelStatus::gain_owner`](crate::ChannelStatus) names the surface.
//! Mute is never owned. A motorized fader owns nothing: other origins may set
//! the gain and the surface moves the fader to match.
//!
//! The same rule covers the other levels a fader can be bound to: a port
//! level ([`BindingAction::PortGain`]), a crosspoint gain
//! ([`BindingAction::CrosspointGain`]) and the host volume
//! ([`BindingAction::HostVolume`]); other origins get
//! [`CommandError::LevelOwned`](crate::CommandError::LevelOwned) (or
//! `GainOwned` when the level is a network channel's gain). The output
//! level of RX channels ([`BindingAction::OutputGain`]) stays unowned, as
//! before, unless a `PortGain` fader on its `HostOut` port owns it.
//!
//! **Control routing.** A [`BindingAction::Follow`] binding makes a control
//! act exactly like a control of another surface (a fader on a second MIDI
//! desk acts as a fader of the first: the first one's bindings apply, as
//! that surface, and its motor moves), see [`BindingAction::Follow`] and
//! [`SurfaceBinding::follow`].
//!
//! **Connect out only.** Tessera opens the devices it uses: local ones
//! (`hidraw`, rawmidi) and network ones ([`SurfaceSource::TcpMidi`]). It
//! does not offer a MIDI port of its own for other software to control it.
//!
//! These types are provisional: fields may still be added before 1.0.

use crate::{Channel, DeviceKey, Direction};
use alloc::string::String;
use alloc::vec::Vec;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Lowest settable gain, milli-dB (−96 dB). Lower values are refused; use
/// [`Gain::Off`].
pub const GAIN_FLOOR_MDB: i32 = -96_000;
/// Highest settable gain, milli-dB (+12 dB, the integer sample headroom).
pub const GAIN_CEILING_MDB: i32 = 12_000;

/// Gain of a channel. Not a float: commands are compared for equality and
/// travel as JSON, which has no infinity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum Gain {
    /// Silence (linear 0), as a fader at the bottom of its travel.
    Off,
    /// `GAIN_FLOOR_MDB..=GAIN_CEILING_MDB`, thousandths of a dB. 0 is unity.
    MilliDb(i32),
}

impl Default for Gain {
    /// Unity (0 dB).
    fn default() -> Self {
        Gain::MilliDb(0)
    }
}

impl Gain {
    /// The gain in milli-dB; `None` for [`Gain::Off`].
    #[must_use]
    pub const fn milli_db(self) -> Option<i32> {
        match self {
            Gain::Off => None,
            Gain::MilliDb(m) => Some(m),
        }
    }

    /// Is the value inside the settable range?
    #[must_use]
    pub const fn is_valid(self) -> bool {
        match self {
            Gain::MilliDb(m) => m >= GAIN_FLOOR_MDB && m <= GAIN_CEILING_MDB,
            Gain::Off => true,
        }
    }
}

/// A set of channels of one device in one direction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ChannelSet {
    /// The device key.
    pub device: DeviceKey,
    /// `rx` (network → host) or `tx` (host → network).
    pub direction: Direction,
    /// 1-based device channels, in order, at least one.
    pub channels: Vec<Channel>,
}

impl Default for ChannelSet {
    /// No channels (device `""`, RX): what a binding whose action has no
    /// channel target carries in [`SurfaceBinding::target`].
    fn default() -> Self {
        Self {
            device: DeviceKey(String::new()),
            direction: Direction::Rx,
            channels: Vec::new(),
        }
    }
}

impl ChannelSet {
    /// No channels listed (the [`Default`] value).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }
}

/// A control of a surface, by surface name and control id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct ControlRef {
    /// The [`SurfaceSpec::name`].
    pub surface: String,
    /// The [`SurfaceControl::id`].
    pub control: String,
}

impl ControlRef {
    /// Control `control` of surface `surface`.
    #[must_use]
    pub fn new(surface: impl Into<String>, control: impl Into<String>) -> Self {
        Self {
            surface: surface.into(),
            control: control.into(),
        }
    }
}

/// Where a surface's input comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(tag = "kind", rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum SurfaceSource {
    /// A USB HID interface (`/dev/hidraw*`), matched by USB id and, when a
    /// device has several, the interface number.
    Hidraw {
        vendor_id: u16,
        product_id: u16,
        #[cfg_attr(
            feature = "serde",
            serde(default, skip_serializing_if = "Option::is_none")
        )]
        interface: Option<u8>,
    },
    /// A MIDI device (an ALSA rawmidi port, `/dev/snd/midiC*D*`), matched by
    /// its ALSA card id (`/proc/asound/cards`), for mixing desks and MIDI
    /// controllers. A control's place is its [`MidiAddress`].
    Midi {
        card: String,
        /// Rawmidi device number on the card (default 0).
        #[cfg_attr(feature = "serde", serde(default))]
        device: u8,
    },
    /// MIDI over a TCP connection the daemon opens to `host:port`: raw MIDI
    /// bytes both ways, no framing (the Allen & Heath CQ offers this on port
    /// 51325). Controls are addressed like [`SurfaceSource::Midi`] ones.
    /// The daemon reconnects with back-off while the surface is enabled.
    /// Tessera only connects out; it never listens.
    TcpMidi {
        /// A host name or IP address (1..=253 bytes, no spaces or control
        /// characters).
        host: String,
        /// The TCP port, 1..=65535.
        port: u16,
    },
}

/// The TCP port the Allen & Heath CQ series offers MIDI on.
pub const CQ_TCP_MIDI_PORT: u16 = 51325;

/// Which kind of MIDI message carries a control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum MidiKind {
    /// Control Change `number` (0..=127), value 0..=127.
    ControlChange,
    /// NRPN parameter `number` (MSB << 7 | LSB, 0..=16383), sent as
    /// CC 99/98 then data entry CC 6 (and CC 38 for 14-bit values).
    Nrpn,
    /// Note `number`: on (velocity > 0) is pressed, off is released.
    Note,
}

/// Where a control lives in a MIDI stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct MidiAddress {
    pub kind: MidiKind,
    /// MIDI channel, 1..=16.
    pub channel: u8,
    /// CC number, NRPN parameter or note number.
    pub number: u16,
}

impl MidiAddress {
    /// `kind` `number` on MIDI channel `channel` (1..=16).
    #[must_use]
    pub const fn new(kind: MidiKind, channel: u8, number: u16) -> Self {
        Self {
            kind,
            channel,
            number,
        }
    }
}

/// One point of a fader curve: raw position `raw` means `mdb` milli-dB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct CurvePoint {
    pub raw: u32,
    pub mdb: i32,
}

/// A sound-card bridge saved by command (`Command::SetCardBridge`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct CardBridgeSpec {
    /// Unique bridge name (letters, digits, `-`, `_`).
    pub name: String,
    pub enabled: bool,
    pub device: DeviceKey,
    /// ALSA PCM of the card (`hw:CARD=SC3,DEV=0`).
    pub card: String,
    pub direction: crate::BridgeDirection,
    /// First device channel carried (1-based): TX for capture, RX for
    /// playback.
    pub first_channel: u16,
    pub channels: u16,
    /// Frames per card period; `None`: the daemon default (10 ms).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub period_frames: Option<u32>,
    /// First card channel carried (1-based, default 1): the bridge carries
    /// `channels` card channels from this one on (for example a mixer's
    /// main mix on USB channels 15 and 16: 15). The card is opened with at
    /// least that many channels (or the count it requires); capture ignores
    /// its other channels, playback sends silence on them. Absent in JSON
    /// from clients that predate it: 1.
    #[cfg_attr(feature = "serde", serde(default = "default_card_first_channel"))]
    pub card_first_channel: u16,
    /// Open the card for the host router only: its channels
    /// `card_first_channel..` become router ports (`CardIn` / `CardOut`)
    /// and no device channel is fed or read, so the card stays off the
    /// network until a crosspoint patches it there. `first_channel` is then
    /// ignored (give 1). `false` (absent in older JSON): the bridge feeds
    /// device channels as before, which the router shows as crosspoints of
    /// origin `bridge`. Needs a daemon with the host router.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "core::ops::Not::not")
    )]
    pub local_only: bool,
}

impl CardBridgeSpec {
    /// An enabled bridge carrying `channels` channels between card channel
    /// 1 on and device channel `first_channel` on, with the default period,
    /// feeding the network (not `local_only`).
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        device: DeviceKey,
        card: impl Into<String>,
        direction: crate::BridgeDirection,
        first_channel: u16,
        channels: u16,
    ) -> Self {
        Self {
            name: name.into(),
            enabled: true,
            device,
            card: card.into(),
            direction,
            first_channel,
            channels,
            period_frames: None,
            card_first_channel: DEFAULT_CARD_FIRST_CHANNEL,
            local_only: false,
        }
    }
}

/// Default [`CardBridgeSpec::card_first_channel`]: card channel 1.
pub const DEFAULT_CARD_FIRST_CHANNEL: u16 = 1;

#[cfg(feature = "serde")]
fn default_card_first_channel() -> u16 {
    DEFAULT_CARD_FIRST_CHANNEL
}

/// What kind of physical control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum ControlKind {
    Button,
    Fader,
    Encoder,
}

/// One physical control and, for HID, where it lives in the input report.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct SurfaceControl {
    /// Identifier bindings refer to (letters, digits, `-`, `_`).
    pub id: String,
    /// Shown in the portal; `None`: the id.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub label: Option<String>,
    pub kind: ControlKind,
    /// HID report id; `None` for unnumbered reports.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub report_id: Option<u8>,
    /// HID: bit offset in the report data (after the report id byte).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub offset: Option<u32>,
    /// HID: field size in bits; `None`: 1 for buttons, 8 otherwise.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub bits: Option<u8>,
    /// Fader: largest raw value; `None`: the field's full range.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub max: Option<u32>,
    /// Fader: raw value grows downwards. Encoder: step sign reversed.
    #[cfg_attr(feature = "serde", serde(default))]
    pub inverted: bool,
    /// Button: the bit is 0 while pressed.
    #[cfg_attr(feature = "serde", serde(default))]
    pub active_low: bool,
    /// Fader: has a motor the surface can drive. A motorized fader does not
    /// own its channels' gain (see the module documentation).
    #[cfg_attr(feature = "serde", serde(default))]
    pub motorized: bool,
    /// Non-HID sources: evdev key code or ALSA mixer element.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub code: Option<String>,
    /// MIDI sources: the message that carries this control.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub midi: Option<MidiAddress>,
}

/// What a binding does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum BindingAction {
    /// Buttons: mute and unmute.
    Mute,
    /// Faders and encoders: gain.
    Gain,
    /// Buttons: mute the OUTPUT of RX channels (after the local mix), for
    /// a master or speaker mute.
    OutputMute,
    /// Faders and encoders: OUTPUT level of RX channels (after the local
    /// mix), for a master or speaker level; independent of the channels'
    /// own (input) gain.
    OutputGain,
    /// Faders and encoders: the host (speaker amplifier) volume. No
    /// target. Needs `[host_volume]` in the configuration.
    HostVolume,
    /// Buttons: the host volume's mute. No target.
    HostMute,
    /// Faders and encoders: the level of router ports
    /// ([`SurfaceBinding::ports`]).
    PortGain,
    /// Buttons: the mute of router ports ([`SurfaceBinding::ports`]).
    PortMute,
    /// Faders and encoders: the gain of router crosspoints
    /// ([`SurfaceBinding::crosspoints`]).
    CrosspointGain,
    /// Buttons: the mute of router crosspoints.
    CrosspointMute,
    /// Faders and buttons: act exactly like a control of another surface
    /// ([`SurfaceBinding::follow`]):
    ///
    /// * the followed control's own bindings are applied, with the
    ///   followed surface as the origin (`Origin::Surface` of its name), so
    ///   fader priority treats a move of the follower as a move of the
    ///   followed control;
    /// * a chain A → B → C applies through: when the followed control
    ///   itself follows another, that one's bindings are applied (origin:
    ///   the last surface of the chain);
    /// * the followed desk's control is moved too (its motor, its light).
    ///
    /// A `follow` binding owns no level itself: whether a level is owned is
    /// decided by the followed control (a fader without a motor) and its
    /// surface (connected, enabled), as if the follower were not there. The
    /// device access lock covers a `follow` binding as the bindings it
    /// reaches. A pair of controls following each other is allowed; any
    /// longer cycle is refused.
    Follow,
}

impl BindingAction {
    /// The actions a fader or encoder takes (a level).
    #[must_use]
    pub const fn is_level(self) -> bool {
        matches!(
            self,
            Self::Gain
                | Self::OutputGain
                | Self::HostVolume
                | Self::PortGain
                | Self::CrosspointGain
        )
    }

    /// The actions a button takes (a mute).
    #[must_use]
    pub const fn is_mute(self) -> bool {
        matches!(
            self,
            Self::Mute | Self::OutputMute | Self::HostMute | Self::PortMute | Self::CrosspointMute
        )
    }

    /// The actions that act on [`SurfaceBinding::target`] (a channel set).
    #[must_use]
    pub const fn uses_channels(self) -> bool {
        matches!(
            self,
            Self::Mute | Self::Gain | Self::OutputMute | Self::OutputGain
        )
    }

    /// The level actions whose non-motorized fader owns the level while the
    /// surface is connected (fader priority). `OutputGain` is not one.
    #[must_use]
    pub const fn owns_level(self) -> bool {
        matches!(
            self,
            Self::Gain | Self::HostVolume | Self::PortGain | Self::CrosspointGain
        )
    }
}

/// How a button drives mute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum ButtonMode {
    /// Each press flips mute.
    #[default]
    Toggle,
    /// Muted while held; release restores the state from before the press.
    Momentary,
    /// Live while held, muted otherwise.
    PushToTalk,
}

/// Fader position → gain curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum Taper {
    /// Even travel per dB.
    #[default]
    Db,
    /// Amplitude linear in position.
    Linear,
}

/// One binding: a control and what it acts on. The action decides which
/// target field is used; the others stay empty:
///
/// | action | field |
/// |---|---|
/// | `mute`, `gain`, `output_mute`, `output_gain` | `target` |
/// | `host_volume`, `host_mute` | none |
/// | `port_gain`, `port_mute` | `ports` |
/// | `crosspoint_gain`, `crosspoint_mute` | `crosspoints` |
/// | `follow` | `follow` |
///
/// The fader law (`min_mdb`, `max_mdb`, `taper`, `curve`) applies to every
/// fader binding, `follow` included (it maps this control's position to
/// dB; the followed control's own binding maps dB back to its position).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct SurfaceBinding {
    /// The [`SurfaceControl::id`].
    pub control: String,
    /// What the control does.
    pub action: BindingAction,
    /// Channel actions: the channels. Empty ([`ChannelSet::default`]) for
    /// the other actions, and left out of JSON then. Read leniently: absent,
    /// `null`, `{}` or a table that lists no channel (`device`/`direction`
    /// without `channels`, or `channels = []`) is no target; a table with
    /// channels needs `device` and `direction`.
    #[cfg_attr(
        feature = "serde",
        serde(
            default,
            deserialize_with = "lenient_target",
            skip_serializing_if = "ChannelSet::is_empty"
        )
    )]
    pub target: ChannelSet,
    /// `port_gain` / `port_mute`: the router ports (at least one).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub ports: Vec<crate::RouterPort>,
    /// `crosspoint_gain` / `crosspoint_mute`: the crosspoints (at least
    /// one; each must be patched when the binding is saved).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub crosspoints: Vec<crate::CrosspointKey>,
    /// `follow`: the control of another surface this one drives. A fader
    /// follows a fader, a button a button. The value travels in dB: this
    /// binding's law turns this control's position into dB, and the
    /// followed control's first fader binding (any action) turns it into
    /// that control's position, or the default law (−60…0 dB) when it has
    /// none; a motorized fader moves, a button's light follows. For
    /// feedback both ways, give the other surface a `follow` binding back:
    /// such a pair is allowed, any longer cycle of `follow` bindings is
    /// refused. The followed control's bindings are applied as if it had
    /// moved (see [`BindingAction::Follow`]); the follow binding itself
    /// never owns a level.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub follow: Option<ControlRef>,
    /// Buttons.
    #[cfg_attr(feature = "serde", serde(default))]
    pub mode: ButtonMode,
    /// Faders: gain at the bottom of travel (just above off), milli-dB.
    #[cfg_attr(feature = "serde", serde(default = "default_min_mdb"))]
    pub min_mdb: i32,
    /// Faders: gain at the top of travel, milli-dB.
    #[cfg_attr(feature = "serde", serde(default))]
    pub max_mdb: i32,
    #[cfg_attr(feature = "serde", serde(default))]
    pub taper: Taper,
    /// Encoders: milli-dB per step.
    #[cfg_attr(feature = "serde", serde(default = "default_step_mdb"))]
    pub step_mdb: i32,
    /// Smallest change worth a command, milli-dB.
    #[cfg_attr(feature = "serde", serde(default = "default_deadband_mdb"))]
    pub deadband_mdb: i32,
    /// Faders: a lookup table (raw position to milli-dB, ascending `raw`,
    /// interpolated between points) that replaces `taper`/`min_mdb`/
    /// `max_mdb` when not empty: for desks with their own fader law. Raw 0
    /// is off.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub curve: Vec<CurvePoint>,
}

/// [`SurfaceBinding::target`] as read: a target that lists no channel is
/// [`ChannelSet::default`] whatever else it names (a client may send
/// `"target": {}` or `null` for a binding without channels).
#[cfg(feature = "serde")]
fn lenient_target<'de, D: serde::Deserializer<'de>>(d: D) -> Result<ChannelSet, D::Error> {
    #[derive(Deserialize)]
    struct Partial {
        #[serde(default)]
        device: Option<DeviceKey>,
        #[serde(default)]
        direction: Option<Direction>,
        #[serde(default)]
        channels: Vec<Channel>,
    }
    let Some(p) = Option::<Partial>::deserialize(d)? else {
        return Ok(ChannelSet::default());
    };
    if p.channels.is_empty() {
        return Ok(ChannelSet::default());
    }
    let device = p
        .device
        .ok_or_else(|| serde::de::Error::missing_field("device"))?;
    let direction = p
        .direction
        .ok_or_else(|| serde::de::Error::missing_field("direction"))?;
    Ok(ChannelSet {
        device,
        direction,
        channels: p.channels,
    })
}

#[cfg(feature = "serde")]
fn default_min_mdb() -> i32 {
    -60_000
}
#[cfg(feature = "serde")]
fn default_step_mdb() -> i32 {
    1_000
}
#[cfg(feature = "serde")]
fn default_deadband_mdb() -> i32 {
    250
}

impl SurfaceBinding {
    /// A binding with the usual defaults: toggle, −60…0 dB, dB taper, 1 dB
    /// steps, 0.25 dB deadband.
    #[must_use]
    pub fn new(control: impl Into<String>, action: BindingAction, target: ChannelSet) -> Self {
        Self {
            control: control.into(),
            action,
            target,
            ports: Vec::new(),
            crosspoints: Vec::new(),
            follow: None,
            mode: ButtonMode::Toggle,
            min_mdb: -60_000,
            max_mdb: 0,
            taper: Taper::Db,
            step_mdb: 1_000,
            deadband_mdb: 250,
            curve: Vec::new(),
        }
    }

    /// A binding with the usual defaults and no channel target, for the
    /// host actions (`HostVolume`, `HostMute`).
    #[must_use]
    pub fn host(control: impl Into<String>, action: BindingAction) -> Self {
        Self::new(control, action, ChannelSet::default())
    }

    /// A `PortGain` / `PortMute` binding on `ports`.
    #[must_use]
    pub fn on_ports(
        control: impl Into<String>,
        action: BindingAction,
        ports: Vec<crate::RouterPort>,
    ) -> Self {
        let mut b = Self::host(control, action);
        b.ports = ports;
        b
    }

    /// A `CrosspointGain` / `CrosspointMute` binding on `crosspoints`.
    #[must_use]
    pub fn on_crosspoints(
        control: impl Into<String>,
        action: BindingAction,
        crosspoints: Vec<crate::CrosspointKey>,
    ) -> Self {
        let mut b = Self::host(control, action);
        b.crosspoints = crosspoints;
        b
    }

    /// A `Follow` binding: `control` drives `target`.
    #[must_use]
    pub fn following(control: impl Into<String>, target: ControlRef) -> Self {
        let mut b = Self::host(control, BindingAction::Follow);
        b.follow = Some(target);
        b
    }
}

/// A surface's saved configuration. Build one with [`SurfaceSpec::new`]
/// (fields may be added before 1.0).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct SurfaceSpec {
    /// Unique name (letters, digits, `-`, `_`); also the command origin.
    pub name: String,
    /// `false` keeps the entry but leaves the device alone (and owns
    /// nothing).
    pub enabled: bool,
    pub source: SurfaceSource,
    pub controls: Vec<SurfaceControl>,
    pub bindings: Vec<SurfaceBinding>,
}

/// What kind of device the daemon detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum DetectedKind {
    /// A USB HID interface (`/dev/hidraw*`): can be a surface source.
    Hid,
    /// A sound card: audio through a `[[bridge]]`, mixer elements as an
    /// `alsa_mixer` source.
    SoundCard,
    /// A MIDI port (rawmidi): can be a surface source.
    Midi,
}

/// A device present on the host now.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct DetectedDevice {
    /// Stable for as long as the device stays plugged in (for example
    /// `hid:3142:0c33:3` or `card:SC3`); what [`Query::LearnControl`]
    /// takes.
    ///
    /// [`Query::LearnControl`]: crate::Query::LearnControl
    pub id: String,
    pub kind: DetectedKind,
    /// Product name as the device reports it.
    pub name: String,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub vendor_id: Option<u16>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub product_id: Option<u16>,
    /// USB interface number (HID).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub interface: Option<u8>,
    /// Device node or card name (`/dev/hidraw3`, `SC3`).
    pub node: String,
    /// The surface (or bridge, for a card) that uses it, if any.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub used_by: Option<String>,
    /// The source a new surface for this device would use (HID only).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub source: Option<SurfaceSource>,
    /// Sound cards: capture channels (inputs), when known (`stream0`).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub capture_channels: Option<u16>,
    /// Sound cards: playback channels (outputs), when known.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub playback_channels: Option<u16>,
    /// Sound cards: sample rates the card offers, when known.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub rates: Vec<u32>,
    /// Sound cards: the ALSA PCM a bridge would open (`hw:CARD=SC3,DEV=0`).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub pcm: Option<String>,
}

/// A control's live value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum ControlValue {
    Button { pressed: bool },
    Fader { value: u32, max: u32 },
}

/// One control's live state.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct ControlState {
    pub id: String,
    /// `None` until the device has reported it.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub value: Option<ControlValue>,
}

/// A configured surface and what it is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct SurfaceStatus {
    pub spec: SurfaceSpec,
    /// The device is open and read: the state engine holds the daemon's
    /// connection report for the current [`connection_epoch`](Self::connection_epoch).
    pub connected: bool,
    /// The state engine's connection epoch of this surface (runtime, never
    /// saved): it changes whenever the state engine drops the surface's
    /// connection because the device must be opened again, that is when
    /// the surface is added, enabled, disabled or its `source` changes
    /// (also when two such edits come so fast that the daemon sees only
    /// the last). The surface runtime reports a connection with the epoch
    /// it read ([`Command::ReportSurfaceConnection`](crate::Command::ReportSurfaceConnection)
    /// `epoch`); a session whose report was made under another epoch than
    /// the one it reads now must report the connection again with the new
    /// one, even if it kept its device open. 0 for a surface not changed
    /// since the daemon started (provisional 0.x).
    #[cfg_attr(feature = "serde", serde(default))]
    pub connection_epoch: u64,
    /// The [`DetectedDevice::id`] in use.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub device: Option<String>,
    /// Why it is not connected (not found, permission denied, ...).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub problem: Option<String>,
    pub controls: Vec<ControlState>,
}

/// [`Query::Surfaces`](crate::Query::Surfaces): devices present and
/// surfaces configured.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct SurfacesReport {
    pub detected: Vec<DetectedDevice>,
    pub surfaces: Vec<SurfaceStatus>,
}

/// [`Query::LearnControl`](crate::Query::LearnControl): the control that
/// moved, as a suggested layout for a [`SurfaceControl`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct LearnedControl {
    pub kind: ControlKind,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub report_id: Option<u8>,
    pub offset: u32,
    pub bits: u8,
    /// Fader: the largest raw value seen while learning.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub max: Option<u32>,
    /// Fader: the value went down when moved up (only known if the user was
    /// asked to move it up).
    pub inverted: bool,
    /// Button: the bit read 1 at rest.
    pub active_low: bool,
    /// MIDI sources: the message that moved (then `offset`/`bits` are 0).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub midi: Option<MidiAddress>,
}

/// A named fader law of a profile (for a binding's `curve`). Build one
/// with [`NamedCurve::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct NamedCurve {
    pub name: String,
    pub points: Vec<CurvePoint>,
}

/// [`Query::SurfaceProfiles`](crate::Query::SurfaceProfiles): a shareable
/// surface profile the daemon knows (built in, or a file it loaded): a
/// device model's controls and fader laws, to start a [`SurfaceSpec`] from.
/// The channels its controls act on are chosen when the surface is set up
/// (provisional 0.x).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct SurfaceProfileInfo {
    /// Unique profile name (`allen-heath-cq`).
    pub name: String,
    pub vendor: String,
    pub model: String,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub description: Option<String>,
    /// `builtin`, or the file the profile was loaded from.
    pub origin: String,
    /// Part of the ALSA card id or name a MIDI device of this model has.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub midi_card: Option<String>,
    /// USB vendor id the model has.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub vendor_id: Option<u16>,
    /// USB product id the model has.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub product_id: Option<u16>,
    pub controls: Vec<SurfaceControl>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub curves: Vec<NamedCurve>,
}

impl SurfaceProfileInfo {
    /// A profile with no description, matching keys, controls or curves.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        vendor: impl Into<String>,
        model: impl Into<String>,
        origin: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            vendor: vendor.into(),
            model: model.into(),
            description: None,
            origin: origin.into(),
            midi_card: None,
            vendor_id: None,
            product_id: None,
            controls: Vec::new(),
            curves: Vec::new(),
        }
    }
}

impl DetectedDevice {
    /// A detected device with no USB ids, interface, user or source; set
    /// those fields as known.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        kind: DetectedKind,
        name: impl Into<String>,
        node: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            name: name.into(),
            vendor_id: None,
            product_id: None,
            interface: None,
            node: node.into(),
            used_by: None,
            source: None,
            capture_channels: None,
            playback_channels: None,
            rates: Vec::new(),
            pcm: None,
        }
    }
}

impl ControlState {
    /// A control and its live value (`None` until reported).
    #[must_use]
    pub fn new(id: impl Into<String>, value: Option<ControlValue>) -> Self {
        Self {
            id: id.into(),
            value,
        }
    }
}

impl SurfaceSpec {
    /// An enabled surface.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        source: SurfaceSource,
        controls: Vec<SurfaceControl>,
        bindings: Vec<SurfaceBinding>,
    ) -> Self {
        Self {
            name: name.into(),
            enabled: true,
            source,
            controls,
            bindings,
        }
    }

    /// The same surface, enabled or not.
    #[must_use]
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

impl NamedCurve {
    /// A named fader law.
    #[must_use]
    pub fn new(name: impl Into<String>, points: Vec<CurvePoint>) -> Self {
        Self {
            name: name.into(),
            points,
        }
    }
}

impl SurfaceStatus {
    /// A surface that is not connected, with no live values (connection
    /// epoch 0).
    #[must_use]
    pub fn new(spec: SurfaceSpec) -> Self {
        Self {
            spec,
            connected: false,
            connection_epoch: 0,
            device: None,
            problem: None,
            controls: Vec::new(),
        }
    }
}

impl LearnedControl {
    /// A learned field with no `max`, not inverted, not active-low.
    #[must_use]
    pub fn new(kind: ControlKind, report_id: Option<u8>, offset: u32, bits: u8) -> Self {
        Self {
            kind,
            report_id,
            offset,
            bits,
            max: None,
            inverted: false,
            active_low: false,
            midi: None,
        }
    }
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;

    #[test]
    fn gain_json_shapes() {
        assert_eq!(serde_json::to_string(&Gain::Off).unwrap(), "\"off\"");
        assert_eq!(
            serde_json::to_string(&Gain::MilliDb(-6000)).unwrap(),
            "{\"milli_db\":-6000}"
        );
        let g: Gain = serde_json::from_str("{\"milli_db\":-250}").unwrap();
        assert_eq!(g.milli_db(), Some(-250));
        assert!(!Gain::MilliDb(13_000).is_valid());
        assert!(Gain::MilliDb(GAIN_FLOOR_MDB).is_valid());
    }

    #[test]
    fn a_card_bridge_without_a_card_channel_starts_at_card_channel_one() {
        let old = "{\"name\":\"cq-main\",\"enabled\":true,\"device\":\"main\",\
                   \"card\":\"hw:CARD=CQ,DEV=0\",\"direction\":\"capture\",\
                   \"first_channel\":1,\"channels\":2}";
        let b: CardBridgeSpec = serde_json::from_str(old).unwrap();
        assert_eq!(b.card_first_channel, DEFAULT_CARD_FIRST_CHANNEL);
        let mut sliced = b.clone();
        sliced.card_first_channel = 15;
        let j = serde_json::to_string(&sliced).unwrap();
        assert!(j.contains("\"card_first_channel\":15"), "{j}");
        assert_eq!(serde_json::from_str::<CardBridgeSpec>(&j).unwrap(), sliced);
        // A status row from a daemon that predates it.
        let row: crate::BridgeStatus = serde_json::from_str("{\"name\":\"x\"}").unwrap();
        assert_eq!(row.card_first_channel, 1);
    }

    #[test]
    fn source_json_is_tagged() {
        let s = SurfaceSource::Hidraw {
            vendor_id: 0x3142,
            product_id: 0x0c33,
            interface: Some(3),
        };
        let j = serde_json::to_string(&s).unwrap();
        assert_eq!(
            j,
            "{\"kind\":\"hidraw\",\"vendor_id\":12610,\"product_id\":3123,\"interface\":3}"
        );
        assert_eq!(serde_json::from_str::<SurfaceSource>(&j).unwrap(), s);
        let tcp = SurfaceSource::TcpMidi {
            host: "192.168.1.40".into(),
            port: CQ_TCP_MIDI_PORT,
        };
        let j = serde_json::to_string(&tcp).unwrap();
        assert_eq!(
            j,
            "{\"kind\":\"tcp_midi\",\"host\":\"192.168.1.40\",\"port\":51325}"
        );
        assert_eq!(serde_json::from_str::<SurfaceSource>(&j).unwrap(), tcp);
    }

    #[test]
    fn bindings_without_a_channel_target_leave_it_out() {
        let host = SurfaceBinding::host("main", BindingAction::HostVolume);
        let j = serde_json::to_string(&host).unwrap();
        assert!(!j.contains("\"target\""), "{j}");
        assert!(j.contains("\"action\":\"host_volume\""), "{j}");
        assert_eq!(serde_json::from_str::<SurfaceBinding>(&j).unwrap(), host);
        let f = SurfaceBinding::following(
            "ip1",
            ControlRef {
                surface: "desk2".into(),
                control: "f1".into(),
            },
        );
        let j = serde_json::to_string(&f).unwrap();
        assert!(
            j.contains("\"follow\":{\"surface\":\"desk2\",\"control\":\"f1\"}"),
            "{j}"
        );
        assert_eq!(serde_json::from_str::<SurfaceBinding>(&j).unwrap(), f);
        let port = crate::RouterPort::CardIn {
            card: "hw:CARD=CQ,DEV=0".into(),
            channel: Channel::new(15).unwrap(),
        };
        let p = SurfaceBinding::on_ports("f2", BindingAction::PortGain, vec![port]);
        let j = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<SurfaceBinding>(&j).unwrap(), p);
    }

    #[test]
    fn an_empty_target_reads_as_no_target() {
        let host = SurfaceBinding::host("main", BindingAction::HostVolume);
        for target in [
            "null",
            "{}",
            "{\"device\":\"main\"}",
            "{\"device\":\"main\",\"direction\":\"rx\",\"channels\":[]}",
        ] {
            let j =
                format!("{{\"control\":\"main\",\"action\":\"host_volume\",\"target\":{target}}}");
            let b: SurfaceBinding = serde_json::from_str(&j).unwrap_or_else(|e| panic!("{j}: {e}"));
            assert_eq!(b, host, "{j}");
        }
        // A target with channels still needs its device and direction.
        let j = "{\"control\":\"f\",\"action\":\"gain\",\"target\":{\"device\":\"main\",\"channels\":[1]}}";
        let e = serde_json::from_str::<SurfaceBinding>(j).unwrap_err();
        assert!(e.to_string().contains("direction"), "{e}");
        let j = "{\"control\":\"f\",\"action\":\"gain\",\"target\":\
                 {\"device\":\"main\",\"direction\":\"tx\",\"channels\":[1,2]}}";
        let b: SurfaceBinding = serde_json::from_str(j).unwrap();
        assert_eq!(b.target.device.0, "main");
        assert_eq!(b.target.direction, Direction::Tx);
        assert_eq!(b.target.channels.len(), 2);
    }

    #[test]
    fn action_classes() {
        for a in [
            BindingAction::Gain,
            BindingAction::OutputGain,
            BindingAction::HostVolume,
            BindingAction::PortGain,
            BindingAction::CrosspointGain,
        ] {
            assert!(a.is_level() && !a.is_mute(), "{a:?}");
        }
        for a in [
            BindingAction::Mute,
            BindingAction::OutputMute,
            BindingAction::HostMute,
            BindingAction::PortMute,
            BindingAction::CrosspointMute,
        ] {
            assert!(a.is_mute() && !a.is_level() && !a.owns_level(), "{a:?}");
        }
        assert!(!BindingAction::Follow.is_level() && !BindingAction::Follow.is_mute());
        assert!(!BindingAction::OutputGain.owns_level());
        assert!(BindingAction::HostVolume.owns_level());
        assert!(BindingAction::OutputMute.uses_channels());
        assert!(!BindingAction::PortGain.uses_channels());
    }
}
