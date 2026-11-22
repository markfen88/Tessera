//! The conversions between the contract types and `tessera-surface`:
//! saved specs to the mapper's configuration, mapper actions to typed
//! commands, gains both ways, and decoder events to live control values;
//! and the daemon's use of the MIDI side of `tessera-surface` (decoder,
//! learner, the NRPN "get").

use tessera_control_types::{
    ChannelSet, Command, DeviceKey, GAIN_CEILING_MDB, GAIN_FLOOR_MDB, Gain, LearnedControl,
    MidiAddress, MidiKind, SurfaceSpec,
};
use tessera_surface::config::{SurfaceConfig, Target};
use tessera_surface::midi::{MidiDecoder, MidiLearner, encode_nrpn_get};
use tessera_surface::{SurfaceAction, SurfaceEvent, SurfaceEventKind};

#[allow(clippy::cast_precision_loss)]
fn db(mdb: i32) -> f32 {
    mdb as f32 / 1000.0
}

/// The sans-IO crate's configuration for a saved surface.
#[must_use]
pub fn config_of(spec: &SurfaceSpec) -> SurfaceConfig {
    SurfaceConfig::from(spec)
}

/// A MIDI surface's input: bytes from the rawmidi port → events of its
/// controls (by each control's [`MidiAddress`]); and the bytes that show a
/// value on the desk.
#[derive(Debug)]
pub struct MidiIn(MidiDecoder);

impl MidiIn {
    /// For the controls of `cfg` that have a MIDI address.
    #[must_use]
    pub fn new(cfg: &SurfaceConfig) -> Self {
        Self(MidiDecoder::new(cfg))
    }

    /// Decode `bytes` (any split of the stream). An NRPN button reports
    /// the desk's state (every report; the mapper sets the mute to it).
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<SurfaceEvent>) {
        self.0.feed(bytes, out);
    }

    /// An NRPN button's 7-bit value waits for its fine byte: call
    /// [`flush`](Self::flush) when nothing more came within
    /// [`tessera_surface::midi::NRPN_HOLD_MS`].
    #[must_use]
    pub fn has_held(&self) -> bool {
        self.0.has_held()
    }

    /// Pass on the held values (no fine byte came).
    pub fn flush(&mut self, out: &mut Vec<SurfaceEvent>) {
        self.0.flush(out);
    }

    /// The controls the bytes fed since the last call addressed, unchanged
    /// values included (see [`MidiDecoder::take_heard`]).
    pub fn take_heard(&mut self) -> Vec<String> {
        self.0.take_heard()
    }

    /// The bytes that put `control` at `value` (event scale: a fader's
    /// position, a button's 0/1); `None` for a control without a MIDI
    /// address.
    #[must_use]
    pub fn output(&self, control: &str, value: u32) -> Option<Vec<u8>> {
        self.0.output(control, value)
    }

    /// A fader was sent to `value`: the event to hand to the mapper (a desk
    /// need not echo it).
    pub fn moved(&mut self, control: &str, value: u32) -> Option<SurfaceEvent> {
        self.0.moved(control, value)
    }
}

/// Learns which MIDI control moves (used like the HID learner; MIDI has no
/// idle noise to learn: realtime bytes are not messages).
#[derive(Debug, Default)]
pub struct MidiLearn(MidiLearner);

impl MidiLearn {
    /// A learner (`moving_up` has no use for MIDI: the desk's values say
    /// which way is up).
    #[must_use]
    pub fn new(_moving_up: bool) -> Self {
        Self(MidiLearner::new())
    }

    /// Bytes during the hands-off start: ignored.
    pub fn idle(&mut self, _bytes: &[u8]) {}

    /// Bytes while a control moves; `Some` for a finished button press.
    pub fn feed(&mut self, bytes: &[u8]) -> Option<LearnedControl> {
        self.0.feed(bytes)
    }

    /// The best guess so far.
    #[must_use]
    pub fn result(&self) -> Option<LearnedControl> {
        self.0.result()
    }
}

/// The request that makes the desk send the control's current value, where
/// the protocol has one (the Allen & Heath NRPN "get").
#[must_use]
pub fn midi_get(addr: &MidiAddress) -> Option<Vec<u8>> {
    (addr.kind == MidiKind::Nrpn).then(|| encode_nrpn_get(addr.channel, addr.number).to_vec())
}

/// A mapper gain (dB, `-inf` for off) as a contract gain: rounded to the
/// milli-dB, off below the floor, clamped to the ceiling.
#[must_use]
pub fn gain_of_db(db: f32) -> Gain {
    if db.is_nan() || db == f32::NEG_INFINITY {
        return Gain::Off;
    }
    let m = (f64::from(db) * 1000.0).round();
    if m < f64::from(GAIN_FLOOR_MDB) {
        Gain::Off
    } else {
        #[allow(clippy::cast_possible_truncation)]
        Gain::MilliDb((m.min(f64::from(GAIN_CEILING_MDB))) as i32)
    }
}

/// A contract gain as the mapper's dB (`-inf` for off).
#[must_use]
pub fn db_of_gain(g: Gain) -> f32 {
    g.milli_db().map_or(f32::NEG_INFINITY, db)
}

fn channel_set(t: &Target) -> Option<ChannelSet> {
    let channels = t
        .channels
        .iter()
        .map(|&c| tessera_control_types::Channel::new(c))
        .collect::<Option<Vec<_>>>()?;
    Some(ChannelSet {
        device: DeviceKey(t.device.clone()),
        direction: t.direction,
        channels,
    })
}

/// The typed command for a mapper action; `None` for an action the daemon
/// does not carry out as a command (a fader move goes to the device) or an
/// impossible target.
#[must_use]
pub fn command_of(action: &SurfaceAction) -> Option<Command> {
    match action {
        SurfaceAction::SetMute { target, muted } => Some(Command::SetChannelMute {
            target: channel_set(target)?,
            muted: *muted,
        }),
        SurfaceAction::SetGain { target, gain_db } => Some(Command::SetChannelGain {
            target: channel_set(target)?,
            gain: gain_of_db(*gain_db),
        }),
        SurfaceAction::SetOutputMute { target, muted } => Some(Command::SetOutputMute {
            target: channel_set(target)?,
            muted: *muted,
        }),
        SurfaceAction::SetOutputGain { target, gain_db } => Some(Command::SetOutputGain {
            target: channel_set(target)?,
            gain: gain_of_db(*gain_db),
        }),
        SurfaceAction::SetHostVolume { gain_db } => Some(Command::SetHostVolume {
            gain: gain_of_db(*gain_db),
        }),
        SurfaceAction::SetHostMute { muted } => Some(Command::SetHostMute { muted: *muted }),
        SurfaceAction::SetPortGain { ports, gain_db } => Some(Command::SetPortGain {
            ports: ports.clone(),
            gain: gain_of_db(*gain_db),
        }),
        SurfaceAction::SetPortMute { ports, muted } => Some(Command::SetPortMute {
            ports: ports.clone(),
            muted: *muted,
        }),
        SurfaceAction::SetCrosspointGain {
            crosspoints,
            gain_db,
        } => Some(Command::SetCrosspointGain {
            crosspoints: crosspoints.clone(),
            gain: gain_of_db(*gain_db),
        }),
        SurfaceAction::SetCrosspointMute { crosspoints, muted } => {
            Some(Command::SetCrosspointMute {
                crosspoints: crosspoints.clone(),
                muted: *muted,
            })
        }
        // To the device, or to another surface's device.
        SurfaceAction::MoveFader { .. } | SurfaceAction::Follow { .. } => None,
    }
}

/// A decoder event as the contract's live control value (`None` for
/// relative controls, which have no position).
#[must_use]
pub fn value_of(kind: &SurfaceEventKind) -> Option<tessera_control_types::ControlValue> {
    use tessera_control_types::ControlValue;
    match *kind {
        SurfaceEventKind::Button { pressed } => Some(ControlValue::Button { pressed }),
        SurfaceEventKind::Fader { value, max } => Some(ControlValue::Fader { value, max }),
        SurfaceEventKind::Encoder { .. } => None,
    }
}

/// Directories of profile files, under the device root: shipped, then the
/// administrator's (a later profile replaces an earlier one of the same
/// name, a file one a built-in one).
pub const PROFILE_DIRS: [&str; 2] = ["usr/share/tessera/surfaces", "etc/tessera/surfaces.d"];

/// The built-in profiles and those of [`PROFILE_DIRS`] under `root`, as the
/// contract lists them; each file that cannot be used is logged and left
/// out. File reads: call off the async workers.
#[must_use]
pub fn load_profiles(root: &std::path::Path) -> Vec<tessera_control_types::SurfaceProfileInfo> {
    let mut out: Vec<tessera_control_types::SurfaceProfileInfo> =
        tessera_surface::presets::builtin()
            .iter()
            .map(|p| profile_info(p, "builtin"))
            .collect();
    for dir in PROFILE_DIRS {
        let path = root.join(dir);
        let (profiles, errors) = tessera_surface::profile::load_dir(&path);
        for e in errors {
            tracing::warn!(
                "surface profile {}: {}",
                crate::sanitize::log_safe(&e.origin, 200),
                crate::sanitize::log_safe(&e.message, 300)
            );
        }
        let origin = format!("/{dir}");
        for p in &profiles {
            let info = profile_info(p, &origin);
            match out.iter_mut().find(|o| o.name == info.name) {
                Some(o) => *o = info,
                None => out.push(info),
            }
        }
    }
    out
}

/// A profile as the contract lists it.
#[must_use]
pub fn profile_info(
    p: &tessera_surface::SurfaceProfile,
    origin: &str,
) -> tessera_control_types::SurfaceProfileInfo {
    let mut i = tessera_control_types::SurfaceProfileInfo::new(
        p.name.clone(),
        p.vendor.clone(),
        p.model.clone(),
        origin,
    );
    i.description.clone_from(&p.description);
    i.midi_card.clone_from(&p.matching.midi_card);
    i.vendor_id = p.matching.usb_id.map(|u| u.0);
    i.product_id = p.matching.usb_id.map(|u| u.1);
    i.controls.clone_from(&p.controls);
    i.curves = p
        .curves
        .iter()
        .map(|(name, points)| tessera_control_types::NamedCurve::new(name.clone(), points.clone()))
        .collect();
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_control_types::{
        BindingAction as Ba, Channel, ControlKind as CtControlKind, Direction, SurfaceBinding,
        SurfaceControl, SurfaceSource,
    };

    #[test]
    fn gains_convert_both_ways() {
        assert_eq!(gain_of_db(f32::NEG_INFINITY), Gain::Off);
        assert_eq!(gain_of_db(f32::NAN), Gain::Off);
        assert_eq!(gain_of_db(-6.0204), Gain::MilliDb(-6020));
        assert_eq!(gain_of_db(-200.0), Gain::Off);
        assert_eq!(gain_of_db(40.0), Gain::MilliDb(GAIN_CEILING_MDB));
        assert_eq!(db_of_gain(Gain::Off), f32::NEG_INFINITY);
        assert!((db_of_gain(Gain::MilliDb(-250)) + 0.25).abs() < 1e-6);
    }

    #[test]
    fn a_spec_becomes_a_valid_config_and_actions_become_commands() {
        let target = ChannelSet {
            device: DeviceKey("main".into()),
            direction: Direction::Tx,
            channels: vec![Channel::new(1).unwrap(), Channel::new(2).unwrap()],
        };
        let spec = SurfaceSpec::new(
            "desk",
            SurfaceSource::Hidraw {
                vendor_id: 0x3142,
                product_id: 0x0c33,
                interface: Some(3),
            },
            vec![SurfaceControl {
                id: "m".into(),
                label: None,
                kind: CtControlKind::Button,
                report_id: Some(1),
                offset: Some(48),
                bits: None,
                max: None,
                inverted: false,
                active_low: false,
                motorized: false,
                code: None,
                midi: None,
            }],
            vec![SurfaceBinding::new("m", Ba::Mute, target.clone())],
        );
        let cfg = config_of(&spec);
        cfg.validate().unwrap();
        assert_eq!(cfg.bindings[0].target.channels, vec![1, 2]);
        assert!((cfg.bindings[0].min_db + 60.0).abs() < 1e-6);
        let cmd = command_of(&SurfaceAction::SetMute {
            target: cfg.bindings[0].target.clone(),
            muted: true,
        });
        assert_eq!(
            cmd,
            Some(Command::SetChannelMute {
                target: target.clone(),
                muted: true
            })
        );
        // Output actions become output commands.
        let mut rx = target.clone();
        rx.direction = Direction::Rx;
        let t = tessera_surface::Target {
            device: "main".into(),
            direction: Direction::Rx,
            channels: vec![1, 2],
        };
        assert_eq!(
            command_of(&SurfaceAction::SetOutputGain {
                target: t.clone(),
                gain_db: -6.0,
            }),
            Some(Command::SetOutputGain {
                target: rx.clone(),
                gain: Gain::MilliDb(-6_000)
            })
        );
        assert_eq!(
            command_of(&SurfaceAction::SetOutputMute {
                target: t,
                muted: true,
            }),
            Some(Command::SetOutputMute {
                target: rx,
                muted: true
            })
        );
    }
}
