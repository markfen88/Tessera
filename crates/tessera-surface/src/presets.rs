//! Built-in surface profiles, shipped with tessera.
//!
//! Each is a profile file under `profiles/` (the same format users write,
//! see [`crate::profile`]), embedded at build time.

use tessera_control_types::{CurvePoint, SurfaceControl};

use crate::profile::SurfaceProfile;

/// `profiles/allen-heath-cq.toml`.
pub const ALLEN_HEATH_CQ_TOML: &str = include_str!("../profiles/allen-heath-cq.toml");

/// Name of the Allen & Heath CQ profile.
pub const ALLEN_HEATH_CQ: &str = "allen-heath-cq";

/// Every built-in profile (tests check that each parses and validates).
#[must_use]
pub fn builtin() -> Vec<SurfaceProfile> {
    [ALLEN_HEATH_CQ_TOML]
        .iter()
        .filter_map(|t| SurfaceProfile::from_toml_str(t).ok())
        .collect()
}

/// Allen & Heath CQ series over USB MIDI: the controls for inputs
/// `Ip1..=Ip<inputs>` (at most 16: level to Main LR, "Ch n", NRPN
/// `0x40 << 7 | n-1`, and mute, NRPN `n-1`), Main LR level and mute, and
/// Out 1..6 level and mute, all on MIDI channel 1, faders `motorized` (the
/// desk shows the values tessera sends back, so it owns no gain) with
/// `max` 16320; and the desk's fader law.
///
/// Source: Allen & Heath CQ MIDI Protocol V1.2 (issue 5) and a community
/// control module for the CQ. Confidence: likely. A thin wrapper over the
/// built-in profile [`ALLEN_HEATH_CQ`].
#[must_use]
pub fn allen_heath_cq(inputs: u8) -> (Vec<SurfaceControl>, Vec<CurvePoint>) {
    let Some(p) = builtin().into_iter().find(|p| p.name == ALLEN_HEATH_CQ) else {
        return (Vec::new(), Vec::new());
    };
    let keep = |id: &str| {
        let Some(rest) = id.strip_prefix("ip") else {
            return true;
        };
        let n = rest.strip_suffix("-mute").unwrap_or(rest);
        n.parse::<u8>().is_ok_and(|n| n <= inputs.min(16))
    };
    let controls = p.controls.iter().filter(|c| keep(&c.id)).cloned().collect();
    (controls, p.curve("fader"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_control_types::{ControlKind, MidiKind};

    #[test]
    fn shipped_profiles_parse_and_validate() {
        let all = builtin();
        assert_eq!(all.len(), 1);
        let cq = &all[0];
        cq.validate().unwrap();
        assert_eq!(cq.controls.len(), 16 * 2 + 2 + 12);
        assert_eq!(cq.bindings.len(), cq.controls.len());
        assert!(cq.matches_midi_card("CQ", "CQ-12T"));
        // Round trip through both formats.
        let again = SurfaceProfile::from_toml_str(&cq.to_toml_string().unwrap()).unwrap();
        assert_eq!(&again, cq);
        let again = SurfaceProfile::from_json_str(&cq.to_json_string().unwrap()).unwrap();
        assert_eq!(&again, cq);
    }

    #[test]
    fn cq_addresses_and_curve() {
        let (controls, curve) = allen_heath_cq(12);
        assert_eq!(controls.len(), 12 * 2 + 2 + 12);
        let by = |id: &str| controls.iter().find(|c| c.id == id).unwrap();
        let num = |id: &str| by(id).midi.unwrap().number;
        assert_eq!(num("ip1"), 0x40 << 7);
        assert_eq!(num("ip12"), (0x40 << 7) | 11);
        assert_eq!(num("ip1-mute"), 0);
        assert_eq!(num("ip12-mute"), 0x0B);
        assert_eq!(num("main-lr"), 0x4F << 7);
        assert_eq!(num("main-lr-mute"), 0x44);
        assert_eq!(num("out1"), (0x4F << 7) | 1);
        assert_eq!(num("out6"), (0x4F << 7) | 6);
        assert_eq!(num("out1-mute"), 0x45);
        assert_eq!(num("out6-mute"), 0x4A);
        assert!(controls.iter().all(|c| {
            let m = c.midi.unwrap();
            m.channel == 1 && m.kind == MidiKind::Nrpn
        }));
        let ip1 = by("ip1");
        assert_eq!(ip1.label.as_deref(), Some("Ch 1"));
        assert!(ip1.motorized && ip1.kind == ControlKind::Fader);
        assert_eq!(ip1.max, Some(16320));
        assert_eq!(by("ip1-mute").kind, ControlKind::Button);
        assert!(!controls.iter().any(|c| c.id == "ip13"));
        assert_eq!(curve.len(), 12);
        assert_eq!((curve[0].raw, curve[0].mdb), (192, -89_000));
        assert_eq!(curve[9].raw, 12544);
        assert_eq!(curve[9].mdb, 0);
        assert_eq!((curve[11].raw, curve[11].mdb), (16320, 10_000));
        assert_eq!(allen_heath_cq(40).0.len(), 16 * 2 + 2 + 12);
        assert_eq!(allen_heath_cq(0).0.len(), 2 + 12);
    }

    /// The owner's CQ-12T mapping, as suggestions: Main L/R → the host
    /// volume and mute (the speaker volume), on the desk's fader law; Ip4 and
    /// Ip5 → the input gain of RX 1-2 and RX 3-4.
    #[test]
    fn cq_suggests_main_as_host_volume_and_two_inputs_as_rx_gain() {
        use tessera_control_types::{BindingAction, Direction};
        let cq = builtin().remove(0);
        let by = |id: &str| cq.bindings.iter().find(|b| b.control == id).unwrap();
        let main = by("main-lr");
        assert_eq!(main.action, BindingAction::HostVolume);
        assert_eq!(main.curve.as_deref(), Some("fader"));
        assert_eq!((main.direction, main.channels.as_slice()), (None, &[][..]));
        assert_eq!(main.suggested_direction(), None);
        let mute = by("main-lr-mute");
        assert_eq!(mute.action, BindingAction::HostMute);
        assert!(mute.direction.is_none() && mute.channels.is_empty());
        for (id, channels) in [("ip4", [1, 2]), ("ip5", [3, 4])] {
            let b = by(id);
            assert_eq!(b.action, BindingAction::Gain, "{id}");
            assert_eq!(b.suggested_direction(), Some(Direction::Rx), "{id}");
            assert_eq!(b.channels, channels, "{id}");
        }
        // The other inputs and the outputs suggest no channels.
        assert!(by("ip1").channels.is_empty() && by("out1").channels.is_empty());
        assert_eq!(by("out1").action, BindingAction::Gain);
    }
}
