//! The `[[surface]]` configuration schema.
//!
//! A surface entry has three parts:
//!
//! * `source`: where input comes from (`hidraw` with a USB id, `evdev` with a
//!   device name, `alsa_mixer` watching a card's mixer controls).
//! * `control`: the physical controls and, for raw HID, where each lives in
//!   the input report (report id, bit offset, bit count; HID fields are
//!   little-endian and need not be byte-aligned). The layout is
//!   configuration, not code, so a new device (or a firmware revision) needs
//!   no rebuild.
//! * `binding`: what each control does (mute or gain of a channel set).
//!
//! ```toml
//! [[surface]]
//! name = "desk-mixer"
//! source = { kind = "hidraw", vendor_id = 0x3142, product_id = 0x0c33 }
//!
//! [[surface.control]]
//! id = "mic-mute"
//! kind = "button"
//! report_id = 1
//! offset = 8       # bit 0 of the byte after the report id is offset 0
//!
//! [[surface.control]]
//! id = "mic-fader"
//! kind = "fader"
//! report_id = 1
//! offset = 16
//! bits = 12
//! max = 4095
//!
//! [[surface.binding]]
//! control = "mic-mute"
//! action = "mute"
//! mode = "toggle"
//! target = { device = "main", direction = "tx", channels = [1, 2] }
//!
//! [[surface.binding]]
//! control = "mic-fader"
//! action = "gain"
//! min_db = -60.0
//! max_db = 0.0
//! target = { device = "main", direction = "tx", channels = [1, 2] }
//! ```
//!
//! This derived schema is for the sans-IO core and its tests. When the
//! daemon reads `[[surface]]`, it goes through `tessera-state`'s
//! hand-written config reader (ADR 0008), which reports every error in one
//! pass; [`SurfaceConfig::validate`] then holds the semantic checks.

use serde::{Deserialize, Serialize};
pub use tessera_control_types::{CrosspointKey, CurvePoint, MidiAddress, MidiKind, RouterPort};
use tessera_control_types::{Direction, GAIN_CEILING_MDB, GAIN_FLOOR_MDB};

/// Lowest gain a fader may be configured to reach, dB. Below it a fader at
/// its bottom end means "off" (linear 0), not a very small gain. Matches the
/// media gain word's floor (`tessera-media` `gain::MIN_DB`).
pub const GAIN_FLOOR_DB: f32 = -96.0;
/// Highest gain a fader may be configured to reach, dB. Headroom above unity
/// is limited by the integer sample path, which saturates; matches
/// `tessera-media` `gain::MAX_DB`.
pub const GAIN_CEILING_DB: f32 = 12.0;

/// Widest HID field the decoder reads, bits.
pub const MAX_FIELD_BITS: u8 = 32;

/// Longest surface name or control id, bytes.
pub const MAX_IDENTIFIER_BYTES: usize = 64;

/// A surface name or control id: 1..=[`MAX_IDENTIFIER_BYTES`] ASCII letters,
/// digits, `-` or `_`.
#[must_use]
pub fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_IDENTIFIER_BYTES
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// One `[[surface]]` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceConfig {
    /// Name of the surface (unique), used in logs, status and as the origin
    /// of its commands.
    pub name: String,
    /// `false` keeps the entry but leaves the device alone.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Where the input comes from.
    pub source: SourceConfig,
    /// The physical controls.
    #[serde(default, rename = "control")]
    pub controls: Vec<ControlConfig>,
    /// What the controls do.
    #[serde(default, rename = "binding")]
    pub bindings: Vec<BindingConfig>,
}

fn yes() -> bool {
    true
}

/// Where a surface's input comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceConfig {
    /// A USB HID interface read through `/dev/hidraw*`, matched by USB id
    /// (and optionally the interface number when a device has several).
    Hidraw {
        vendor_id: u16,
        product_id: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interface: Option<u8>,
    },
    /// A kernel input device (`/dev/input/event*`) matched by its name, for
    /// devices whose buttons arrive as keys (`KEY_MUTE`, `KEY_VOLUMEUP`).
    Evdev { name: String },
    /// Mixer controls of an ALSA card, for devices whose hardware controls
    /// move the card's own mixer elements. Read only: tessera never writes
    /// the elements it watches (that would fight the hardware).
    AlsaMixer { card: String },
    /// A MIDI port (ALSA rawmidi `/dev/snd/midiC*D*`) of the card whose id
    /// is `card`, for mixing desks and MIDI controllers. Each control names
    /// its message with `midi`.
    Midi {
        card: String,
        /// Rawmidi device number on the card.
        #[serde(default)]
        device: u8,
    },
    /// Raw MIDI bytes both ways over a TCP connection tessera opens to
    /// `host:port` (the Allen & Heath CQ: port 51325). Controls are
    /// addressed like a `midi` source's.
    TcpMidi { host: String, port: u16 },
}

impl SourceConfig {
    /// A source whose controls are MIDI messages (`midi`, `tcp_midi`).
    #[must_use]
    pub fn is_midi(&self) -> bool {
        matches!(self, Self::Midi { .. } | Self::TcpMidi { .. })
    }
}

/// What kind of physical control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    /// Two states: pressed / released.
    Button,
    /// An absolute position, `0..=max`.
    Fader,
    /// A step source (encoder, volume up/down keys): signed deltas.
    Encoder,
}

/// One physical control.
///
/// For a `hidraw` source the layout keys say where the control lives in the
/// input report; other sources name the control with `code` (an evdev key
/// code or an ALSA mixer element name) and leave the layout keys out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlConfig {
    /// Identifier bindings refer to.
    pub id: String,
    pub kind: ControlKind,
    /// HID report id; `None`: the device uses unnumbered reports (the first
    /// byte is data, not an id). All controls of a surface agree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_id: Option<u8>,
    /// Bit offset of the field in the report data (after the report id
    /// byte): offset 0 is the least significant bit of the first data byte,
    /// as in a HID report descriptor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
    /// Field size in bits, little-endian. Default: 1 for buttons, 8
    /// otherwise. Buttons are one bit; encoder fields are signed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bits: Option<u8>,
    /// Fader: largest raw value (bottom is 0). `None`: the field's full
    /// range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
    /// Fader: the raw value grows downwards (top of travel is 0).
    /// Encoder: the step sign is reversed.
    #[serde(default)]
    pub inverted: bool,
    /// Button: the bit is 0 while pressed.
    #[serde(default)]
    pub active_low: bool,
    /// Fader: has a motor the surface can drive. A motorized fader owns no
    /// gain: when the gain changes elsewhere the mapper asks for the fader
    /// to be moved ([`SurfaceAction::MoveFader`]). A fader without a motor
    /// is authoritative while its surface is connected.
    ///
    /// [`SurfaceAction::MoveFader`]: crate::SurfaceAction::MoveFader
    #[serde(default)]
    pub motorized: bool,
    /// Non-HID sources: evdev key code name or ALSA mixer element.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// MIDI sources: the message that carries this control (CC, NRPN or
    /// note, and its channel).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub midi: Option<MidiAddress>,
}

impl ControlConfig {
    /// Field size in bits, with the per-kind default.
    #[must_use]
    pub fn field_bits(&self) -> u8 {
        self.bits.unwrap_or(match self.kind {
            ControlKind::Button => 1,
            ControlKind::Fader | ControlKind::Encoder => 8,
        })
    }

    /// Largest value the field can hold. A MIDI control: 127 for a CC or a
    /// note, 16383 for an NRPN.
    #[must_use]
    pub fn field_max(&self) -> u32 {
        if let Some(m) = &self.midi {
            return match m.kind {
                MidiKind::Nrpn => u32::from(crate::midi::MAX_14BIT),
                _ => 127,
            };
        }
        match self.field_bits() {
            b if b >= 32 => u32::MAX,
            b => (1u32 << b) - 1,
        }
    }
}

/// Which tessera channels a binding acts on.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    /// The `device.id`.
    pub device: String,
    /// `rx` (network → host) or `tx` (host → network).
    pub direction: Direction,
    /// 1-based device channels; order is kept.
    pub channels: Vec<u16>,
}

impl Target {
    /// No channels: the target of a binding whose action has another (or
    /// no) target.
    #[must_use]
    pub fn none() -> Self {
        Self {
            device: String::new(),
            direction: Direction::Rx,
            channels: Vec::new(),
        }
    }

    /// Lists no channel.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }

    /// Does this target include `channel` of `device`/`direction`?
    #[must_use]
    pub fn covers(&self, device: &str, direction: Direction, channel: u16) -> bool {
        self.device == device && self.direction == direction && self.channels.contains(&channel)
    }
}

/// How a button drives mute.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ButtonMode {
    /// Each press flips mute.
    #[default]
    Toggle,
    /// Muted while held ("cough" button); release restores the mute state
    /// from before the press.
    Momentary,
    /// Unmuted while held, muted otherwise (also muted when the surface
    /// starts).
    PushToTalk,
}

/// Fader position → gain curve.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Taper {
    /// dB linear in position: even travel per dB. The usual console feel.
    #[default]
    Db,
    /// Amplitude linear in position (what a plain potentiometer does).
    Linear,
}

/// What a binding does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingAction {
    /// Mute/unmute (buttons).
    Mute,
    /// Set gain (faders, encoders).
    Gain,
    /// Mute/unmute the OUTPUT of RX channels, after the local mix (buttons):
    /// a master or speaker mute.
    OutputMute,
    /// Set the OUTPUT level of RX channels, after the local mix (faders,
    /// encoders): a master or speaker level, independent of the channels'
    /// own gain.
    OutputGain,
    /// The host (speaker amplifier) volume (faders, encoders). No target.
    HostVolume,
    /// The host volume's mute (buttons). No target.
    HostMute,
    /// The level of router ports (`ports`; faders, encoders).
    PortGain,
    /// The mute of router ports (`ports`; buttons).
    PortMute,
    /// The gain of router crosspoints (`crosspoints`; faders, encoders).
    CrosspointGain,
    /// The mute of router crosspoints (`crosspoints`; buttons).
    CrosspointMute,
    /// Drive a control of another surface (`follow`): a fader its fader, a
    /// button its button's light.
    Follow,
}

impl BindingAction {
    /// A mute (buttons).
    #[must_use]
    pub fn is_mute(self) -> bool {
        matches!(
            self,
            Self::Mute | Self::OutputMute | Self::HostMute | Self::PortMute | Self::CrosspointMute
        )
    }

    /// A level (faders, encoders).
    #[must_use]
    pub fn is_gain(self) -> bool {
        matches!(
            self,
            Self::Gain
                | Self::OutputGain
                | Self::HostVolume
                | Self::PortGain
                | Self::CrosspointGain
        )
    }

    /// Acts on `target` (a channel set).
    #[must_use]
    pub fn uses_channels(self) -> bool {
        matches!(
            self,
            Self::Mute | Self::Gain | Self::OutputMute | Self::OutputGain
        )
    }

    /// Acts on the output stage (after the local mix), which only RX
    /// channels have.
    #[must_use]
    pub fn is_output(self) -> bool {
        matches!(self, Self::OutputMute | Self::OutputGain)
    }
}

/// One `[[surface.binding]]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingConfig {
    /// The `control.id` this binding listens to.
    pub control: String,
    pub action: BindingAction,
    /// Channel actions (`mute`, `gain`, `output_*`): the channels.
    #[serde(default = "Target::none", skip_serializing_if = "Target::is_empty")]
    pub target: Target,
    /// `port_gain` / `port_mute`: the router ports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<RouterPort>,
    /// `crosspoint_gain` / `crosspoint_mute`: the router crosspoints.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crosspoints: Vec<CrosspointKey>,
    /// `follow`: the control of another surface this one drives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow: Option<FollowTarget>,
    /// Buttons bound to `mute`.
    #[serde(default)]
    pub mode: ButtonMode,
    /// Faders bound to `gain`: gain at the bottom of travel, dB. At the very
    /// bottom the channel is off (linear 0) whatever this is.
    #[serde(default = "default_min_db")]
    pub min_db: f32,
    /// Faders bound to `gain`: gain at the top of travel, dB.
    #[serde(default)]
    pub max_db: f32,
    #[serde(default)]
    pub taper: Taper,
    /// Encoders bound to `gain`: dB per step.
    #[serde(default = "default_step_db")]
    pub step_db: f32,
    /// Smallest gain change worth a command, dB (fader noise and the
    /// command rate stay bounded).
    #[serde(default = "default_deadband_db")]
    pub deadband_db: f32,
    /// Faders: a fader law (raw position → milli-dB, ascending, linear
    /// between points, clamped to the ends; raw 0 is off). When not empty
    /// it replaces `taper`, and `min_db`/`max_db` are its first and last
    /// gain ([`BindingConfig::apply_curve_range`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub curve: Vec<CurvePoint>,
}

/// A control of a surface, which a `follow` binding drives.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FollowTarget {
    /// The surface's name.
    pub surface: String,
    /// The control's id.
    pub control: String,
}

/// One Tessera value a binding acts on, in canonical form: a router port
/// that is a network channel is that channel's gain, a host output the RX
/// channel's output level (so one level has one key, whichever action names
/// it).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Level {
    /// A device channel's gain and mute (`output`: its output stage).
    Channel {
        output: bool,
        device: String,
        direction: Direction,
        channel: u16,
    },
    /// The host volume and its mute.
    Host,
    /// A local router port's level (`app_out`, `card_in`, `card_out`).
    Port(RouterPort),
    /// A crosspoint's gain and mute.
    Crosspoint(CrosspointKey),
}

impl Level {
    /// The canonical level of a router port.
    #[must_use]
    pub fn of_port(p: &RouterPort) -> Self {
        let ch = |c: tessera_control_types::Channel| c.get();
        match p {
            RouterPort::NetworkRx { device, channel } => Self::Channel {
                output: false,
                device: device.0.clone(),
                direction: Direction::Rx,
                channel: ch(*channel),
            },
            RouterPort::NetworkTx { device, channel } => Self::Channel {
                output: false,
                device: device.0.clone(),
                direction: Direction::Tx,
                channel: ch(*channel),
            },
            RouterPort::HostOut { device, channel } => Self::Channel {
                output: true,
                device: device.0.clone(),
                direction: Direction::Rx,
                channel: ch(*channel),
            },
            other => Self::Port(other.clone()),
        }
    }
}

impl BindingConfig {
    /// The values this binding acts on (none for `follow`).
    #[must_use]
    pub fn levels(&self) -> Vec<Level> {
        match self.action {
            a if a.uses_channels() => self
                .target
                .channels
                .iter()
                .map(|&channel| Level::Channel {
                    output: a.is_output(),
                    device: self.target.device.clone(),
                    direction: self.target.direction,
                    channel,
                })
                .collect(),
            BindingAction::HostVolume | BindingAction::HostMute => vec![Level::Host],
            BindingAction::PortGain | BindingAction::PortMute => {
                self.ports.iter().map(Level::of_port).collect()
            }
            BindingAction::CrosspointGain | BindingAction::CrosspointMute => self
                .crosspoints
                .iter()
                .cloned()
                .map(Level::Crosspoint)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// With a `curve`: set `min_db`/`max_db` to its first and last gain,
    /// which the mapper's off-point hysteresis, end stops and deadband use.
    pub fn apply_curve_range(&mut self) {
        if let (Some(first), Some(last)) = (self.curve.first(), self.curve.last()) {
            self.min_db = db(first.mdb);
            self.max_db = db(last.mdb);
        }
    }
}

/// Checks a fader law: at least two points, `raw` and `mdb` both strictly
/// ascending, `raw` above 0 (0 is always off), gains within the settable
/// range.
///
/// # Errors
/// What is wrong, in words.
pub fn check_curve(curve: &[CurvePoint]) -> Result<(), String> {
    if curve.len() < 2 {
        return Err("a curve needs at least two points".into());
    }
    if curve[0].raw == 0 {
        return Err("raw 0 is always off; start the curve above 0".into());
    }
    if let Some(p) = curve
        .iter()
        .find(|p| !(GAIN_FLOOR_MDB..=GAIN_CEILING_MDB).contains(&p.mdb))
    {
        return Err(format!(
            "point raw {}: {} mdB is outside {GAIN_FLOOR_MDB}..={GAIN_CEILING_MDB}",
            p.raw, p.mdb
        ));
    }
    if let Some(w) = curve
        .windows(2)
        .find(|w| w[1].raw <= w[0].raw || w[1].mdb <= w[0].mdb)
    {
        return Err(format!(
            "points must ascend in both raw and mdb (raw {} then raw {})",
            w[0].raw, w[1].raw
        ));
    }
    Ok(())
}

fn default_min_db() -> f32 {
    -60.0
}
fn default_step_db() -> f32 {
    1.0
}
fn default_deadband_db() -> f32 {
    0.25
}

/// A configuration mistake, with the surface and the offending item named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl core::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

impl SurfaceConfig {
    /// Checks the entry on its own (device ids and channel ranges are the
    /// state crate's to check against the device list).
    ///
    /// # Errors
    /// The first problem found.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let err = |m: String| Err(ConfigError(format!("surface {:?}: {m}", self.name)));
        if !is_identifier(&self.name) {
            return err(format!(
                "the name must be 1..={MAX_IDENTIFIER_BYTES} letters, digits, `-` or `_`"
            ));
        }
        let hid = matches!(self.source, SourceConfig::Hidraw { .. });
        let midi = self.source.is_midi();
        if let SourceConfig::TcpMidi { host, port } = &self.source {
            let ok = !host.is_empty()
                && host.len() <= 253
                && !host.chars().any(|c| c.is_whitespace() || c.is_control());
            if !ok || *port == 0 {
                return err(
                    "tcp_midi needs a host name or address (1..=253 bytes, no spaces) and a \
                     port 1..=65535"
                        .into(),
                );
            }
        }
        let numbered = self.controls.first().map(|c| c.report_id.is_some());
        for (i, c) in self.controls.iter().enumerate() {
            if !is_identifier(&c.id) {
                return err(format!(
                    "control {:?}: the id must be 1..={MAX_IDENTIFIER_BYTES} letters, digits, \
                     `-` or `_`",
                    c.id
                ));
            }
            if self.controls[..i].iter().any(|o| o.id == c.id) {
                return err(format!("control {:?} is defined twice", c.id));
            }
            if c.motorized && c.kind != ControlKind::Fader {
                return err(format!(
                    "control {:?}: only a fader can be `motorized`",
                    c.id
                ));
            }
            if c.midi.is_some() != midi {
                return err(format!(
                    "control {:?}: `midi` is needed for a midi source and only there",
                    c.id
                ));
            }
            if let Some(m) = &c.midi {
                if c.offset.is_some() || c.report_id.is_some() || c.bits.is_some() {
                    return err(format!(
                        "control {:?}: a midi control has no `offset`, `report_id` or `bits`",
                        c.id
                    ));
                }
                if c.kind == ControlKind::Encoder {
                    return err(format!(
                        "control {:?}: encoders are not read from midi",
                        c.id
                    ));
                }
                let top = match m.kind {
                    MidiKind::Nrpn => crate::midi::MAX_14BIT,
                    _ => 127,
                };
                if !(1..=16).contains(&m.channel) || m.number > top {
                    return err(format!(
                        "control {:?}: midi channel must be 1..=16 and the number 0..={top}",
                        c.id
                    ));
                }
                if c.max.is_some_and(|x| x > c.field_max()) {
                    return err(format!(
                        "fader {:?}: `max` is beyond what the message carries ({})",
                        c.id,
                        c.field_max()
                    ));
                }
                if self.controls[..i].iter().any(|o| o.midi == c.midi) {
                    return err(format!(
                        "control {:?}: another control already uses this midi message",
                        c.id
                    ));
                }
            } else if hid {
                if c.report_id == Some(0) {
                    return err(format!(
                        "control {:?}: report id 0 is reserved (leave `report_id` out for \
                         unnumbered reports)",
                        c.id
                    ));
                }
                if c.report_id.is_some() != numbered.unwrap_or(false) {
                    return err(format!(
                        "control {:?}: either every control has `report_id` or none does",
                        c.id
                    ));
                }
                let Some(offset) = c.offset else {
                    return err(format!(
                        "control {:?} needs `offset` for a hidraw source",
                        c.id
                    ));
                };
                let bits = c.field_bits();
                let bits_ok = match c.kind {
                    ControlKind::Button => bits == 1,
                    ControlKind::Fader => (1..=MAX_FIELD_BITS).contains(&bits),
                    ControlKind::Encoder => (2..=MAX_FIELD_BITS).contains(&bits),
                };
                if !bits_ok {
                    return err(format!(
                        "control {:?}: `bits` must be 1 for a button, 1..={MAX_FIELD_BITS} for a \
                         fader, 2..={MAX_FIELD_BITS} for an encoder",
                        c.id
                    ));
                }
                if offset.checked_add(u32::from(bits)).is_none() {
                    return err(format!("control {:?}: `offset` is out of range", c.id));
                }
                if c.max.is_some_and(|m| m > c.field_max()) {
                    return err(format!(
                        "fader {:?}: `max` does not fit in {bits} bits, the top of travel would \
                         be unreachable",
                        c.id
                    ));
                }
            } else if c.code.is_none() {
                return err(format!("control {:?} needs `code` for this source", c.id));
            }
            if c.kind == ControlKind::Fader && c.max == Some(0) {
                return err(format!("fader {:?}: `max` must be above 0", c.id));
            }
        }
        for (i, b) in self.bindings.iter().enumerate() {
            let Some(c) = self.controls.iter().find(|c| c.id == b.control) else {
                return err(format!("binding names unknown control {:?}", b.control));
            };
            let follow = b.action == BindingAction::Follow;
            let fits = match c.kind {
                ControlKind::Button => b.action.is_mute() || follow,
                ControlKind::Fader => b.action.is_gain() || follow,
                ControlKind::Encoder => b.action.is_gain(),
            };
            if !fits {
                return err(format!(
                    "control {:?} is a {:?} and cannot be bound to {:?}",
                    c.id, c.kind, b.action
                ));
            }
            self.check_target_fields(b).or_else(err)?;
            if b.action.is_output() && b.target.direction != Direction::Rx {
                return err(format!(
                    "binding of {:?}: only RX channels have an output level and mute",
                    b.control
                ));
            }
            if !b.curve.is_empty() && c.kind != ControlKind::Fader {
                return err(format!(
                    "binding of {:?}: only a fader takes a `curve`",
                    b.control
                ));
            }
            if !b.curve.is_empty() {
                check_curve(&b.curve)
                    .or_else(|m| err(format!("binding of {:?}: curve: {m}", b.control)))?;
            }
            let mut b = b.clone();
            b.apply_curve_range();
            let b = &b;
            let range = GAIN_FLOOR_DB..=GAIN_CEILING_DB;
            if !range.contains(&b.min_db) || !range.contains(&b.max_db) || b.min_db >= b.max_db {
                return err(format!(
                    "binding of {:?}: need {GAIN_FLOOR_DB} <= min_db < max_db <= {GAIN_CEILING_DB}",
                    b.control
                ));
            }
            let span = b.max_db - b.min_db;
            let deadband_ok = b.deadband_db.is_finite() && (0.0..=span).contains(&b.deadband_db);
            // Steps are used only by encoders, so only they are held to the span.
            let step_ok = b.step_db.is_finite()
                && b.step_db > 0.0
                && (c.kind != ControlKind::Encoder || b.step_db <= span);
            if !deadband_ok || !step_ok {
                return err(format!(
                    "binding of {:?}: deadband_db must be 0..={span} and step_db above 0 and at \
                     most {span}",
                    b.control
                ));
            }
            // Two gain bindings sharing a level would not see each other's
            // changes (they are one origin), so a fader and an encoder on the
            // same channel could jump the level. The channel gain and the
            // output level are different stages: one binding of each on a
            // channel is fine (different levels).
            let mine = b.levels();
            let overlap = b.action.is_gain()
                && self.bindings[..i]
                    .iter()
                    .any(|o| o.action.is_gain() && o.levels().iter().any(|l| mine.contains(l)));
            if overlap {
                return err(format!(
                    "binding of {:?}: another gain binding of this surface already covers one \
                     of its channels",
                    b.control
                ));
            }
        }
        Ok(())
    }

    /// The target fields fit the action: channel actions `target`, host
    /// actions none, port actions `ports`, crosspoint actions
    /// `crosspoints`, `follow` its control.
    fn check_target_fields(&self, b: &BindingConfig) -> Result<(), String> {
        let of = |m: &str| Err(format!("binding of {:?}: {m}", b.control));
        let (target, ports, crosspoints, follow) = (
            !b.target.is_empty(),
            !b.ports.is_empty(),
            !b.crosspoints.is_empty(),
            b.follow.is_some(),
        );
        match b.action {
            a if a.uses_channels() => {
                if b.target.channels.is_empty() || b.target.channels.contains(&0) {
                    return of("`channels` must list 1-based channels");
                }
                if ports || crosspoints || follow {
                    return of("a channel binding has only `target`");
                }
            }
            BindingAction::HostVolume | BindingAction::HostMute => {
                // Say which field to remove, in the state's words.
                let extra = [
                    (target, "target", "channels"),
                    (ports, "ports", "router ports"),
                    (crosspoints, "crosspoints", "crosspoints"),
                    (follow, "follow", "another surface's control"),
                ]
                .into_iter()
                .find(|(set, _, _)| *set);
                if let Some((_, field, what)) = extra {
                    return of(&format!(
                        "remove `{field}`: {}, not on {what}",
                        host_binding_acts_on(b.action)
                    ));
                }
            }
            BindingAction::PortGain | BindingAction::PortMute => {
                if !ports || target || crosspoints || follow {
                    return of("a port binding needs `ports` (and nothing else)");
                }
            }
            BindingAction::CrosspointGain | BindingAction::CrosspointMute => {
                if !crosspoints || target || ports || follow {
                    return of("a crosspoint binding needs `crosspoints` (and nothing else)");
                }
                if b.crosspoints
                    .iter()
                    .any(|x| !x.destination.is_destination() || x.source == x.destination)
                {
                    return of("a crosspoint runs from a port to another, destination, port");
                }
            }
            _ => {
                let Some(f) = &b.follow else {
                    return of("a follow binding needs `follow` (and nothing else)");
                };
                if target || ports || crosspoints {
                    return of("a follow binding needs `follow` (and nothing else)");
                }
                if !is_identifier(&f.surface) || !is_identifier(&f.control) {
                    return of("`follow` names a surface and one of its controls");
                }
                if f.surface == self.name && f.control == b.control {
                    return of("a control cannot follow itself");
                }
            }
        }
        Ok(())
    }
}

/// What a host binding acts on, in the words the state uses when it refuses
/// a host binding with a target: `a host volume binding acts on the host
/// volume`.
pub(crate) fn host_binding_acts_on(action: BindingAction) -> &'static str {
    if action == BindingAction::HostMute {
        "a host mute binding acts on the host mute"
    } else {
        "a host volume binding acts on the host volume"
    }
}

/// milli-dB → dB.
#[allow(clippy::cast_precision_loss)]
fn db(mdb: i32) -> f32 {
    mdb as f32 / 1000.0
}

impl From<&tessera_control_types::SurfaceSpec> for SurfaceConfig {
    /// The saved surface as this crate's configuration (milli-dB become dB,
    /// contract channels 1-based numbers). Run [`SurfaceConfig::validate`]
    /// on the result: the conversion itself checks nothing.
    fn from(spec: &tessera_control_types::SurfaceSpec) -> Self {
        use tessera_control_types as ct;
        let source = match &spec.source {
            ct::SurfaceSource::Hidraw {
                vendor_id,
                product_id,
                interface,
            } => SourceConfig::Hidraw {
                vendor_id: *vendor_id,
                product_id: *product_id,
                interface: *interface,
            },
            ct::SurfaceSource::Midi { card, device } => SourceConfig::Midi {
                card: card.clone(),
                device: *device,
            },
            ct::SurfaceSource::TcpMidi { host, port } => SourceConfig::TcpMidi {
                host: host.clone(),
                port: *port,
            },
            // A source this build does not know matches no device.
            _ => SourceConfig::Hidraw {
                vendor_id: 0,
                product_id: 0,
                interface: None,
            },
        };
        let controls = spec
            .controls
            .iter()
            .map(|c| ControlConfig {
                id: c.id.clone(),
                kind: match c.kind {
                    ct::ControlKind::Button => ControlKind::Button,
                    ct::ControlKind::Fader => ControlKind::Fader,
                    ct::ControlKind::Encoder => ControlKind::Encoder,
                    // Kinds, actions, modes and tapers a newer contract adds fall back
                    // to the plainest meaning; validation then refuses what does not fit.
                    _ => ControlKind::Button,
                },
                report_id: c.report_id,
                offset: c.offset,
                bits: c.bits,
                max: c.max,
                inverted: c.inverted,
                active_low: c.active_low,
                motorized: c.motorized,
                code: c.code.clone(),
                midi: c.midi,
            })
            .collect();
        let bindings = spec
            .bindings
            .iter()
            .map(|b| BindingConfig {
                control: b.control.clone(),
                action: match b.action {
                    ct::BindingAction::Mute => BindingAction::Mute,
                    ct::BindingAction::Gain => BindingAction::Gain,
                    ct::BindingAction::OutputMute => BindingAction::OutputMute,
                    ct::BindingAction::OutputGain => BindingAction::OutputGain,
                    ct::BindingAction::HostVolume => BindingAction::HostVolume,
                    ct::BindingAction::HostMute => BindingAction::HostMute,
                    ct::BindingAction::PortGain => BindingAction::PortGain,
                    ct::BindingAction::PortMute => BindingAction::PortMute,
                    ct::BindingAction::CrosspointGain => BindingAction::CrosspointGain,
                    ct::BindingAction::CrosspointMute => BindingAction::CrosspointMute,
                    ct::BindingAction::Follow => BindingAction::Follow,
                    _ => BindingAction::Mute,
                },
                target: Target {
                    device: b.target.device.0.clone(),
                    direction: b.target.direction,
                    channels: b.target.channels.iter().map(|c| c.get()).collect(),
                },
                ports: b.ports.clone(),
                crosspoints: b.crosspoints.clone(),
                follow: b.follow.as_ref().map(|f| FollowTarget {
                    surface: f.surface.clone(),
                    control: f.control.clone(),
                }),
                mode: match b.mode {
                    ct::ButtonMode::Toggle => ButtonMode::Toggle,
                    ct::ButtonMode::Momentary => ButtonMode::Momentary,
                    ct::ButtonMode::PushToTalk => ButtonMode::PushToTalk,
                    _ => ButtonMode::Toggle,
                },
                min_db: db(b.min_mdb),
                max_db: db(b.max_mdb),
                taper: match b.taper {
                    ct::Taper::Db => Taper::Db,
                    ct::Taper::Linear => Taper::Linear,
                    _ => Taper::Db,
                },
                step_db: db(b.step_mdb),
                deadband_db: db(b.deadband_mdb),
                curve: b.curve.clone(),
            })
            .map(|mut b: BindingConfig| {
                b.apply_curve_range();
                b
            })
            .collect();
        Self {
            name: spec.name.clone(),
            enabled: spec.enabled,
            source,
            controls,
            bindings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct File {
        surface: Vec<SurfaceConfig>,
    }

    const SAMPLE: &str = r#"
[[surface]]
name = "desk-mixer"
source = { kind = "hidraw", vendor_id = 0x3142, product_id = 0x0c33 }

[[surface.control]]
id = "mic-mute"
kind = "button"
report_id = 1
offset = 8

[[surface.control]]
id = "mic-fader"
kind = "fader"
report_id = 1
offset = 16
bits = 12
max = 4095

[[surface.binding]]
control = "mic-mute"
action = "mute"
target = { device = "main", direction = "tx", channels = [1, 2] }

[[surface.binding]]
control = "mic-fader"
action = "gain"
max_db = 6.0
target = { device = "main", direction = "tx", channels = [1, 2] }
"#;

    fn sample() -> SurfaceConfig {
        toml::from_str::<File>(SAMPLE).unwrap().surface.remove(0)
    }

    #[test]
    fn sample_parses_with_defaults() {
        let s = sample();
        assert!(s.enabled);
        assert_eq!(
            s.source,
            SourceConfig::Hidraw {
                vendor_id: 0x3142,
                product_id: 0x0c33,
                interface: None
            }
        );
        assert_eq!(s.controls[0].field_bits(), 1);
        assert_eq!(s.controls[1].field_max(), 4095);
        assert_eq!(s.bindings[0].mode, ButtonMode::Toggle);
        assert_eq!(s.bindings[1].min_db, -60.0);
        assert_eq!(s.bindings[1].taper, Taper::Db);
        assert!(!s.controls[1].motorized);
        assert_eq!(s.bindings[1].target.direction, Direction::Tx);
        assert!(s.bindings[1].target.covers("main", Direction::Tx, 2));
        assert!(!s.bindings[1].target.covers("main", Direction::Rx, 2));
        s.validate().unwrap();
    }

    #[test]
    fn button_cannot_drive_gain() {
        let mut s = sample();
        s.bindings[0].action = BindingAction::Gain;
        assert!(s.validate().unwrap_err().0.contains("cannot be bound"));
    }

    #[test]
    fn hid_layout_checked() {
        let mut s = sample();
        s.controls[0].offset = None;
        assert!(s.validate().is_err(), "offset required");
        let mut s = sample();
        s.controls[0].bits = Some(2);
        assert!(s.validate().is_err(), "a button is one bit");
        let mut s = sample();
        s.controls[1].bits = Some(33);
        assert!(s.validate().is_err(), "too wide");
        let mut s = sample();
        s.controls[1].max = Some(5000);
        assert!(s.validate().is_err(), "max beyond 12 bits");
        let mut s = sample();
        s.controls[1].offset = Some(u32::MAX - 3);
        assert!(s.validate().is_err(), "offset overflow");
    }

    #[test]
    fn mixed_report_numbering_refused() {
        let mut s = sample();
        s.controls[1].report_id = None;
        assert!(s.validate().unwrap_err().0.contains("report_id"));
    }

    #[test]
    fn unknown_control_and_zero_channel_refused() {
        let mut s = sample();
        s.bindings[0].control = "nope".into();
        assert!(s.validate().is_err());
        let mut s = sample();
        s.bindings[1].target.channels = vec![0];
        assert!(s.validate().is_err());
    }

    #[test]
    fn gain_range_and_steps_checked() {
        let mut s = sample();
        s.bindings[1].max_db = 20.0;
        assert!(s.validate().is_err());
        let mut s = sample();
        s.bindings[1].min_db = 6.0;
        assert!(s.validate().is_err());
        let mut s = sample();
        s.bindings[1].step_db = f32::INFINITY;
        assert!(s.validate().is_err());
        let mut s = sample();
        s.bindings[1].deadband_db = f32::NAN;
        assert!(s.validate().is_err());
    }

    #[test]
    fn overlapping_gain_bindings_refused() {
        let mut s = sample();
        let mut second = s.bindings[1].clone();
        second.target.channels = vec![2, 3];
        s.bindings.push(second);
        assert!(s.validate().unwrap_err().0.contains("already covers"));
        // A mute binding on the same channels is fine.
        let mut s = sample();
        s.bindings.push(s.bindings[0].clone());
        s.validate().unwrap();
    }

    /// The sample with its bindings on RX and turned into output bindings.
    fn output_sample() -> SurfaceConfig {
        let mut s = sample();
        for b in &mut s.bindings {
            b.target.direction = Direction::Rx;
        }
        s.bindings[0].action = BindingAction::OutputMute;
        s.bindings[1].action = BindingAction::OutputGain;
        s
    }

    #[test]
    fn output_bindings_fit_their_controls_and_are_rx_only() {
        output_sample().validate().unwrap();
        assert!(
            output_sample()
                .bindings
                .iter()
                .all(|b| b.action.is_output())
        );
        // Kinds: a button drives mute, a fader gain, on either stage.
        let mut s = output_sample();
        s.bindings[0].action = BindingAction::OutputGain;
        assert!(s.validate().unwrap_err().0.contains("cannot be bound"));
        let mut s = output_sample();
        s.bindings[1].action = BindingAction::OutputMute;
        assert!(s.validate().unwrap_err().0.contains("cannot be bound"));
        // TX has no output stage.
        for i in 0..2 {
            let mut s = output_sample();
            s.bindings[i].target.direction = Direction::Tx;
            assert!(s.validate().unwrap_err().0.contains("only RX"), "{i}");
        }
        // A channel-stage gain on TX is still fine.
        sample().validate().unwrap();
    }

    #[test]
    fn output_gain_overlaps_only_with_output_gain() {
        // An output level and a channel gain on the same channels: two stages.
        let mut s = output_sample();
        let mut channel = s.bindings[1].clone();
        channel.action = BindingAction::Gain;
        s.bindings.push(channel);
        s.validate().unwrap();
        // Two output levels on one channel are refused.
        let mut s = output_sample();
        let mut second = s.bindings[1].clone();
        second.target.channels = vec![2, 3];
        s.bindings.push(second);
        assert!(s.validate().unwrap_err().0.contains("already covers"));
        // Mute and output mute buttons may share channels.
        let mut s = output_sample();
        let mut m = s.bindings[0].clone();
        m.action = BindingAction::Mute;
        s.bindings.push(m.clone());
        s.bindings.push(s.bindings[0].clone());
        s.validate().unwrap();
    }

    #[test]
    fn output_actions_convert_from_the_contract() {
        use tessera_control_types as ct;
        let target = ct::ChannelSet {
            device: ct::DeviceKey("main".into()),
            direction: Direction::Rx,
            channels: vec![ct::Channel::new(1).unwrap()],
        };
        let spec = ct::SurfaceSpec::new(
            "desk",
            ct::SurfaceSource::Hidraw {
                vendor_id: 1,
                product_id: 2,
                interface: None,
            },
            vec![],
            vec![
                ct::SurfaceBinding::new("a", ct::BindingAction::OutputGain, target.clone()),
                ct::SurfaceBinding::new("b", ct::BindingAction::OutputMute, target),
            ],
        );
        let c = SurfaceConfig::from(&spec);
        assert_eq!(c.bindings[0].action, BindingAction::OutputGain);
        assert_eq!(c.bindings[1].action, BindingAction::OutputMute);
    }

    #[test]
    fn report_id_zero_refused_and_narrow_fader_allowed() {
        let mut s = sample();
        for c in &mut s.controls {
            c.report_id = Some(0);
        }
        assert!(s.validate().unwrap_err().0.contains("reserved"));
        let mut s = sample();
        s.bindings[1].min_db = -0.5;
        s.bindings[1].max_db = 0.0;
        s.bindings[1].deadband_db = 0.1;
        s.validate().unwrap(); // step_db 1.0 is unused by a fader
    }

    #[test]
    fn unknown_keys_refused() {
        let bad = SAMPLE.replace("offset = 8", "offset = 8\nbogus = 1");
        assert!(toml::from_str::<File>(&bad).is_err());
    }

    #[test]
    fn motorized_only_on_faders() {
        let mut s = sample();
        s.controls[1].motorized = true;
        s.validate().unwrap();
        let mut s = sample();
        s.controls[0].motorized = true;
        assert!(s.validate().unwrap_err().0.contains("motorized"));
    }

    #[test]
    fn names_and_ids_are_identifiers() {
        let mut s = sample();
        s.name = "desk mixer".into();
        assert!(s.validate().unwrap_err().0.contains("name"));
        let mut s = sample();
        s.name = String::new();
        assert!(s.validate().is_err());
        let mut s = sample();
        s.controls[0].id = "mic/mute".into();
        s.bindings[0].control = "mic/mute".into();
        assert!(s.validate().unwrap_err().0.contains("id"));
        assert!(is_identifier("Desk_mixer-2"));
        assert!(!is_identifier(&"x".repeat(MAX_IDENTIFIER_BYTES + 1)));
    }

    #[test]
    fn pickup_key_is_gone() {
        let bad = SAMPLE.replace("max_db = 6.0", "max_db = 6.0\npickup = false");
        assert!(toml::from_str::<File>(&bad).is_err());
    }

    #[test]
    fn converts_from_the_contract() {
        use tessera_control_types as ct;
        let target = ct::ChannelSet {
            device: ct::DeviceKey("main".into()),
            direction: Direction::Tx,
            channels: vec![ct::Channel::new(1).unwrap(), ct::Channel::new(2).unwrap()],
        };
        let mut fader =
            ct::SurfaceBinding::new("mic-fader", ct::BindingAction::Gain, target.clone());
        fader.max_mdb = 6_000;
        fader.min_mdb = -40_500;
        fader.taper = ct::Taper::Linear;
        let mut mute = ct::SurfaceBinding::new("mic-mute", ct::BindingAction::Mute, target);
        mute.mode = ct::ButtonMode::PushToTalk;
        let control = |id: &str, kind, offset, bits, max, motorized| ct::SurfaceControl {
            id: id.into(),
            label: Some("Mic".into()),
            kind,
            report_id: Some(1),
            offset: Some(offset),
            bits,
            max,
            inverted: false,
            active_low: true,
            motorized,
            code: None,
            midi: None,
        };
        let spec = ct::SurfaceSpec::new(
            "desk-mixer",
            ct::SurfaceSource::Hidraw {
                vendor_id: 0x3142,
                product_id: 0x0c33,
                interface: Some(3),
            },
            vec![
                control("mic-mute", ct::ControlKind::Button, 8, None, None, false),
                control(
                    "mic-fader",
                    ct::ControlKind::Fader,
                    16,
                    Some(12),
                    Some(4095),
                    true,
                ),
            ],
            vec![mute, fader],
        )
        .with_enabled(false);
        let s = SurfaceConfig::from(&spec);
        s.validate().unwrap();
        assert!(!s.enabled);
        assert_eq!(
            s.source,
            SourceConfig::Hidraw {
                vendor_id: 0x3142,
                product_id: 0x0c33,
                interface: Some(3)
            }
        );
        assert!(s.controls[0].active_low);
        assert!(s.controls[1].motorized);
        assert_eq!(s.controls[1].field_bits(), 12);
        let b = &s.bindings[1];
        assert_eq!(b.target.channels, [1, 2]);
        assert_eq!((b.min_db, b.max_db), (-40.5, 6.0));
        assert_eq!((b.step_db, b.deadband_db), (1.0, 0.25));
        assert_eq!(b.taper, Taper::Linear);
        assert_eq!(s.bindings[0].mode, ButtonMode::PushToTalk);
    }

    fn midi_sample() -> SurfaceConfig {
        let mut s = sample();
        s.source = SourceConfig::Midi {
            card: "CQ".into(),
            device: 0,
        };
        for (c, number) in s.controls.iter_mut().zip([0u16, 0x2000]) {
            c.report_id = None;
            c.offset = None;
            c.bits = None;
            c.max = (c.kind == ControlKind::Fader).then_some(16320);
            c.midi = Some(MidiAddress::new(MidiKind::Nrpn, 1, number));
        }
        s.bindings[1].curve = crate::presets::allen_heath_cq(1).1;
        s
    }

    #[test]
    fn midi_controls_and_curves_checked() {
        let s = midi_sample();
        s.validate().unwrap();
        assert_eq!(s.controls[1].field_max(), 16383);
        let mut bad = midi_sample();
        bad.controls[0].midi = None;
        assert!(bad.validate().unwrap_err().0.contains("`midi`"));
        let mut bad = midi_sample();
        bad.controls[0].offset = Some(3);
        assert!(bad.validate().unwrap_err().0.contains("offset"));
        let mut bad = midi_sample();
        bad.controls[1].midi = bad.controls[0].midi;
        assert!(bad.validate().unwrap_err().0.contains("already uses"));
        let mut bad = midi_sample();
        bad.controls[0].midi.as_mut().unwrap().channel = 0;
        assert!(bad.validate().is_err());
        let mut bad = midi_sample();
        bad.controls[1].midi.as_mut().unwrap().kind = MidiKind::ControlChange;
        assert!(bad.validate().unwrap_err().0.contains("0..=127"));
        let mut bad = midi_sample();
        bad.controls[1].max = Some(20_000);
        assert!(bad.validate().is_err(), "max beyond 14 bits");
        let mut bad = sample();
        bad.controls[0].midi = midi_sample().controls[0].midi;
        assert!(bad.validate().is_err(), "midi on a hidraw source");
        let mut bad = midi_sample();
        bad.bindings[0].curve = bad.bindings[1].curve.clone();
        assert!(bad.validate().unwrap_err().0.contains("only a fader"));
        let mut bad = midi_sample();
        bad.bindings[1].curve.swap(0, 1);
        assert!(bad.validate().unwrap_err().0.contains("ascend"));
        let mut bad = midi_sample();
        bad.bindings[1].curve[0].raw = 0;
        assert!(bad.validate().unwrap_err().0.contains("off"));
        let mut bad = midi_sample();
        bad.bindings[1].curve[11].mdb = 13_000;
        assert!(bad.validate().is_err(), "above the ceiling");
    }

    #[test]
    fn midi_source_and_curve_convert_from_the_contract() {
        use tessera_control_types as ct;
        let (controls, curve) = crate::presets::allen_heath_cq(1);
        let target = ct::ChannelSet {
            device: ct::DeviceKey("main".into()),
            direction: Direction::Rx,
            channels: vec![ct::Channel::new(1).unwrap()],
        };
        let mut b = ct::SurfaceBinding::new("ip1", ct::BindingAction::Gain, target);
        b.curve = curve;
        let spec = ct::SurfaceSpec::new(
            "cq",
            ct::SurfaceSource::Midi {
                card: "CQ".into(),
                device: 1,
            },
            controls,
            vec![b],
        );
        let s = SurfaceConfig::from(&spec);
        s.validate().unwrap();
        assert_eq!(
            s.source,
            SourceConfig::Midi {
                card: "CQ".into(),
                device: 1
            }
        );
        assert!(s.controls[0].midi.is_some());
        let b = &s.bindings[0];
        assert_eq!(b.curve.len(), 12);
        assert_eq!((b.min_db, b.max_db), (-89.0, 10.0), "range from the curve");
        // TOML spelling of a midi source and control.
        let t = r#"
[[surface]]
name = "cq"
source = { kind = "midi", card = "CQ" }

[[surface.control]]
id = "ip1"
kind = "fader"
max = 16320
midi = { kind = "nrpn", channel = 1, number = 0x2000 }
"#;
        let s = toml::from_str::<File>(t).unwrap().surface.remove(0);
        s.validate().unwrap();
        assert_eq!(
            s.source,
            SourceConfig::Midi {
                card: "CQ".into(),
                device: 0
            }
        );
    }

    const ROUTED: &str = r#"
[[surface]]
name = "cq"
source = { kind = "tcp_midi", host = "192.168.1.40", port = 51325 }

[[surface.control]]
id = "f1"
kind = "fader"
midi = { kind = "nrpn", channel = 1, number = 0x2000 }

[[surface.control]]
id = "f2"
kind = "fader"
midi = { kind = "nrpn", channel = 1, number = 0x2001 }

[[surface.control]]
id = "m1"
kind = "button"
midi = { kind = "nrpn", channel = 1, number = 0x0100 }

[[surface.control]]
id = "m2"
kind = "button"
midi = { kind = "nrpn", channel = 1, number = 0x0101 }

[[surface.binding]]
control = "f1"
action = "host_volume"

[[surface.binding]]
control = "f2"
action = "follow"
follow = { surface = "desk2", control = "f1" }

[[surface.binding]]
control = "m1"
action = "port_mute"
ports = [{ kind = "card_in", card = "hw:CARD=CQ,DEV=0", channel = 15 }]

[[surface.binding]]
control = "m2"
action = "crosspoint_mute"
crosspoints = [{ source = { kind = "network_rx", device = "main", channel = 1 }, destination = { kind = "host_out", device = "main", channel = 1 } }]
"#;

    #[test]
    fn tcp_midi_and_the_router_actions_parse_and_validate() {
        let s = toml::from_str::<File>(ROUTED).unwrap().surface.remove(0);
        assert!(s.source.is_midi());
        s.validate().unwrap();
        assert_eq!(s.bindings[0].levels(), vec![Level::Host]);
        assert!(s.bindings[1].levels().is_empty());
        // Wrong fields for the action.
        // A host binding names the field to remove, in the state's words.
        let host = |b: BindingConfig| {
            let mut bad = s.clone();
            bad.bindings[0] = b;
            bad.validate().unwrap_err().0
        };
        for (b, want) in [
            (
                BindingConfig {
                    target: Target {
                        device: "main".into(),
                        direction: Direction::Rx,
                        channels: vec![1, 2],
                    },
                    ..s.bindings[0].clone()
                },
                "binding of \"f1\": remove `target`: a host volume binding acts on the host volume, not on channels",
            ),
            (
                BindingConfig {
                    ports: s.bindings[2].ports.clone(),
                    ..s.bindings[0].clone()
                },
                "binding of \"f1\": remove `ports`: a host volume binding acts on the host volume, not on router ports",
            ),
            (
                BindingConfig {
                    crosspoints: s.bindings[3].crosspoints.clone(),
                    ..s.bindings[0].clone()
                },
                "binding of \"f1\": remove `crosspoints`: a host volume binding acts on the host volume, not on crosspoints",
            ),
            (
                BindingConfig {
                    follow: s.bindings[1].follow.clone(),
                    ..s.bindings[0].clone()
                },
                "binding of \"f1\": remove `follow`: a host volume binding acts on the host volume, not on another surface's control",
            ),
        ] {
            let e = host(b);
            assert!(e.contains(want), "{e}");
        }
        let mut bad = s.clone();
        bad.bindings[2].action = BindingAction::HostMute;
        let e = bad.validate().unwrap_err().0;
        assert!(
            e.contains(
                "binding of \"m1\": remove `ports`: a host mute binding acts on the host mute, not on router ports"
            ),
            "{e}"
        );
        let mut bad = s.clone();
        bad.bindings[2].ports.clear();
        assert!(bad.validate().unwrap_err().0.contains("needs `ports`"));
        let mut bad = s.clone();
        bad.bindings[1].follow = None;
        assert!(bad.validate().unwrap_err().0.contains("needs `follow`"));
        let mut bad = s.clone();
        bad.bindings[1].follow = Some(FollowTarget {
            surface: "cq".into(),
            control: "f2".into(),
        });
        assert!(bad.validate().unwrap_err().0.contains("itself"));
        let mut bad = s.clone();
        bad.source = SourceConfig::TcpMidi {
            host: "has space".into(),
            port: 51325,
        };
        assert!(bad.validate().unwrap_err().0.contains("tcp_midi"));
        // Two level bindings on one value overlap.
        let mut bad = s;
        bad.bindings[1] = BindingConfig {
            follow: None,
            ..bad.bindings[0].clone()
        };
        bad.bindings[1].control = "f2".into();
        assert!(bad.validate().unwrap_err().0.contains("already covers"));
    }
}
