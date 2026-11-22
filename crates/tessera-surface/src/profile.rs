//! Surface profiles: a description of a MIDI or HID device's controls in a
//! file people can write and share, so a surface for that device needs no
//! learning.
//!
//! A profile names the device, says how to recognise it (`[match]`), lists
//! its controls (the same fields as a surface's controls, including each
//! one's `midi` message or HID report layout), named fader laws
//! (`[curves]`), and optionally suggests what each control does
//! (`[[binding]]`; the device and channels belong to the host, so any
//! `direction` and `channels` there are only an example to start from).
//!
//! TOML is the preferred form (the project's configuration language); JSON
//! is accepted too. A profile file is read strictly: a key it does not know
//! (a typo such as `motorised`), anywhere in the file, is an error naming
//! the key (and, from [`load_dir`], the file), never silently ignored. See `profiles/allen-heath-cq.toml` for an example. The
//! daemon reads every `*.toml` and `*.json` file of a directory with
//! [`load_dir`] (`/usr/share/tessera/surfaces/`, then
//! `/etc/tessera/surfaces.d/`).
//!
//! ```toml
//! name = "my-controller"
//! vendor = "Acme"
//! model = "Faders 2"
//!
//! [match]
//! midi_card = "Faders2"          # part of the ALSA card id or name
//!
//! [curves]
//! fader = [{ raw = 1, mdb = -60000 }, { raw = 127, mdb = 0 }]
//!
//! [[control]]
//! id = "f1"
//! kind = "fader"
//! midi = { kind = "control_change", channel = 1, number = 7 }
//!
//! [[binding]]
//! control = "f1"
//! action = "gain"
//! curve = "fader"
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize};
use tessera_control_types::{
    BindingAction, ButtonMode, ControlKind, CurvePoint, Direction, MidiAddress, MidiKind,
    SurfaceControl, SurfaceSource, SurfaceSpec,
};

use crate::config::{SurfaceConfig, check_curve, is_identifier};

/// How to recognise the device a profile describes. Every key given must
/// match.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileMatch {
    /// Part of the ALSA card id or name (`/proc/asound/cards`), compared
    /// without regard to case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub midi_card: Option<String>,
    /// USB vendor and product id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usb_id: Option<(u16, u16)>,
}

impl ProfileMatch {
    fn is_empty(&self) -> bool {
        self.midi_card.is_none() && self.usb_id.is_none()
    }
}

/// A suggested binding: what a control is for. The channels it acts on are
/// chosen when the surface is set up; `direction` and `channels` are only an
/// example to start from (the device is always the host's choice).
///
/// A suggestion is one of the actions a shared file can mean on any host:
/// `gain`, `output_gain` or `host_volume` for a fader or encoder, `mute`,
/// `output_mute` or `host_mute` for a button. The host volume and mute take
/// no `direction` or `channels`. The router actions (`port_*`,
/// `crosspoint_*`) and `follow` name one host's router ports, crosspoints or
/// surfaces, so a profile cannot suggest them: they are mapped on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileBinding {
    pub control: String,
    pub action: BindingAction,
    /// Buttons bound to mute.
    #[serde(default)]
    pub mode: ButtonMode,
    /// Faders: the name of a curve in `[curves]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curve: Option<String>,
    /// Suggested direction of the channels: an example. The output actions
    /// are RX only (and imply RX when left out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<Direction>,
    /// Suggested 1-based channels: an example, changed when the surface is
    /// set up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<u16>,
}

impl ProfileBinding {
    /// The suggested direction: the one given, or RX for the output
    /// actions (only RX channels have an output stage).
    #[must_use]
    pub fn suggested_direction(&self) -> Option<Direction> {
        self.direction.or(matches!(
            self.action,
            BindingAction::OutputMute | BindingAction::OutputGain
        )
        .then_some(Direction::Rx))
    }
}

/// One device description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceProfile {
    /// Identifier (letters, digits, `-`, `_`), unique among profiles.
    pub name: String,
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(
        rename = "match",
        default,
        skip_serializing_if = "ProfileMatch::is_empty"
    )]
    pub matching: ProfileMatch,
    /// Named fader laws (raw position → milli-dB), for example `fader`.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "strict_curves"
    )]
    pub curves: BTreeMap<String, Vec<CurvePoint>>,
    #[serde(
        rename = "control",
        alias = "controls",
        deserialize_with = "strict_controls"
    )]
    pub controls: Vec<SurfaceControl>,
    #[serde(
        rename = "binding",
        alias = "bindings",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub bindings: Vec<ProfileBinding>,
}

/// A [`MidiAddress`] as a profile file spells it: unknown keys refused.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileMidi {
    kind: MidiKind,
    channel: u8,
    number: u16,
}

/// A [`SurfaceControl`] as a profile file spells it: the same keys and
/// defaults, unknown keys refused (the wire contract stays lenient).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileControl {
    id: String,
    #[serde(default)]
    label: Option<String>,
    kind: ControlKind,
    #[serde(default)]
    report_id: Option<u8>,
    #[serde(default)]
    offset: Option<u32>,
    #[serde(default)]
    bits: Option<u8>,
    #[serde(default)]
    max: Option<u32>,
    #[serde(default)]
    inverted: bool,
    #[serde(default)]
    active_low: bool,
    #[serde(default)]
    motorized: bool,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    midi: Option<FileMidi>,
}

impl From<FileControl> for SurfaceControl {
    fn from(c: FileControl) -> Self {
        SurfaceControl {
            id: c.id,
            label: c.label,
            kind: c.kind,
            report_id: c.report_id,
            offset: c.offset,
            bits: c.bits,
            max: c.max,
            inverted: c.inverted,
            active_low: c.active_low,
            motorized: c.motorized,
            code: c.code,
            midi: c
                .midi
                .map(|m| MidiAddress::new(m.kind, m.channel, m.number)),
        }
    }
}

/// A [`CurvePoint`] as a profile file spells it: unknown keys refused.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePoint {
    raw: u32,
    mdb: i32,
}

fn strict_controls<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<SurfaceControl>, D::Error> {
    let v: Vec<FileControl> = Vec::deserialize(d)?;
    Ok(v.into_iter().map(SurfaceControl::from).collect())
}

fn strict_curves<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, Vec<CurvePoint>>, D::Error> {
    let m: BTreeMap<String, Vec<FilePoint>> = BTreeMap::deserialize(d)?;
    Ok(m.into_iter()
        .map(|(name, points)| {
            let points = points
                .into_iter()
                .map(|p| CurvePoint {
                    raw: p.raw,
                    mdb: p.mdb,
                })
                .collect();
            (name, points)
        })
        .collect())
}

/// A profile that could not be read or is not valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileError {
    /// The file (or `toml`/`json` for text given directly).
    pub origin: String,
    pub message: String,
}

impl core::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.origin, self.message)
    }
}

impl std::error::Error for ProfileError {}

/// An action as a profile file spells it (`port_gain`).
fn action_name(action: BindingAction) -> String {
    match serde_json::to_value(action) {
        Ok(serde_json::Value::String(name)) => name,
        _ => format!("{action:?}"),
    }
}

fn error(origin: &str, message: impl Into<String>) -> ProfileError {
    ProfileError {
        origin: origin.to_owned(),
        message: message.into(),
    }
}

impl SurfaceProfile {
    /// Read and validate a TOML profile.
    ///
    /// # Errors
    /// The parser's message (with line and column) or the first validation
    /// problem.
    pub fn from_toml_str(text: &str) -> Result<Self, ProfileError> {
        let p: Self = toml::from_str(text).map_err(|e| error("toml", e.to_string()))?;
        p.validate().map_err(|m| error("toml", m))?;
        Ok(p)
    }

    /// Read and validate a JSON profile.
    ///
    /// # Errors
    /// As [`from_toml_str`](Self::from_toml_str).
    pub fn from_json_str(text: &str) -> Result<Self, ProfileError> {
        let p: Self = serde_json::from_str(text).map_err(|e| error("json", e.to_string()))?;
        p.validate().map_err(|m| error("json", m))?;
        Ok(p)
    }

    /// The profile as TOML.
    ///
    /// # Errors
    /// The serializer's message (a bug if it happens).
    pub fn to_toml_string(&self) -> Result<String, ProfileError> {
        toml::to_string(self).map_err(|e| error("toml", e.to_string()))
    }

    /// The profile as pretty-printed JSON.
    ///
    /// # Errors
    /// The serializer's message (a bug if it happens).
    pub fn to_json_string(&self) -> Result<String, ProfileError> {
        serde_json::to_string_pretty(self).map_err(|e| error("json", e.to_string()))
    }

    /// Does the profile describe the ALSA card with this id and name? Only
    /// for profiles with a `midi_card` key (a `usb_id` is not checked here).
    #[must_use]
    pub fn matches_midi_card(&self, card_id: &str, card_name: &str) -> bool {
        let Some(want) = &self.matching.midi_card else {
            return false;
        };
        let want = want.to_lowercase();
        card_id.to_lowercase().contains(&want) || card_name.to_lowercase().contains(&want)
    }

    /// Does the profile describe the USB device with these ids? Only for
    /// profiles with a `usb_id` key.
    #[must_use]
    pub fn matches_usb(&self, vendor_id: u16, product_id: u16) -> bool {
        self.matching.usb_id == Some((vendor_id, product_id))
    }

    /// The source a surface built from this profile reads: a MIDI port when
    /// its controls have `midi` addresses (on `card`), otherwise the HID
    /// interface of `[match] usb_id`.
    #[must_use]
    pub fn source(&self, card: &str) -> SurfaceSource {
        if self.controls.iter().any(|c| c.midi.is_some()) {
            SurfaceSource::Midi {
                card: card.to_owned(),
                device: 0,
            }
        } else {
            let (vendor_id, product_id) = self.matching.usb_id.unwrap_or((0, 0));
            SurfaceSource::Hidraw {
                vendor_id,
                product_id,
                interface: None,
            }
        }
    }

    /// The named curve, empty if there is none.
    #[must_use]
    pub fn curve(&self, name: &str) -> Vec<CurvePoint> {
        self.curves.get(name).cloned().unwrap_or_default()
    }

    /// Checks the profile on its own: its name, its controls (the same rules
    /// as a surface's), its curves and its suggested bindings.
    ///
    /// # Errors
    /// The first problem, in words.
    pub fn validate(&self) -> Result<(), String> {
        let p = &self.name;
        if !is_identifier(p) {
            return Err(format!(
                "profile name {p:?} must be letters, digits, `-` or `_`"
            ));
        }
        if self.controls.is_empty() {
            return Err(format!("profile {p:?}: list at least one [[control]]"));
        }
        for (name, curve) in &self.curves {
            if !is_identifier(name) {
                return Err(format!(
                    "profile {p:?}: curve name {name:?} must be letters, digits, `-` or `_`"
                ));
            }
            check_curve(curve).map_err(|m| format!("profile {p:?}: curve {name:?}: {m}"))?;
        }
        let spec = SurfaceSpec::new(
            self.name.clone(),
            self.source("profile"),
            self.controls.clone(),
            Vec::new(),
        );
        SurfaceConfig::from(&spec)
            .validate()
            .map_err(|e| format!("profile {p:?}: {e}"))?;
        for b in &self.bindings {
            let Some(c) = self.controls.iter().find(|c| c.id == b.control) else {
                return Err(format!(
                    "profile {p:?}: binding names unknown control {:?}",
                    b.control
                ));
            };
            let button = c.kind == ControlKind::Button;
            if !matches!(
                b.action,
                BindingAction::Mute
                    | BindingAction::OutputMute
                    | BindingAction::HostMute
                    | BindingAction::Gain
                    | BindingAction::OutputGain
                    | BindingAction::HostVolume
            ) {
                return Err(format!(
                    "profile {p:?}: binding of {:?}: `{}` names one host's router ports, \
                     crosspoints or surfaces, which a profile cannot know; suggest {} and map \
                     it on the host",
                    c.id,
                    action_name(b.action),
                    if button {
                        "`mute`, `output_mute` or `host_mute`"
                    } else {
                        "`gain`, `output_gain` or `host_volume`"
                    }
                ));
            }
            let fits = if button {
                b.action.is_mute()
            } else {
                b.action.is_level()
            };
            if !fits {
                return Err(format!(
                    "profile {p:?}: control {:?} is a {:?} and cannot be bound to {:?}",
                    c.id, c.kind, b.action
                ));
            }
            let output = matches!(
                b.action,
                BindingAction::OutputMute | BindingAction::OutputGain
            );
            if matches!(
                b.action,
                BindingAction::HostVolume | BindingAction::HostMute
            ) {
                let field = if b.direction.is_some() {
                    Some("direction")
                } else {
                    (!b.channels.is_empty()).then_some("channels")
                };
                if let Some(field) = field {
                    let does = if b.action == BindingAction::HostMute {
                        "a host mute binding acts on the host mute"
                    } else {
                        "a host volume binding acts on the host volume"
                    };
                    return Err(format!(
                        "profile {p:?}: binding of {:?}: remove `{field}`: {does}, not on \
                         channels",
                        c.id
                    ));
                }
            }
            if output && b.direction == Some(Direction::Tx) {
                return Err(format!(
                    "profile {p:?}: binding of {:?}: only RX channels have an output level and \
                     mute",
                    c.id
                ));
            }
            if b.channels.contains(&0)
                || (!b.channels.is_empty() && b.suggested_direction().is_none())
            {
                return Err(format!(
                    "profile {p:?}: binding of {:?}: suggested `channels` are 1-based and need \
                     a `direction`",
                    c.id
                ));
            }
            if let Some(curve) = &b.curve {
                if c.kind != ControlKind::Fader {
                    return Err(format!(
                        "profile {p:?}: binding of {:?}: only a fader takes a curve",
                        c.id
                    ));
                }
                if !self.curves.contains_key(curve) {
                    return Err(format!(
                        "profile {p:?}: binding of {:?} names unknown curve {curve:?}",
                        c.id
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Every profile (`*.toml`, `*.json`) in `dir`, in file name order, and an
/// error for each file that could not be read, is not valid, or repeats the
/// name of an earlier profile of the directory. A directory that does not
/// exist has no profiles and is no error.
#[must_use]
pub fn load_dir(dir: &Path) -> (Vec<SurfaceProfile>, Vec<ProfileError>) {
    let mut profiles: Vec<SurfaceProfile> = Vec::new();
    let mut errors = Vec::new();
    let origin = dir.display().to_string();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (profiles, errors),
        Err(e) => {
            errors.push(error(&origin, e.to_string()));
            return (profiles, errors);
        }
    };
    let mut paths: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && matches!(
                    p.extension().and_then(|x| x.to_str()),
                    Some("toml" | "json")
                )
        })
        .collect();
    paths.sort();
    let mut names = BTreeSet::new();
    for path in paths {
        let origin = path.display().to_string();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                errors.push(error(&origin, e.to_string()));
                continue;
            }
        };
        let parsed = if path.extension().is_some_and(|x| x == "json") {
            SurfaceProfile::from_json_str(&text)
        } else {
            SurfaceProfile::from_toml_str(&text)
        };
        match parsed {
            Ok(p) if !names.insert(p.name.clone()) => errors.push(error(
                &origin,
                format!(
                    "another file of this directory already defines profile {:?}",
                    p.name
                ),
            )),
            Ok(p) => profiles.push(p),
            Err(e) => errors.push(error(&origin, e.message)),
        }
    }
    (profiles, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: &str = r#"
name = "my-controller"
vendor = "Acme"
model = "Faders 2"

[match]
midi_card = "Faders2"

[curves]
fader = [{ raw = 1, mdb = -60000 }, { raw = 127, mdb = 0 }]

[[control]]
id = "f1"
kind = "fader"
midi = { kind = "control_change", channel = 1, number = 7 }

[[control]]
id = "b1"
kind = "button"
midi = { kind = "note", channel = 1, number = 36 }

[[binding]]
control = "f1"
action = "gain"
curve = "fader"

[[binding]]
control = "b1"
action = "mute"
mode = "momentary"
"#;

    #[test]
    fn toml_and_json_round_trip() {
        let p = SurfaceProfile::from_toml_str(SMALL).unwrap();
        assert_eq!(p.controls.len(), 2);
        assert_eq!(
            p.controls[0].midi,
            Some(MidiAddress::new(MidiKind::ControlChange, 1, 7))
        );
        assert_eq!(p.bindings[1].mode, ButtonMode::Momentary);
        let t = p.to_toml_string().unwrap();
        assert_eq!(SurfaceProfile::from_toml_str(&t).unwrap(), p);
        let j = p.to_json_string().unwrap();
        assert_eq!(SurfaceProfile::from_json_str(&j).unwrap(), p);
        assert!(p.matches_midi_card("Faders2", "Acme Faders 2"));
        assert!(p.matches_midi_card("x", "acme faders2 usb"));
        assert!(!p.matches_midi_card("CQ", "CQ-12T"));
        assert!(!p.matches_usb(1, 2));
        assert!(matches!(p.source("Faders2"), SurfaceSource::Midi { .. }));
    }

    #[test]
    fn errors_are_clear() {
        let e = SurfaceProfile::from_toml_str(&SMALL.replace("vendor", "vendr")).unwrap_err();
        assert!(e.message.contains("vendr"), "{e}");
        let e = SurfaceProfile::from_toml_str(&SMALL.replace("curve = \"fader\"", "curve = \"x\""))
            .unwrap_err();
        assert!(e.message.contains("unknown curve"), "{e}");
        let e = SurfaceProfile::from_toml_str(&SMALL.replace("number = 7", "number = 700"))
            .unwrap_err();
        assert!(e.message.contains("0..=127"), "{e}");
        let e = SurfaceProfile::from_toml_str(
            &SMALL.replace("{ raw = 127, mdb = 0 }", "{ raw = 1, mdb = 0 }"),
        )
        .unwrap_err();
        assert!(e.message.contains("ascend"), "{e}");
        let e =
            SurfaceProfile::from_toml_str(&SMALL.replace("action = \"mute\"", "action = \"gain\""))
                .unwrap_err();
        assert!(e.message.contains("cannot be bound"), "{e}");
        let e = SurfaceProfile::from_json_str("{\"name\": \"x\"}").unwrap_err();
        assert_eq!(e.origin, "json");
        assert!(e.to_string().starts_with("json: "));
    }

    #[test]
    fn unknown_keys_anywhere_in_a_profile_file_are_refused() {
        for (at, with, field) in [
            // A control, its MIDI address, a curve point, [match], a binding.
            (
                "kind = \"fader\"\nmidi",
                "kind = \"fader\"\nmotorised = true\nmidi",
                "motorised",
            ),
            ("number = 7 }", "number = 7, chanel = 2 }", "chanel"),
            (
                "{ raw = 1, mdb = -60000 }",
                "{ raw = 1, mdb = -60000, db = -60 }",
                "db",
            ),
            (
                "midi_card = \"Faders2\"",
                "midi_card = \"Faders2\"\nusb = 1",
                "usb",
            ),
            (
                "mode = \"momentary\"",
                "mode = \"momentary\"\ncurv = \"fader\"",
                "curv",
            ),
        ] {
            let text = SMALL.replacen(at, with, 1);
            assert_ne!(text, SMALL, "{at}");
            let e = SurfaceProfile::from_toml_str(&text).unwrap_err();
            assert!(e.message.contains(&format!("`{field}`")), "{field}: {e}");
        }
        // The same in JSON.
        let p = SurfaceProfile::from_toml_str(SMALL).unwrap();
        let mut j: serde_json::Value = serde_json::from_str(&p.to_json_string().unwrap()).unwrap();
        j["control"][0]["motorised"] = serde_json::Value::Bool(true);
        let e = SurfaceProfile::from_json_str(&j.to_string()).unwrap_err();
        assert!(e.message.contains("`motorised`"), "{e}");
        // From a directory, the error names the file and the key.
        let dir = std::env::temp_dir().join(format!("tessera-strict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("typo.toml"),
            SMALL.replacen("kind = \"fader\"", "kind = \"fader\"\nlable = \"Vol\"", 1),
        )
        .unwrap();
        let (profiles, errors) = load_dir(&dir);
        assert!(profiles.is_empty());
        assert_eq!(errors.len(), 1);
        let shown = errors[0].to_string();
        assert!(
            shown.contains("typo.toml") && shown.contains("`lable`"),
            "{shown}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Every key of the contract's control is a key a profile file may
    /// use: a control with every field set survives the strict reading.
    #[test]
    fn the_strict_control_knows_every_contract_field() {
        let full = SurfaceControl {
            id: "f1".into(),
            label: Some("Vol".into()),
            kind: ControlKind::Fader,
            report_id: Some(1),
            offset: Some(8),
            bits: Some(12),
            max: Some(4095),
            inverted: true,
            active_low: true,
            motorized: true,
            code: Some("x".into()),
            midi: Some(MidiAddress::new(MidiKind::Nrpn, 3, 0x2000)),
        };
        let j = serde_json::to_value(&full).unwrap();
        let back: FileControl = serde_json::from_value(j).unwrap();
        assert_eq!(SurfaceControl::from(back), full);
    }

    #[test]
    fn a_profile_suggests_host_volume_but_not_host_specific_actions() {
        // The host volume and mute: a fader and a button, no channels.
        let host = SMALL
            .replace("action = \"gain\"", "action = \"host_volume\"")
            .replace("action = \"mute\"", "action = \"host_mute\"");
        let p = SurfaceProfile::from_toml_str(&host).unwrap();
        assert_eq!(p.bindings[0].action, BindingAction::HostVolume);
        assert_eq!(p.bindings[0].curve.as_deref(), Some("fader"));
        assert_eq!(p.bindings[0].suggested_direction(), None);
        assert_eq!(p.bindings[1].action, BindingAction::HostMute);
        // ... and only on the kind of control they fit.
        let e = SurfaceProfile::from_toml_str(
            &SMALL.replace("action = \"mute\"", "action = \"host_volume\""),
        )
        .unwrap_err();
        assert!(e.message.contains("cannot be bound"), "{e}");
        // The host volume acts on no channels.
        let e = SurfaceProfile::from_toml_str(&host.replace(
            "action = \"host_volume\"",
            "action = \"host_volume\"\ndirection = \"rx\"\nchannels = [1, 2]",
        ))
        .unwrap_err();
        assert!(
            e.message.contains(
                "binding of \"f1\": remove `direction`: a host volume binding acts on the host \
                 volume, not on channels"
            ),
            "{e}"
        );
        let e = SurfaceProfile::from_toml_str(&host.replace(
            "action = \"host_mute\"",
            "action = \"host_mute\"\nchannels = [1]",
        ))
        .unwrap_err();
        assert!(
            e.message.contains(
                "binding of \"b1\": remove `channels`: a host mute binding acts on the host \
                 mute, not on channels"
            ),
            "{e}"
        );
        // Router ports, crosspoints and other surfaces belong to one host.
        for (from, to, suggest) in [
            (
                "action = \"gain\"",
                "port_gain",
                "`gain`, `output_gain` or `host_volume`",
            ),
            (
                "action = \"gain\"",
                "crosspoint_gain",
                "`gain`, `output_gain` or `host_volume`",
            ),
            (
                "action = \"gain\"",
                "follow",
                "`gain`, `output_gain` or `host_volume`",
            ),
            (
                "action = \"mute\"",
                "port_mute",
                "`mute`, `output_mute` or `host_mute`",
            ),
            (
                "action = \"mute\"",
                "crosspoint_mute",
                "`mute`, `output_mute` or `host_mute`",
            ),
        ] {
            let e =
                SurfaceProfile::from_toml_str(&SMALL.replace(from, &format!("action = \"{to}\"")))
                    .unwrap_err();
            assert!(
                e.message.contains(&format!(
                    "`{to}` names one host's router ports, crosspoints or surfaces, which a \
                     profile cannot know; suggest {suggest}"
                )),
                "{e}"
            );
        }
    }

    #[test]
    fn the_shipped_profile_files_load_strictly() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("profiles");
        let (profiles, errors) = load_dir(&dir);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!profiles.is_empty());
    }

    #[test]
    fn load_dir_reads_toml_and_json_and_reports_bad_files() {
        let dir = std::env::temp_dir().join(format!("tessera-profiles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = SurfaceProfile::from_toml_str(SMALL).unwrap();
        let mut other = p.clone();
        other.name = "other".into();
        std::fs::write(dir.join("a.toml"), SMALL).unwrap();
        std::fs::write(dir.join("b.json"), other.to_json_string().unwrap()).unwrap();
        std::fs::write(dir.join("c.toml"), SMALL).unwrap(); // same name as a.toml
        std::fs::write(dir.join("d.toml"), "name = ").unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let (profiles, errors) = load_dir(&dir);
        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["my-controller", "other"]);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].origin.ends_with("c.toml"));
        assert!(errors[1].origin.ends_with("d.toml"));
        std::fs::remove_dir_all(&dir).unwrap();
        let (p, e) = load_dir(&dir);
        assert!(p.is_empty() && e.is_empty(), "a missing directory is empty");
    }

    #[test]
    fn suggested_channels_and_output_actions() {
        let out = SMALL
            .replace(
                "action = \"gain\"\ncurve = \"fader\"",
                "action = \"output_gain\"\ncurve = \"fader\"\nchannels = [1, 2]",
            )
            .replace(
                "action = \"mute\"",
                "action = \"output_mute\"\ndirection = \"rx\"\nchannels = [1, 2]",
            );
        let p = SurfaceProfile::from_toml_str(&out).unwrap();
        assert_eq!(p.bindings[0].action, BindingAction::OutputGain);
        assert_eq!(p.bindings[0].direction, None);
        assert_eq!(
            p.bindings[0].suggested_direction(),
            Some(Direction::Rx),
            "output actions imply RX"
        );
        assert_eq!(p.bindings[1].channels, [1, 2]);
        let again = SurfaceProfile::from_json_str(&p.to_json_string().unwrap()).unwrap();
        assert_eq!(again, p);
        // TX has no output stage.
        let e =
            SurfaceProfile::from_toml_str(&out.replace("direction = \"rx\"", "direction = \"tx\""))
                .unwrap_err();
        assert!(e.message.contains("only RX"), "{e}");
        // A channel-stage suggestion with channels needs a direction.
        let e = SurfaceProfile::from_toml_str(
            &SMALL.replace("action = \"mute\"", "action = \"mute\"\nchannels = [1]"),
        )
        .unwrap_err();
        assert!(e.message.contains("direction"), "{e}");
        let e = SurfaceProfile::from_toml_str(&SMALL.replace(
            "action = \"mute\"",
            "action = \"mute\"\ndirection = \"tx\"\nchannels = [0]",
        ))
        .unwrap_err();
        assert!(e.message.contains("1-based"), "{e}");
        // Kinds still have to fit.
        let e = SurfaceProfile::from_toml_str(
            &SMALL.replace("action = \"mute\"", "action = \"output_gain\""),
        )
        .unwrap_err();
        assert!(e.message.contains("cannot be bound"), "{e}");
    }
}
