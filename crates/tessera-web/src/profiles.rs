//! Surface profiles ("mixboard" profiles) on the portal: the format is
//! [`tessera_surface::profile`] (described in `docs/surface-profiles.md`);
//! this module adds what the pages need around it: which profiles fit a
//! surface, applying one to a surface, a surface as a profile, and reading
//! an uploaded file.
//!
//! The list is the daemon's (`Query::SurfaceProfiles`: built in and
//! installed), plus one imported per surface in this portal run.

use std::collections::BTreeMap;

use tessera_control_types::{
    BindingAction, ButtonMode, ControlKind, CurvePoint, DetectedDevice, SurfaceControl,
    SurfaceSource, SurfaceSpec,
};
pub use tessera_surface::SurfaceProfile as Profile;
use tessera_surface::profile::{ProfileBinding, ProfileMatch};

/// A level action, the kind a fader law applies to: a channel's input
/// level, the RX output level, the host (speaker) volume, a router port or
/// crosspoint level.
#[must_use]
pub const fn is_gain(a: BindingAction) -> bool {
    matches!(
        a,
        BindingAction::Gain
            | BindingAction::OutputGain
            | BindingAction::HostVolume
            | BindingAction::PortGain
            | BindingAction::CrosspointGain
    )
}

/// An action a fader law applies to: a level action ([`is_gain`]), and a
/// follow, whose law turns the fader into the dB it passes on (the followed
/// control acts on it as if it had moved itself).
#[must_use]
pub const fn takes_law(a: BindingAction) -> bool {
    is_gain(a) || matches!(a, BindingAction::Follow)
}

/// A mute action: the channel's input or the RX output.
#[must_use]
pub const fn is_mute(a: BindingAction) -> bool {
    matches!(a, BindingAction::Mute | BindingAction::OutputMute)
}

/// An action on the RX output stage (after the local mix).
#[must_use]
pub const fn is_output(a: BindingAction) -> bool {
    matches!(a, BindingAction::OutputGain | BindingAction::OutputMute)
}

/// Does a binding with action `a` fit a control of kind `k`?
#[must_use]
pub const fn action_fits(k: ControlKind, a: BindingAction) -> bool {
    match k {
        ControlKind::Button => a.is_mute() || matches!(a, BindingAction::Follow),
        _ => a.is_level() || matches!(a, BindingAction::Follow),
    }
}

/// Largest profile file the portal accepts, bytes.
pub const MAX_PROFILE_BYTES: usize = 256 * 1024;

/// A profile the daemon reported. Its list has no suggested bindings: a
/// profile with one fader law gives it to every fader, and the suggestions
/// (mode, output stage, example channels) come from the built-in profile of
/// the same name and controls when there is one.
#[must_use]
pub fn from_info(info: &tessera_control_types::SurfaceProfileInfo) -> Profile {
    let curves: BTreeMap<String, Vec<CurvePoint>> = info
        .curves
        .iter()
        .map(|c| (c.name.clone(), c.points.clone()))
        .collect();
    let usb_id = info.vendor_id.zip(info.product_id);
    let mut p = Profile {
        name: info.name.clone(),
        vendor: info.vendor.clone(),
        model: info.model.clone(),
        description: info.description.clone(),
        matching: ProfileMatch {
            midi_card: info.midi_card.clone(),
            usb_id,
        },
        curves,
        controls: info.controls.clone(),
        bindings: Vec::new(),
    };
    if let Some(b) = tessera_surface::presets::builtin()
        .into_iter()
        .find(|b| b.name == p.name && b.controls == p.controls)
    {
        p.bindings = b.bindings;
    } else if p.curves.len() == 1 {
        let law = p.curves.keys().next().cloned();
        p.bindings = p
            .controls
            .iter()
            .filter(|c| c.kind == ControlKind::Fader)
            .map(|c| ProfileBinding {
                control: c.id.clone(),
                action: BindingAction::Gain,
                mode: ButtonMode::Toggle,
                curve: law.clone(),
                direction: None,
                channels: Vec::new(),
            })
            .collect();
    }
    p
}

/// Read a profile: JSON when the text starts with `{`, else TOML. The
/// error says what is wrong and where.
///
/// # Errors
/// The text is not a profile, or not a valid one.
pub fn parse(text: &str) -> Result<Profile, String> {
    if text.len() > MAX_PROFILE_BYTES {
        return Err(format!(
            "The file is larger than {} KiB: not a profile.",
            MAX_PROFILE_BYTES / 1024
        ));
    }
    if text.trim_start().starts_with('{') {
        Profile::from_json_str(text).map_err(|e| format!("Not a valid JSON profile: {}", e.message))
    } else {
        Profile::from_toml_str(text).map_err(|e| format!("Not a valid TOML profile: {}", e.message))
    }
}

/// What the pages call a profile: vendor and model when it has them.
#[must_use]
pub fn title(p: &Profile) -> String {
    match (p.vendor.trim(), p.model.trim()) {
        ("", "") => p.name.clone(),
        (v, "") => format!("{v} ({})", p.name),
        ("", m) => m.to_owned(),
        (v, m) => format!("{v} {m}"),
    }
}

/// What the portal asks of a profile.
pub trait ProfileExt {
    /// Are its controls MIDI messages (rather than HID report fields)?
    fn is_midi(&self) -> bool;
    /// Does it fit this source (and, when known, this detected device)?
    fn fits(&self, source: &SurfaceSource, detected: Option<&DetectedDevice>) -> bool;
    /// The fader law its suggested mapping gives control `c` (the same
    /// control: id, kind and MIDI message).
    fn curve_for(&self, c: &SurfaceControl) -> Option<Vec<CurvePoint>>;
    /// The mode its suggested mute mapping gives button `c`.
    fn mode_for(&self, c: &SurfaceControl) -> Option<ButtonMode>;
    /// Its suggested mapping of control `c` (the same control).
    fn suggestion_for(&self, c: &SurfaceControl) -> Option<ProfileBinding>;
    /// Apply it to a surface: its controls replace those with the same id
    /// and the others are added; the surface's own other controls and its
    /// mappings stay, and gain mappings of its faders take its fader law.
    ///
    /// # Errors
    /// The profile is for another kind of device than the surface.
    fn apply(&self, spec: &mut SurfaceSpec) -> Result<String, String>;
}

fn same(a: &SurfaceControl, b: &SurfaceControl) -> bool {
    a.id == b.id && a.kind == b.kind && a.midi == b.midi
}

/// The curve name a profile suggests for control `id`'s gain mapping.
fn curve_name<'a>(p: &'a Profile, id: &str) -> Option<&'a str> {
    p.bindings
        .iter()
        .find(|b| b.control == id && takes_law(b.action))
        .and_then(|b| b.curve.as_deref())
}

impl ProfileExt for Profile {
    fn is_midi(&self) -> bool {
        self.controls.iter().any(|c| c.midi.is_some())
    }

    fn fits(&self, source: &SurfaceSource, detected: Option<&DetectedDevice>) -> bool {
        match source {
            SurfaceSource::Midi { card, .. } => {
                let name = detected.map_or("", |d| d.name.as_str());
                self.matches_midi_card(card, name)
            }
            SurfaceSource::Hidraw {
                vendor_id,
                product_id,
                ..
            } => self.matches_usb(*vendor_id, *product_id),
            _ => false,
        }
    }

    fn curve_for(&self, c: &SurfaceControl) -> Option<Vec<CurvePoint>> {
        let own = self.controls.iter().find(|x| same(x, c))?;
        let law = self.curve(curve_name(self, &own.id)?);
        (!law.is_empty()).then_some(law)
    }

    fn mode_for(&self, c: &SurfaceControl) -> Option<ButtonMode> {
        let own = self.controls.iter().find(|x| same(x, c))?;
        self.bindings
            .iter()
            .find(|b| b.control == own.id && is_mute(b.action))
            .map(|b| b.mode)
    }

    fn suggestion_for(&self, c: &SurfaceControl) -> Option<ProfileBinding> {
        let own = self.controls.iter().find(|x| same(x, c))?;
        self.bindings.iter().find(|b| b.control == own.id).cloned()
    }

    fn apply(&self, spec: &mut SurfaceSpec) -> Result<String, String> {
        let title = title(self);
        let midi_surface = matches!(
            spec.source,
            SurfaceSource::Midi { .. } | SurfaceSource::TcpMidi { .. }
        );
        if self.is_midi() && !midi_surface {
            return Err(format!(
                "The profile {title} describes MIDI messages, but this surface is not a MIDI device."
            ));
        }
        if !self.is_midi() && midi_surface {
            return Err(format!(
                "The profile {title} has no MIDI messages, but this surface is a MIDI device."
            ));
        }
        let (mut added, mut updated, mut laws) = (0, 0, 0);
        for c in &self.controls {
            match spec.controls.iter_mut().find(|x| x.id == c.id) {
                Some(slot) => {
                    if slot != c {
                        updated += 1;
                    }
                    slot.clone_from(c);
                }
                None => {
                    added += 1;
                    spec.controls.push(c.clone());
                }
            }
            let law = curve_name(self, &c.id).map(|n| self.curve(n));
            if let Some(law) = law.filter(|l| !l.is_empty()) {
                for b in spec.bindings.iter_mut().filter(|b| {
                    // A follow takes it on a fader only (a button passes
                    // on its state, not a level).
                    b.control == c.id
                        && (is_gain(b.action)
                            || (c.kind == ControlKind::Fader && takes_law(b.action)))
                }) {
                    b.curve.clone_from(&law);
                    laws += 1;
                }
            }
        }
        // Mappings whose control changed kind no longer fit.
        let kinds: Vec<(String, ControlKind)> = spec
            .controls
            .iter()
            .map(|c| (c.id.clone(), c.kind))
            .collect();
        let before = spec.bindings.len();
        spec.bindings.retain(|b| {
            kinds
                .iter()
                .any(|(id, k)| *id == b.control && action_fits(*k, b.action))
        });
        let dropped = before - spec.bindings.len();
        let mut msg =
            format!("Profile {title} applied: {added} control(s) added, {updated} updated.");
        if laws > 0 {
            msg.push_str(&format!(" {laws} fader mapping(s) now use its fader law."));
        }
        if dropped > 0 {
            msg.push_str(&format!(
                " {dropped} mapping(s) that no longer fit their control were removed."
            ));
        }
        msg.push_str(" Map the controls you use.");
        Ok(msg)
    }
}

/// A surface as a profile: its controls, the fader laws its gain mappings
/// use, and what each mapped control is for (without its channels).
#[must_use]
pub fn of_surface(spec: &SurfaceSpec) -> Profile {
    let mut curves: BTreeMap<String, Vec<CurvePoint>> = BTreeMap::new();
    let mut bindings = Vec::new();
    for c in &spec.controls {
        let Some(b) = spec.bindings.iter().find(|b| b.control == c.id) else {
            continue;
        };
        // A profile is shared between hosts: router ports, crosspoints and
        // followed surfaces belong to this one, so those bindings stay out.
        if matches!(
            b.action,
            BindingAction::PortGain
                | BindingAction::PortMute
                | BindingAction::CrosspointGain
                | BindingAction::CrosspointMute
                | BindingAction::Follow
        ) {
            continue;
        }
        let host = matches!(
            b.action,
            BindingAction::HostVolume | BindingAction::HostMute
        );
        let curve = (!b.curve.is_empty()).then(|| {
            if let Some((name, _)) = curves.iter().find(|(_, v)| **v == b.curve) {
                return name.clone();
            }
            let name = if curves.is_empty() {
                "fader".to_owned()
            } else {
                format!("fader-{}", curves.len() + 1)
            };
            curves.insert(name.clone(), b.curve.clone());
            name
        });
        bindings.push(ProfileBinding {
            control: c.id.clone(),
            action: b.action,
            mode: b.mode,
            curve,
            // A host volume suggestion names no channels.
            direction: (!host).then_some(b.target.direction),
            channels: if host {
                Vec::new()
            } else {
                b.target.channels.iter().map(|n| n.get()).collect()
            },
        });
    }
    let matching = match &spec.source {
        SurfaceSource::Midi { card, .. } => ProfileMatch {
            midi_card: Some(card.clone()),
            usb_id: None,
        },
        SurfaceSource::Hidraw {
            vendor_id,
            product_id,
            ..
        } => ProfileMatch {
            midi_card: None,
            usb_id: Some((*vendor_id, *product_id)),
        },
        _ => ProfileMatch::default(),
    };
    Profile {
        name: spec.name.clone(),
        vendor: String::new(),
        model: String::new(),
        description: None,
        matching,
        curves,
        controls: spec.controls.clone(),
        bindings,
    }
}

/// The gain a fader law gives at raw position `raw`, milli-dB (`None`:
/// off). Linear between points; held at the first and last point outside
/// them; raw 0 is off.
#[must_use]
pub fn curve_mdb(points: &[CurvePoint], raw: u32) -> Option<i32> {
    if raw == 0 || points.is_empty() {
        return None;
    }
    let first = points[0];
    if raw <= first.raw {
        return Some(first.mdb);
    }
    for w in points.windows(2) {
        let (a, b) = (w[0], w[1]);
        if raw <= b.raw {
            let t = f64::from(raw - a.raw) / f64::from(b.raw - a.raw);
            #[allow(clippy::cast_possible_truncation)]
            let m = (f64::from(a.mdb) + t * f64::from(b.mdb - a.mdb)).round() as i32;
            return Some(m);
        }
    }
    points.last().map(|p| p.mdb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_control_types::{Channel, ChannelSet, DeviceKey, Direction, SurfaceBinding};

    fn cq() -> Profile {
        tessera_surface::presets::builtin()
            .into_iter()
            .find(|p| p.name == tessera_surface::presets::ALLEN_HEATH_CQ)
            .unwrap()
    }

    #[test]
    fn the_cq_law_reads_as_the_desk_shows_it() {
        let p = cq();
        let ip1 = p.controls.iter().find(|c| c.id == "ip1").unwrap();
        let law = p.curve_for(ip1).unwrap();
        assert_eq!(curve_mdb(&law, 0), None);
        assert_eq!(curve_mdb(&law, 12_544), Some(0));
        assert_eq!(curve_mdb(&law, 16_320), Some(10_000));
        assert_eq!(curve_mdb(&law, 6_944), Some(-15_000));
        assert_eq!(title(&p), "Allen & Heath CQ series");
    }

    #[test]
    fn bad_files_are_refused_with_the_reason() {
        let e = parse("name = \"x\"\ncontrol = []\n").unwrap_err();
        assert!(e.starts_with("Not a valid TOML profile"), "{e}");
        let e = parse("nonsense = 1\n").unwrap_err();
        assert!(e.contains("TOML"), "{e}");
        let e = parse("{\"name\": 3}").unwrap_err();
        assert!(e.contains("JSON"), "{e}");
    }

    #[test]
    fn applying_merges_controls_and_gives_mapped_faders_the_law() {
        let p = cq();
        let first = p.controls[0].clone();
        let mut spec = SurfaceSpec::new(
            "cq",
            SurfaceSource::Midi {
                card: "CQ".into(),
                device: 0,
            },
            vec![first.clone()],
            vec![SurfaceBinding::new(
                first.id.clone(),
                BindingAction::Gain,
                ChannelSet {
                    device: DeviceKey("main".into()),
                    direction: Direction::Tx,
                    channels: vec![Channel::new(1).unwrap()],
                },
            )],
        );
        let msg = p.apply(&mut spec).unwrap();
        assert!(msg.contains("45 control(s) added"), "{msg}");
        assert_eq!(spec.controls.len(), p.controls.len());
        assert_eq!(spec.bindings[0].curve.len(), 12);
        assert!(p.fits(&spec.source, None));
        let hid = SurfaceSource::Hidraw {
            vendor_id: 1,
            product_id: 2,
            interface: None,
        };
        assert!(!p.fits(&hid, None));
        let mut wrong = spec.clone();
        wrong.source = hid;
        assert!(p.apply(&mut wrong).is_err());
        // Exported again, it is a valid profile and the law comes along.
        let back = of_surface(&spec);
        back.validate().unwrap();
        let text = back.to_toml_string().unwrap();
        let again = parse(&text).unwrap();
        assert_eq!(again.curve_for(&first).map(|l| l.len()), Some(12));
        assert_eq!(again.matching.midi_card.as_deref(), Some("CQ"));
    }

    #[test]
    fn applying_gives_a_followed_fader_the_law_too() {
        // A follow passes the fader on as dB: the law that turns it into dB
        // applies as for a level mapping.
        assert!(takes_law(BindingAction::Follow));
        assert!(!is_gain(BindingAction::Follow));
        let p = cq();
        let first = p.controls[0].clone();
        assert_eq!(first.kind, ControlKind::Fader);
        let mut follow = SurfaceBinding::new(
            first.id.clone(),
            BindingAction::Follow,
            ChannelSet {
                device: DeviceKey(String::new()),
                direction: Direction::Rx,
                channels: Vec::new(),
            },
        );
        follow.follow = Some(tessera_control_types::ControlRef::new("other", "f1"));
        let mut spec = SurfaceSpec::new(
            "cq",
            SurfaceSource::Midi {
                card: "CQ".into(),
                device: 0,
            },
            vec![first],
            vec![follow],
        );
        let msg = p.apply(&mut spec).unwrap();
        assert_eq!(spec.bindings[0].curve.len(), 12, "{msg}");
        assert!(
            msg.contains("1 fader mapping(s) now use its fader law"),
            "{msg}"
        );
    }
}
