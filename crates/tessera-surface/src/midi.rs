//! MIDI surfaces: a byte-stream parser, NRPN assembly, the decoder that turns
//! a desk's messages into [`SurfaceEvent`]s, MIDI learn, and the encoders for
//! what tessera sends back (motorized fader positions, mute state).
//!
//! Sources: the MIDI 1.0 Detailed Specification (running status, real-time
//! bytes, SysEx framing, NRPN through CC 99/98 and data entry CC 6/38), and
//! the Allen & Heath CQ MIDI Protocol V1.2 (issue 5) for the byte order a
//! desk uses (CC 99, CC 98, CC 6, CC 38 for every 14-bit parameter).
//! Confidence: likely.
//!
//! All channels in this module are **1-based** (1..=16), as in
//! [`MidiAddress`].

use std::collections::BTreeSet;

use tessera_control_types::{LearnedControl, MidiAddress, MidiKind};

use crate::config::{ControlKind, SurfaceConfig};
use crate::decode::{SurfaceEvent, SurfaceEventKind};

/// Largest 14-bit value (NRPN numbers and values).
pub const MAX_14BIT: u16 = 0x3FFF;

/// One channel message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiMessage {
    NoteOff {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    /// Velocity is never 0: a Note On with velocity 0 is a [`NoteOff`].
    ///
    /// [`NoteOff`]: MidiMessage::NoteOff
    NoteOn {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    PolyPressure {
        channel: u8,
        note: u8,
        value: u8,
    },
    ControlChange {
        channel: u8,
        controller: u8,
        value: u8,
    },
    ProgramChange {
        channel: u8,
        program: u8,
    },
    ChannelPressure {
        channel: u8,
        value: u8,
    },
    /// 14-bit, 0x2000 is the centre.
    PitchBend {
        channel: u8,
        value: u16,
    },
}

/// Streaming MIDI byte parser. Keeps running status; real-time bytes
/// (0xF8..=0xFF) are ignored wherever they appear, even inside a message or
/// a SysEx; SysEx (0xF0 .. 0xF7) and system common messages are skipped.
/// Data bytes with no status to belong to are dropped.
#[derive(Debug, Default, Clone)]
pub struct MidiParser {
    /// Running status (0: none).
    status: u8,
    data: [u8; 2],
    have: usize,
    sysex: bool,
    /// Data bytes of a system common message still to skip.
    skip: u8,
}

impl MidiParser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget any partial message and the running status.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// One byte; a message when it completes one.
    pub fn push(&mut self, byte: u8) -> Option<MidiMessage> {
        if byte >= 0xF8 {
            return None;
        }
        if byte & 0x80 != 0 {
            // Any status byte but a real-time one ends a SysEx.
            self.sysex = byte == 0xF0;
            self.have = 0;
            self.skip = 0;
            self.status = 0;
            match byte {
                0xF0 | 0xF7 | 0xF4 | 0xF5 | 0xF6 => {}
                0xF1 | 0xF3 => self.skip = 1,
                0xF2 => self.skip = 2,
                _ => self.status = byte,
            }
            return None;
        }
        if self.sysex {
            return None;
        }
        if self.skip > 0 {
            self.skip -= 1;
            return None;
        }
        if self.status == 0 {
            return None;
        }
        self.data[self.have] = byte;
        self.have += 1;
        let kind = self.status & 0xF0;
        let needed = if matches!(kind, 0xC0 | 0xD0) { 1 } else { 2 };
        if self.have < needed {
            return None;
        }
        self.have = 0;
        let channel = (self.status & 0x0F) + 1;
        let [a, b] = self.data;
        Some(match kind {
            0x80 => MidiMessage::NoteOff {
                channel,
                note: a,
                velocity: b,
            },
            0x90 if b == 0 => MidiMessage::NoteOff {
                channel,
                note: a,
                velocity: 0,
            },
            0x90 => MidiMessage::NoteOn {
                channel,
                note: a,
                velocity: b,
            },
            0xA0 => MidiMessage::PolyPressure {
                channel,
                note: a,
                value: b,
            },
            0xB0 => MidiMessage::ControlChange {
                channel,
                controller: a,
                value: b,
            },
            0xC0 => MidiMessage::ProgramChange {
                channel,
                program: a,
            },
            0xD0 => MidiMessage::ChannelPressure { channel, value: a },
            _ => MidiMessage::PitchBend {
                channel,
                value: u16::from(a) | (u16::from(b) << 7),
            },
        })
    }

    /// Every message completed by `bytes`, appended to `out`.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<MidiMessage>) {
        out.extend(bytes.iter().filter_map(|&b| self.push(b)));
    }
}

/// NRPN parameter select, MSB.
pub const CC_NRPN_MSB: u8 = 99;
/// NRPN parameter select, LSB.
pub const CC_NRPN_LSB: u8 = 98;
/// RPN parameter select, MSB.
pub const CC_RPN_MSB: u8 = 101;
/// RPN parameter select, LSB.
pub const CC_RPN_LSB: u8 = 100;
/// Data entry, MSB ("VC").
pub const CC_DATA_MSB: u8 = 6;
/// Data entry, LSB ("VF").
pub const CC_DATA_LSB: u8 = 38;
/// Data increment (the CQ's "get" request: value 0x7F).
pub const CC_DATA_INCREMENT: u8 = 96;
/// Data decrement.
pub const CC_DATA_DECREMENT: u8 = 97;

/// Is `controller` part of the (N)RPN protocol rather than a control of its
/// own?
#[must_use]
pub fn is_parameter_cc(controller: u8) -> bool {
    matches!(
        controller,
        CC_NRPN_MSB
            | CC_NRPN_LSB
            | CC_RPN_MSB
            | CC_RPN_LSB
            | CC_DATA_MSB
            | CC_DATA_LSB
            | CC_DATA_INCREMENT
            | CC_DATA_DECREMENT
    )
}

/// An NRPN value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nrpn {
    pub channel: u8,
    /// Parameter, `MSB << 7 | LSB`.
    pub number: u16,
    /// `fine`: the 14-bit value `(VC << 7) | VF`. Otherwise the 7-bit data
    /// entry value VC alone (a device that never sends CC 38).
    pub value: u16,
    pub fine: bool,
}

#[derive(Debug, Default, Clone, Copy)]
struct NrpnChannel {
    msb: Option<u8>,
    lsb: Option<u8>,
    /// An RPN is selected: data entry is not ours.
    rpn: bool,
    vc: Option<u8>,
    /// This channel has sent a CC 38 (a 14-bit device): CC 6 alone no
    /// longer completes a value.
    fine_seen: bool,
}

/// Assembles NRPN values per channel from Control Change messages.
///
/// CC 99 / CC 98 select the parameter, CC 6 sets the value MSB (VC), CC 38
/// the LSB (VF). A value is complete on CC 38 (`fine`, 14 bits), or on CC 6
/// for as long as the channel has never sent a CC 38 (a 7-bit device). So
/// the order 99, 98, 6, 38 gives a 7-bit value at the first CC 6 a channel
/// ever sends and the 14-bit value at the CC 38, and every later parameter
/// gives only the 14-bit value; 99, 98, 6 gives the 7-bit value. Consumers
/// that know a parameter is 14-bit ignore values that are not `fine`.
/// Selecting an RPN (CC 101/100) or the null parameter (127/127) stops data
/// entry from counting.
#[derive(Debug, Default, Clone)]
pub struct NrpnAssembler {
    ch: [NrpnChannel; 16],
}

impl NrpnAssembler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget every selection.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// A completed NRPN value, if `msg` completes one.
    pub fn push(&mut self, msg: &MidiMessage) -> Option<Nrpn> {
        let &MidiMessage::ControlChange {
            channel,
            controller,
            value,
        } = msg
        else {
            return None;
        };
        let c = self.ch.get_mut(usize::from(channel.checked_sub(1)?))?;
        let number = |c: &NrpnChannel| -> Option<u16> {
            let (m, l) = (c.msb?, c.lsb?);
            (!c.rpn && !(m == 127 && l == 127)).then_some((u16::from(m) << 7) | u16::from(l))
        };
        match controller {
            CC_NRPN_MSB => {
                c.msb = Some(value);
                c.rpn = false;
                c.vc = None;
            }
            CC_NRPN_LSB => {
                c.lsb = Some(value);
                c.rpn = false;
                c.vc = None;
            }
            CC_RPN_MSB | CC_RPN_LSB => {
                c.rpn = true;
                c.vc = None;
            }
            CC_DATA_MSB => {
                let number = number(c)?;
                c.vc = Some(value);
                if !c.fine_seen {
                    return Some(Nrpn {
                        channel,
                        number,
                        value: u16::from(value),
                        fine: false,
                    });
                }
            }
            CC_DATA_LSB => {
                let number = number(c)?;
                c.fine_seen = true;
                let vc = c.vc.unwrap_or(0);
                return Some(Nrpn {
                    channel,
                    number,
                    value: (u16::from(vc) << 7) | u16::from(value),
                    fine: true,
                });
            }
            _ => {}
        }
        None
    }
}

/// A MIDI message reduced to what a control reads: its address and value.
fn addressed(msg: &MidiMessage, nrpn: &mut NrpnAssembler) -> Option<(MidiAddress, u32, bool)> {
    if let Some(n) = nrpn.push(msg) {
        let addr = MidiAddress::new(MidiKind::Nrpn, n.channel, n.number);
        return Some((addr, u32::from(n.value), n.fine));
    }
    let (kind, channel, number, value) = match *msg {
        MidiMessage::ControlChange {
            channel,
            controller,
            value,
        } if !is_parameter_cc(controller) => (MidiKind::ControlChange, channel, controller, value),
        MidiMessage::NoteOn {
            channel,
            note,
            velocity,
        } => (MidiKind::Note, channel, note, velocity),
        MidiMessage::NoteOff { channel, note, .. } => (MidiKind::Note, channel, note, 0),
        _ => return None,
    };
    let addr = MidiAddress::new(kind, channel, u16::from(number));
    Some((addr, u32::from(value), true))
}

/// Is a control at `addr` a desk's state parameter (an NRPN button, which
/// reports "on"/"off" rather than press and release)?
#[must_use]
pub fn is_latching(kind: ControlKind, addr: Option<&MidiAddress>) -> bool {
    kind == ControlKind::Button && addr.is_some_and(|a| a.kind == MidiKind::Nrpn)
}

#[derive(Debug)]
struct MidiField {
    control: String,
    kind: ControlKind,
    addr: MidiAddress,
    max: u32,
    inverted: bool,
    active_low: bool,
    /// A 14-bit NRPN fader: 7-bit values are ignored.
    wide: bool,
    latching: bool,
    last: Option<u32>,
    /// A latching NRPN button's 7-bit value (CC 6 alone) waiting for its
    /// fine byte (CC 38): see [`MidiDecoder`].
    held: Option<u32>,
}

impl MidiField {
    fn baseline(&self) -> Option<u32> {
        (self.kind == ControlKind::Button).then_some(0)
    }

    /// The event for a value `raw` of this field, if it is one.
    fn apply(&mut self, raw: u32, out: &mut Vec<SurfaceEvent>) {
        let kind = match self.kind {
            ControlKind::Button => {
                let pressed = (raw != 0) != self.active_low;
                let v = u32::from(pressed);
                if self.last.replace(v) == Some(v) && !self.latching {
                    return;
                }
                SurfaceEventKind::Button { pressed }
            }
            ControlKind::Fader => {
                let v = raw.min(self.max);
                if self.last.replace(v) == Some(v) {
                    return;
                }
                let value = if self.inverted { self.max - v } else { v };
                SurfaceEventKind::Fader {
                    value,
                    max: self.max,
                }
            }
            ControlKind::Encoder => return,
        };
        out.push(SurfaceEvent {
            control: self.control.clone(),
            kind,
        });
    }
}

/// Decoder for one MIDI surface: raw bytes from the rawmidi port →
/// [`SurfaceEvent`]s, by each control's [`MidiAddress`].
///
/// * Faders: CC values 0..=127, NRPN values 0..=`max` (default 16383),
///   Note velocities; clamped to `max`, `inverted` applied; repeats dropped.
/// * Buttons: a Note On is pressed, Note Off released; a CC or NRPN value
///   other than 0 is pressed (`active_low` reverses it). CC and Note
///   buttons report edges only. An NRPN button is a desk's state parameter
///   (a mute: 1 on, 0 off): every report is passed on, since tessera may have
///   changed the desk's state since the last one (mute feedback), and the
///   mapper acts on it as a state, not a press. Its 7-bit value (CC 6 before
///   the channel ever sent a CC 38) is **held** until it is known whether a
///   fine byte follows: a desk that sends 14-bit values (a mute on is VC 0,
///   VF 1) would otherwise show "off" for a moment at its first message.
///   A held value is passed on at the next Control Change on its channel
///   that is not the CC 38 completing it (a 7-bit device), or when the
///   caller flushes ([`flush`](Self::flush), after
///   [`NRPN_HOLD_MS`] without more bytes); the CC 38 replaces it.
/// * Encoders are not read from MIDI (validation refuses them).
#[derive(Debug)]
pub struct MidiDecoder {
    parser: MidiParser,
    nrpn: NrpnAssembler,
    fields: Vec<MidiField>,
    /// Controls a message addressed since the last
    /// [`take_heard`](Self::take_heard), repeats included.
    heard: Vec<String>,
}

/// How long a caller lets a held 7-bit NRPN button value wait for its fine
/// byte before [`MidiDecoder::flush`], milliseconds (the two bytes of one
/// value arrive together; this only matters for a 7-bit device).
pub const NRPN_HOLD_MS: u64 = 20;

impl MidiDecoder {
    /// A decoder for the controls of `cfg` that have a `midi` address.
    /// Expects a validated config.
    #[must_use]
    pub fn new(cfg: &SurfaceConfig) -> Self {
        let fields = cfg
            .controls
            .iter()
            .filter_map(|c| {
                let addr = c.midi?;
                let max = c.max.unwrap_or_else(|| c.field_max());
                let mut f = MidiField {
                    control: c.id.clone(),
                    kind: c.kind,
                    addr,
                    max,
                    inverted: c.inverted,
                    active_low: c.active_low,
                    wide: addr.kind == MidiKind::Nrpn && c.kind == ControlKind::Fader && max > 127,
                    latching: is_latching(c.kind, Some(&addr)),
                    last: None,
                    held: None,
                };
                f.last = f.baseline();
                Some(f)
            })
            .collect();
        Self {
            parser: MidiParser::new(),
            nrpn: NrpnAssembler::new(),
            fields,
            heard: Vec::new(),
        }
    }

    /// The controls a message addressed since the last call (each once),
    /// whether or not its value changed: an unchanged value decodes to no
    /// event, but it is still the desk speaking about that control (the
    /// answer to a value request).
    pub fn take_heard(&mut self) -> Vec<String> {
        std::mem::take(&mut self.heard)
    }

    /// Forget every value, parameter selection and partial message (after
    /// the port was reopened).
    pub fn reset(&mut self) {
        self.heard.clear();
        self.parser.reset();
        self.nrpn.reset();
        for f in &mut self.fields {
            f.last = f.baseline();
            f.held = None;
        }
    }

    /// A 7-bit NRPN button value is held (call [`flush`](Self::flush) when
    /// no more bytes came within [`NRPN_HOLD_MS`]).
    #[must_use]
    pub fn has_held(&self) -> bool {
        self.fields.iter().any(|f| f.held.is_some())
    }

    /// Pass on every held 7-bit NRPN button value (no fine byte came).
    pub fn flush(&mut self, out: &mut Vec<SurfaceEvent>) {
        for f in &mut self.fields {
            if let Some(raw) = f.held.take() {
                f.apply(raw, out);
            }
        }
    }

    /// Decode bytes read from the port, appending the changes to `out`.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<SurfaceEvent>) {
        for &b in bytes {
            if let Some(m) = self.parser.push(b) {
                self.handle(&m, out);
            }
        }
    }

    /// Decode one parsed message.
    pub fn handle(&mut self, msg: &MidiMessage, out: &mut Vec<SurfaceEvent>) {
        // Any other Control Change on a channel with a held value: no fine
        // byte is coming for it (a 7-bit device).
        if let &MidiMessage::ControlChange {
            channel,
            controller,
            ..
        } = msg
        {
            if controller != CC_DATA_LSB {
                for f in &mut self.fields {
                    if f.addr.channel == channel {
                        if let Some(raw) = f.held.take() {
                            f.apply(raw, out);
                        }
                    }
                }
            }
        }
        let Some((addr, raw, fine)) = addressed(msg, &mut self.nrpn) else {
            return;
        };
        for f in self.fields.iter_mut().filter(|f| f.addr == addr) {
            if f.wide && !fine {
                continue;
            }
            if !self.heard.contains(&f.control) {
                self.heard.push(f.control.clone());
            }
            if f.latching {
                if fine {
                    // The fine byte: the held coarse value was a part.
                    f.held = None;
                } else {
                    f.held = Some(raw);
                    continue;
                }
            }
            f.apply(raw, out);
        }
    }

    /// tessera sent fader `control` to `value` (a
    /// [`SurfaceAction::MoveFader`](crate::SurfaceAction::MoveFader), same
    /// scale). A desk need not echo what it is sent, so hand the returned
    /// event to the mapper: it then knows the fader got there. A later echo
    /// of the same position is not a movement.
    pub fn moved(&mut self, control: &str, value: u32) -> Option<SurfaceEvent> {
        let f = self
            .fields
            .iter_mut()
            .find(|f| f.control == control && f.kind == ControlKind::Fader)?;
        let value = value.min(f.max);
        f.last = Some(if f.inverted { f.max - value } else { value });
        Some(SurfaceEvent {
            control: f.control.clone(),
            kind: SurfaceEventKind::Fader { value, max: f.max },
        })
    }

    /// The bytes that put control `control` at `value` on the device (event
    /// scale: for a fader 0 is the bottom, `inverted` is applied here; for a
    /// button 0 is off, anything else on). `None` for a control this decoder
    /// does not know.
    #[must_use]
    pub fn output(&self, control: &str, value: u32) -> Option<Vec<u8>> {
        let f = self.fields.iter().find(|f| f.control == control)?;
        let raw = match f.kind {
            ControlKind::Fader => {
                let v = value.min(f.max);
                if f.inverted { f.max - v } else { v }
            }
            _ => u32::from((value != 0) != f.active_low),
        };
        Some(encode_control(&f.addr, raw))
    }
}

/// Finds the first MIDI control that moves (the portal's "Learn").
///
/// * A Note is a button (final at once).
/// * A CC or NRPN whose values are only 0 and 1, or only 0 and 127, is a
///   button (final once it went to 127 and back to 0); otherwise a fader
///   with `max` the largest value seen. Faders are never final on their own:
///   read [`result`](Self::result) when the caller's time is up.
///
/// The result has `offset`/`bits` 0 and `midi` set.
#[derive(Debug, Default)]
pub struct MidiLearner {
    parser: MidiParser,
    nrpn: NrpnAssembler,
    first: Option<MidiAddress>,
    values: BTreeSet<u32>,
    last: Option<u32>,
    released: bool,
}

impl MidiLearner {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes read while the user moves the control; the result once final.
    pub fn feed(&mut self, bytes: &[u8]) -> Option<LearnedControl> {
        let mut done = None;
        for &b in bytes {
            if let Some(m) = self.parser.push(b) {
                done = done.or_else(|| self.handle(&m));
            }
        }
        done
    }

    /// One parsed message; the result once final.
    pub fn handle(&mut self, msg: &MidiMessage) -> Option<LearnedControl> {
        let (addr, value, _) = addressed(msg, &mut self.nrpn)?;
        match self.first {
            None => {
                // A Note Off cannot start a detection (a key released from
                // before the learn).
                if addr.kind == MidiKind::Note && value == 0 {
                    return None;
                }
                self.first = Some(addr);
            }
            Some(f) if f != addr => return None,
            Some(_) => {}
        }
        if self.last == Some(127) && value == 0 {
            self.released = true;
        }
        self.last = Some(value);
        self.values.insert(value);
        let r = self.result()?;
        let fin = addr.kind == MidiKind::Note
            || (r.kind == tessera_control_types::ControlKind::Button
                && self.released
                && self.values.iter().all(|v| matches!(v, 0 | 127)));
        fin.then_some(r)
    }

    /// The control found so far; `None` while nothing has moved.
    #[must_use]
    pub fn result(&self) -> Option<LearnedControl> {
        let addr = self.first?;
        let two_state = self.values.iter().all(|v| matches!(v, 0 | 1))
            || self.values.iter().all(|v| matches!(v, 0 | 127));
        let button = addr.kind == MidiKind::Note || two_state;
        let mut l = LearnedControl::new(
            if button {
                tessera_control_types::ControlKind::Button
            } else {
                tessera_control_types::ControlKind::Fader
            },
            None,
            0,
            0,
        );
        if !button {
            l.max = self.values.last().copied();
        }
        l.midi = Some(addr);
        Some(l)
    }
}

fn status(kind: u8, channel: u8) -> u8 {
    kind | (channel.clamp(1, 16) - 1)
}

/// A Control Change (channel 1..=16; data masked to 7 bits).
#[must_use]
pub fn encode_cc(channel: u8, controller: u8, value: u8) -> [u8; 3] {
    [status(0xB0, channel), controller & 0x7F, value & 0x7F]
}

/// A Note On with `velocity`, or a Note Off for velocity 0.
#[must_use]
pub fn encode_note(channel: u8, note: u8, velocity: u8) -> [u8; 3] {
    if velocity & 0x7F == 0 {
        [status(0x80, channel), note & 0x7F, 0]
    } else {
        [status(0x90, channel), note & 0x7F, velocity & 0x7F]
    }
}

/// An NRPN write: CC 99 (MSB of `number`), CC 98 (LSB), CC 6 (VC), CC 38
/// (VF), each with its status byte (no running status). `number` and
/// `value` are clamped to 14 bits.
#[must_use]
pub fn encode_nrpn(channel: u8, number: u16, value: u16) -> [u8; 12] {
    let s = status(0xB0, channel);
    let n = number.min(MAX_14BIT);
    let v = value.min(MAX_14BIT);
    let [nm, nl] = [(n >> 7) as u8, (n & 0x7F) as u8];
    let [vc, vf] = [(v >> 7) as u8, (v & 0x7F) as u8];
    [
        s,
        CC_NRPN_MSB,
        nm,
        s,
        CC_NRPN_LSB,
        nl,
        s,
        CC_DATA_MSB,
        vc,
        s,
        CC_DATA_LSB,
        vf,
    ]
}

/// An NRPN "get" (the CQ answers with the parameter's value): CC 99, CC 98,
/// CC 96 = 0x7F.
#[must_use]
pub fn encode_nrpn_get(channel: u8, number: u16) -> [u8; 9] {
    let s = status(0xB0, channel);
    let n = number.min(MAX_14BIT);
    [
        s,
        CC_NRPN_MSB,
        (n >> 7) as u8,
        s,
        CC_NRPN_LSB,
        (n & 0x7F) as u8,
        s,
        CC_DATA_INCREMENT,
        0x7F,
    ]
}

/// The message that sets the control at `addr` to raw `value` (CC and
/// Note: clamped to 127, a Note with value 0 is a Note Off; NRPN: clamped
/// to 14 bits).
#[must_use]
pub fn encode_control(addr: &MidiAddress, value: u32) -> Vec<u8> {
    let seven = u8::try_from(value.min(127)).unwrap_or(127);
    let number = u8::try_from(addr.number.min(127)).unwrap_or(127);
    match addr.kind {
        MidiKind::Nrpn => {
            let v = u16::try_from(value.min(u32::from(MAX_14BIT))).unwrap_or(MAX_14BIT);
            encode_nrpn(addr.channel, addr.number, v).to_vec()
        }
        MidiKind::Note => encode_note(addr.channel, number, seven).to_vec(),
        _ => encode_cc(addr.channel, number, seven).to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ControlConfig, SourceConfig};

    fn parse(bytes: &[u8]) -> Vec<MidiMessage> {
        let mut p = MidiParser::new();
        let mut out = vec![];
        p.feed(bytes, &mut out);
        out
    }

    fn cc(channel: u8, controller: u8, value: u8) -> MidiMessage {
        MidiMessage::ControlChange {
            channel,
            controller,
            value,
        }
    }

    #[test]
    fn parser_running_status_and_note_on_zero() {
        let m = parse(&[0x90, 60, 100, 61, 0, 0xB1, 7, 99, 8, 1]);
        assert_eq!(
            m,
            vec![
                MidiMessage::NoteOn {
                    channel: 1,
                    note: 60,
                    velocity: 100
                },
                MidiMessage::NoteOff {
                    channel: 1,
                    note: 61,
                    velocity: 0
                },
                cc(2, 7, 99),
                cc(2, 8, 1),
            ]
        );
    }

    #[test]
    fn parser_realtime_anywhere_and_sysex_skipped() {
        // Clock and active sensing inside a message, a SysEx with a clock in
        // it, then running status lost (SysEx cancels it).
        let m = parse(&[
            0xB0, 0xF8, 7, 0xFE, 64, 0xF0, 0x00, 0xF8, 0x21, 0x7F, 0xF7, 7, 10, 0xB0, 7, 11,
        ]);
        assert_eq!(m, vec![cc(1, 7, 64), cc(1, 7, 11)]);
    }

    #[test]
    fn parser_system_common_skipped_and_stray_data_dropped() {
        let m = parse(&[
            5, 6, 0xF2, 1, 2, 0xC3, 9, 0xF1, 3, 0xD0, 4, 0xE0, 0x00, 0x40,
        ]);
        assert_eq!(
            m,
            vec![
                MidiMessage::ProgramChange {
                    channel: 4,
                    program: 9
                },
                MidiMessage::ChannelPressure {
                    channel: 1,
                    value: 4
                },
                MidiMessage::PitchBend {
                    channel: 1,
                    value: 0x2000
                },
            ]
        );
        // A status byte ends an unterminated SysEx.
        assert_eq!(parse(&[0xF0, 1, 2, 0xB0, 1, 2]), vec![cc(1, 1, 2)]);
        // Program change keeps running status with one data byte.
        assert_eq!(parse(&[0xC0, 1, 2]).len(), 2);
    }

    fn nrpn_all(bytes: &[u8]) -> Vec<Nrpn> {
        let mut a = NrpnAssembler::new();
        parse(bytes).iter().filter_map(|m| a.push(m)).collect()
    }

    #[test]
    fn nrpn_cq_order_gives_coarse_once_then_fine_only() {
        // Ip1 level 12544 (VC 98, VF 0), then Ip2 mute on.
        let v = nrpn_all(&[
            0xB0, 99, 0x40, 98, 0x00, 6, 98, 38, 0, 99, 0x00, 98, 0x01, 6, 0, 38, 1,
        ]);
        assert_eq!(
            v,
            vec![
                Nrpn {
                    channel: 1,
                    number: 0x40 << 7,
                    value: 98,
                    fine: false
                },
                Nrpn {
                    channel: 1,
                    number: 0x40 << 7,
                    value: 12544,
                    fine: true
                },
                Nrpn {
                    channel: 1,
                    number: 1,
                    value: 1,
                    fine: true
                },
            ]
        );
    }

    #[test]
    fn nrpn_seven_bit_device_and_rpn_null_ignored() {
        let v = nrpn_all(&[0xB2, 99, 1, 98, 2, 6, 100, 6, 101]);
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].channel, v[0].number, v[0].value), (3, 130, 100));
        assert!(!v[1].fine);
        // RPN selected: data entry is not ours; null NRPN too; and data entry
        // with nothing selected.
        assert!(nrpn_all(&[0xB0, 6, 5, 101, 0, 100, 0, 6, 2, 38, 0]).is_empty());
        assert!(nrpn_all(&[0xB0, 99, 127, 98, 127, 6, 2, 38, 1]).is_empty());
        // CC 38 alone after a selection: VC counts as 0.
        let v = nrpn_all(&[0xB0, 99, 0, 98, 5, 38, 9]);
        assert_eq!(v[0].value, 9);
        // Channels are independent.
        let v = nrpn_all(&[0xB0, 99, 0, 98, 5, 0xB1, 6, 9, 0xB0, 6, 1]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].channel, 1);
    }

    fn control(id: &str, kind: ControlKind, addr: MidiAddress, max: Option<u32>) -> ControlConfig {
        ControlConfig {
            id: id.into(),
            kind,
            report_id: None,
            offset: None,
            bits: None,
            max,
            inverted: false,
            active_low: false,
            motorized: false,
            code: None,
            midi: Some(addr),
        }
    }

    fn addr(kind: MidiKind, number: u16) -> MidiAddress {
        MidiAddress::new(kind, 1, number)
    }

    fn cfg(controls: Vec<ControlConfig>) -> SurfaceConfig {
        SurfaceConfig {
            name: "desk".into(),
            enabled: true,
            source: SourceConfig::Midi {
                card: "CQ".into(),
                device: 0,
            },
            controls,
            bindings: vec![],
        }
    }

    fn decode(d: &mut MidiDecoder, bytes: &[u8]) -> Vec<SurfaceEvent> {
        let mut out = vec![];
        d.feed(bytes, &mut out);
        out
    }

    #[test]
    fn decoder_nrpn_fader_ignores_coarse_and_dedupes() {
        let c = cfg(vec![control(
            "ip1",
            ControlKind::Fader,
            addr(MidiKind::Nrpn, 0x40 << 7),
            Some(16320),
        )]);
        c.validate().unwrap();
        let mut d = MidiDecoder::new(&c);
        let ev = decode(&mut d, &encode_nrpn(1, 0x40 << 7, 12544));
        assert_eq!(
            ev,
            vec![SurfaceEvent {
                control: "ip1".into(),
                kind: SurfaceEventKind::Fader {
                    value: 12544,
                    max: 16320
                }
            }]
        );
        assert!(decode(&mut d, &encode_nrpn(1, 0x40 << 7, 12544)).is_empty());
        // Above max is the top; another parameter is nobody's.
        let ev = decode(&mut d, &encode_nrpn(1, 0x40 << 7, 16383));
        assert_eq!(
            ev[0].kind,
            SurfaceEventKind::Fader {
                value: 16320,
                max: 16320
            }
        );
        assert!(decode(&mut d, &encode_nrpn(1, (0x40 << 7) | 1, 5)).is_empty());
        assert!(decode(&mut d, &encode_nrpn(2, 0x40 << 7, 5)).is_empty());
    }

    #[test]
    fn heard_controls_include_unchanged_values() {
        let c = cfg(vec![
            control(
                "ip1",
                ControlKind::Fader,
                addr(MidiKind::Nrpn, 0x40 << 7),
                Some(16320),
            ),
            control("m", ControlKind::Button, addr(MidiKind::Nrpn, 0), None),
        ]);
        let mut d = MidiDecoder::new(&c);
        assert!(!decode(&mut d, &encode_nrpn(1, 0x40 << 7, 12544)).is_empty());
        assert_eq!(d.take_heard(), vec!["ip1".to_owned()]);
        // The same value again: no event, but the desk spoke of it (once).
        assert!(decode(&mut d, &encode_nrpn(1, 0x40 << 7, 12544)).is_empty());
        assert!(decode(&mut d, &encode_nrpn(1, 0x40 << 7, 12544)).is_empty());
        assert_eq!(d.take_heard(), vec!["ip1".to_owned()]);
        assert!(d.take_heard().is_empty(), "taken");
        // Another parameter is nobody's.
        decode(&mut d, &encode_nrpn(1, (0x40 << 7) | 1, 5));
        assert!(d.take_heard().is_empty());
    }

    #[test]
    fn decoder_nrpn_mute_is_a_state_and_never_deduped() {
        let mut d = MidiDecoder::new(&cfg(vec![control(
            "ip1-mute",
            ControlKind::Button,
            addr(MidiKind::Nrpn, 0),
            None,
        )]));
        let pressed = |ev: &[SurfaceEvent]| -> Vec<bool> {
            ev.iter()
                .map(|e| match e.kind {
                    SurfaceEventKind::Button { pressed } => pressed,
                    _ => panic!("not a button"),
                })
                .collect()
        };
        // First message ever on the channel: the coarse VC 0 is held, the
        // VF 1 completes it: on, never "off" first.
        assert_eq!(pressed(&decode(&mut d, &encode_nrpn(1, 0, 1))), [true]);
        assert!(!d.has_held());
        assert_eq!(pressed(&decode(&mut d, &encode_nrpn(1, 0, 1))), [true]);
        assert_eq!(pressed(&decode(&mut d, &encode_nrpn(1, 0, 0))), [false]);
    }

    #[test]
    fn decoder_holds_a_seven_bit_nrpn_button_value_until_it_is_known() {
        let mut d = MidiDecoder::new(&cfg(vec![control(
            "m",
            ControlKind::Button,
            addr(MidiKind::Nrpn, 0),
            None,
        )]));
        // Split mid-value: the coarse part alone shows nothing yet.
        let bytes = encode_nrpn(1, 0, 1);
        let (a, b) = bytes.split_at(9);
        assert!(decode(&mut d, a).is_empty());
        assert!(d.has_held());
        assert_eq!(
            decode(&mut d, b)[0].kind,
            SurfaceEventKind::Button { pressed: true }
        );
        // A 7-bit device (CC 6 only): the value is passed on at the next
        // parameter selection, or by a flush when nothing more comes.
        let mut d = MidiDecoder::new(&cfg(vec![control(
            "m",
            ControlKind::Button,
            addr(MidiKind::Nrpn, 0),
            None,
        )]));
        assert!(decode(&mut d, &[0xB0, 99, 0, 98, 0, 6, 1]).is_empty());
        let ev = decode(&mut d, &[0xB0, 99, 0]);
        assert_eq!(ev[0].kind, SurfaceEventKind::Button { pressed: true });
        assert!(decode(&mut d, &[0xB0, 98, 0, 6, 0]).is_empty());
        // Another channel does not decide.
        assert!(decode(&mut d, &[0xB1, 99, 0]).is_empty());
        let mut out = Vec::new();
        d.flush(&mut out);
        assert_eq!(out[0].kind, SurfaceEventKind::Button { pressed: false });
        assert!(!d.has_held());
    }

    #[test]
    fn decoder_cc_and_note_buttons_report_edges() {
        let mut d = MidiDecoder::new(&cfg(vec![
            control(
                "b",
                ControlKind::Button,
                addr(MidiKind::ControlChange, 20),
                None,
            ),
            control("n", ControlKind::Button, addr(MidiKind::Note, 36), None),
            control(
                "f",
                ControlKind::Fader,
                addr(MidiKind::ControlChange, 7),
                None,
            ),
        ]));
        let ev = decode(&mut d, &[0xB0, 20, 127, 20, 127, 20, 0]);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].kind, SurfaceEventKind::Button { pressed: true });
        assert_eq!(ev[1].kind, SurfaceEventKind::Button { pressed: false });
        let ev = decode(&mut d, &[0x90, 36, 90, 36, 0, 0x80, 36, 0]);
        assert_eq!(ev.len(), 2, "note on, note off; the second off is a repeat");
        let ev = decode(&mut d, &[0xB0, 7, 64]);
        assert_eq!(
            ev[0].kind,
            SurfaceEventKind::Fader {
                value: 64,
                max: 127
            }
        );
        // NRPN parts are not plain CCs.
        let mut d = MidiDecoder::new(&cfg(vec![control(
            "x",
            ControlKind::Fader,
            addr(MidiKind::ControlChange, 6),
            None,
        )]));
        assert!(decode(&mut d, &[0xB0, 6, 5]).is_empty());
    }

    #[test]
    fn decoder_seven_bit_nrpn_fader_and_inversion() {
        let mut f = control("f", ControlKind::Fader, addr(MidiKind::Nrpn, 3), Some(127));
        f.inverted = true;
        let mut d = MidiDecoder::new(&cfg(vec![f]));
        let ev = decode(&mut d, &[0xB0, 99, 0, 98, 3, 6, 27]);
        assert_eq!(
            ev[0].kind,
            SurfaceEventKind::Fader {
                value: 100,
                max: 127
            }
        );
        // tessera moved it: an echo is no movement; output re-inverts.
        let e = d.moved("f", 50).unwrap();
        assert_eq!(
            e.kind,
            SurfaceEventKind::Fader {
                value: 50,
                max: 127
            }
        );
        assert!(decode(&mut d, &[0xB0, 6, 77]).is_empty());
        assert_eq!(d.output("f", 50).unwrap(), encode_nrpn(1, 3, 77).to_vec());
        d.reset();
        assert_eq!(
            decode(&mut d, &[6, 77]).len(),
            0,
            "running status forgotten"
        );
    }

    #[test]
    fn encoders_round_trip_through_the_parser() {
        let mut a = NrpnAssembler::new();
        let msgs = parse(&encode_nrpn(16, 0x3FFE, 0x2ABC));
        let fine: Vec<Nrpn> = msgs
            .iter()
            .filter_map(|m| a.push(m))
            .filter(|n| n.fine)
            .collect();
        assert_eq!(
            fine,
            vec![Nrpn {
                channel: 16,
                number: 0x3FFE,
                value: 0x2ABC,
                fine: true
            }]
        );
        assert_eq!(encode_nrpn(1, 0x40 << 7, 16320)[..3], [0xB0, 99, 0x40]);
        assert_eq!(encode_nrpn(1, 0, 99_00)[8], (9900 >> 7) as u8);
        assert_eq!(parse(&encode_cc(3, 7, 200)), vec![cc(3, 7, 72)]);
        assert_eq!(
            parse(&encode_note(1, 60, 100)),
            vec![MidiMessage::NoteOn {
                channel: 1,
                note: 60,
                velocity: 100
            }]
        );
        assert_eq!(encode_note(1, 60, 0), [0x80, 60, 0]);
        let get = encode_nrpn_get(1, 0x4F << 7);
        assert_eq!(get, [0xB0, 99, 0x4F, 0xB0, 98, 0, 0xB0, 96, 0x7F]);
        let mut a = NrpnAssembler::new();
        assert!(parse(&get).iter().all(|m| a.push(m).is_none()));
        assert_eq!(
            encode_control(&addr(MidiKind::Note, 36), 1),
            vec![0x90, 36, 1]
        );
        assert_eq!(
            encode_control(&addr(MidiKind::ControlChange, 7), 500),
            vec![0xB0, 7, 127]
        );
    }

    fn learn(bytes: &[u8]) -> (Option<LearnedControl>, Option<LearnedControl>) {
        let mut l = MidiLearner::new();
        let fin = l.feed(bytes);
        (fin, l.result())
    }

    #[test]
    fn learner_note_is_a_final_button() {
        let (fin, _) = learn(&[0x80, 40, 0, 0x91, 36, 100]);
        let fin = fin.unwrap();
        assert_eq!(fin.kind, tessera_control_types::ControlKind::Button);
        assert_eq!(fin.midi, Some(MidiAddress::new(MidiKind::Note, 2, 36)));
        assert_eq!((fin.offset, fin.bits), (0, 0));
    }

    #[test]
    fn learner_nrpn_fader_and_mute() {
        let mut bytes = vec![];
        for v in [12544u16, 12000, 11000, 9000] {
            bytes.extend(encode_nrpn(1, 0x40 << 7, v));
        }
        // Another control after the first is ignored.
        bytes.extend(encode_nrpn(1, 0x41 << 7, 16000));
        let (fin, r) = learn(&bytes);
        assert!(fin.is_none(), "a fader is never final");
        let r = r.unwrap();
        assert_eq!(r.kind, tessera_control_types::ControlKind::Fader);
        assert_eq!(r.max, Some(12544));
        assert_eq!(r.midi.unwrap().number, 0x40 << 7);

        let (fin, r) = learn(&encode_nrpn(1, 5, 1));
        assert!(fin.is_none());
        let r = r.unwrap();
        assert_eq!(r.kind, tessera_control_types::ControlKind::Button);
        assert_eq!(r.max, None);
        assert_eq!(r.midi.unwrap().kind, MidiKind::Nrpn);
    }

    #[test]
    fn learner_cc_button_final_on_release_and_cc_fader() {
        let (fin, _) = learn(&[0xB0, 20, 127, 20, 0]);
        assert_eq!(
            fin.unwrap().kind,
            tessera_control_types::ControlKind::Button
        );
        let (fin, r) = learn(&[0xB0, 7, 10, 7, 11, 7, 40]);
        assert!(fin.is_none());
        let r = r.unwrap();
        assert_eq!(r.kind, tessera_control_types::ControlKind::Fader);
        assert_eq!(r.max, Some(40));
        assert_eq!(learn(&[0xF8, 0xFE]).1, None);
    }
}
