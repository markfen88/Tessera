//! The mixer's widgets, rendered from the view model ([`super::model`]):
//! a panel of groups of strips; per strip a scribble-strip label, buttons, a
//! vertical fader with its dB scale and readout, and badges for the surface
//! controls bound to the same target. Every form posts to an existing
//! handler (channel gain, mute, output stage, the amplifier's volume) with
//! `back=/mixer#<strip>`, so it also works without the script.
//!
//! Positions on the scale are SVG attributes (percentages), not inline
//! styles: the content security policy refuses those.

use maud::{Markup, html};

use super::model::{
    Button, ButtonAction, ButtonMode, Fader, FaderAction, Group, Level, Link, Panel, Range, Strip,
};
use crate::appliance::camilla::signed_db;
use crate::pages::surfaces::{control_anchor, surface_anchor};
use crate::pages::{dev_path, fill_class, slider_text};
use crate::ports;

/// Where a form returns to without the script.
fn back(base: &str, strip: &str) -> String {
    format!("{base}#{strip}")
}

/// A whole panel: a card with its groups side by side (stacked on a phone).
#[must_use]
pub fn panel(p: &Panel, csrf: &str) -> Markup {
    let b = p.back.as_str();
    html! {
        section.card.mixer id=(p.id) {
            h2 { (p.title) span.n { "faders apply while they move" } }
            @if p.groups.is_empty() {
                p.muted { "This host has no channels yet." }
            } @else {
                div.desk {
                    @for g in &p.groups { (group(g, b, csrf)) }
                }
            }
        }
    }
}

/// A group of strips; a collapsed one folds (channels not in use).
#[must_use]
pub fn group(g: &Group, back_base: &str, csrf: &str) -> Markup {
    let strips = html! { div.strips { @for s in &g.strips { (strip(s, back_base, csrf)) } } };
    html! {
        @if g.collapsed {
            details.group id=(g.id) {
                summary { (g.title) span.n { (g.strips.len()) } }
                (strips)
            }
        } @else {
            section.group.main[g.master] id=(g.id) aria-label=[(!g.title.is_empty()).then_some(g.title.as_str())] {
                @if !g.title.is_empty() { h3 { (g.title) } }
                (strips)
            }
        }
    }
}

/// One strip. Desktop: buttons, the fader, the readout, the links, then the
/// label at the bottom like a scribble strip. Phone: one row.
#[must_use]
pub fn strip(s: &Strip, back_base: &str, csrf: &str) -> Markup {
    let label_id = format!("{}-label", s.id);
    let about_id = format!("{}-about", s.id);
    let about = (!s.about.is_empty()).then_some(s.about.as_str());
    html! {
        div.strip.master[s.master] id=(s.id) role="group" aria-labelledby=(label_id)
            aria-describedby=[about.map(|_| about_id.as_str())] {
            div.scribble title=[about] {
                bdi.label id=(label_id) { (s.label) }
                span.detail { (s.detail) }
                @if let Some(a) = about { span.sr id=(about_id) { (a) } }
            }
            // A strip without buttons keeps their place, so its fader lines
            // up with its neighbours'.
            @if s.buttons.is_empty() {
                div.buttons.empty aria-hidden="true" {}
            } @else {
                div.buttons {
                    @for b in &s.buttons { (button(s, b, back_base, csrf)) }
                }
            }
            @if let Some(f) = &s.fader { (fader(s, f, back_base, csrf)) }
            @if let Some(p) = &s.problem { p.problem role="status" { (p) } }
            @if s.amp_unmute { (amp_unmute(csrf)) }
            @let links: Vec<&Link> = s.fader.iter().flat_map(|f| f.links.iter())
                .chain(s.buttons.iter().flat_map(|b| b.links.iter())).collect();
            @if !links.is_empty() {
                ul.links aria-label="Linked surface controls" {
                    @for l in links { li { (link(l)) } }
                }
            }
        }
    }
}

/// A linked surface control: the control's label (`Ch 5 · motor`), the
/// surface in front only when several are enabled, whether it is
/// connected. The title and the accessible name always say the surface.
#[must_use]
pub fn link(l: &Link) -> Markup {
    let title = format!(
        "{} on {}{}{}",
        l.label,
        l.surface,
        if l.motorized { ", motorized" } else { "" },
        if l.connected { "" } else { " (not connected)" }
    );
    html! {
        a.link.off[!l.connected] href=(control_anchor(&l.surface, &l.control)) title=(title) {
            span.dot {}
            @if l.surface_shown { bdi { (unbroken(&l.surface)) } " \u{b7} " }
            bdi { (l.label) }
            @if !l.surface_shown { span.sr { " on " (l.surface) } }
            @if l.motorized { " \u{b7} motor" }
            @if !l.connected { span.sr { " (not connected)" } }
        }
    }
}

/// A name whose hyphens never break a line (`allen-heath-cq` stays one
/// word in a narrow strip): non-breaking hyphens, for display only.
fn unbroken(name: &str) -> String {
    name.replace('-', "\u{2011}")
}

/// Where the thumb sits, dB, for a level on a range.
fn position(level: Level, r: Range) -> f64 {
    match level {
        Level::Db(v) if r.off_at_bottom => v.clamp(r.min + r.step, r.max),
        Level::Db(v) => v.clamp(r.min, r.max),
        Level::Off | Level::Floor => r.min,
    }
}

/// The readout of a level.
fn readout(action: FaderAction, level: Level) -> String {
    match (action, level) {
        (FaderAction::AmpVolume | FaderAction::HostVolume, Level::Db(v)) => {
            format!("{} dB", signed_db(v))
        }
        (FaderAction::AmpVolume | FaderAction::HostVolume, _) => "\u{2212}\u{221e} dB".into(),
        (_, Level::Db(v)) => {
            #[allow(clippy::cast_possible_truncation)]
            let m = (v * 1000.0).round() as i32;
            slider_text(tessera_control_types::Gain::MilliDb(m))
        }
        (_, Level::Off | Level::Floor) => "off".into(),
    }
}

/// The marks of a scale: value and text, inside the range, not crowded.
fn marks(r: Range) -> Vec<(f64, String)> {
    let span = r.max - r.min;
    if span <= 0.0 {
        return Vec::new();
    }
    let mut out: Vec<(f64, String)> = Vec::new();
    let mut taken: Vec<f64> = Vec::new();
    if r.off_at_bottom {
        out.push((r.min, "off".into()));
        taken.push(r.min);
    }
    let mut candidates = vec![r.max];
    candidates.extend([10.0, 6.0, 0.0, -10.0, -20.0, -30.0, -40.0, -60.0]);
    if !r.off_at_bottom {
        candidates.push(r.min);
    }
    for v in candidates {
        if v > r.max || v < r.min {
            continue;
        }
        if taken.iter().any(|t| ((t - v) / span).abs() < 0.07) {
            continue;
        }
        taken.push(v);
        let text = if v > 0.0 {
            format!("+{v:.0}")
        } else if v < 0.0 {
            format!("\u{2212}{:.0}", -v)
        } else {
            "0".into()
        };
        out.push((v, text));
    }
    out.sort_by(|a, b| b.0.total_cmp(&a.0));
    out
}

/// The dB scale beside a vertical fader: ticks and numbers at the values'
/// heights (an SVG; positions are attributes, not styles).
fn scale(r: Range) -> Markup {
    let span = r.max - r.min;
    html! {
        svg.scale aria-hidden="true" focusable="false" {
            @for (v, text) in marks(r) {
                @let y = format!("{:.1}%", 100.0 * (r.max - v) / span);
                line x1="70%" x2="100%" y1=(y) y2=(y) {}
                text x="62%" y=(y) { (text) }
            }
        }
    }
}

/// Where a fader posts and how the script treats it, with the hidden
/// fields naming its target.
fn fader_post(f: &Fader) -> (String, &'static str, Vec<(&'static str, String)>) {
    match (f.action, &f.target) {
        (FaderAction::AmpVolume, _) => ("/speaker/volume".into(), "volume", Vec::new()),
        (FaderAction::HostVolume, _) => ("/host/volume".into(), "volume", Vec::new()),
        (FaderAction::PortGain, _) => (
            "/levels/port/gain".into(),
            "fader",
            f.ports.iter().map(|p| ("port", ports::encode(p))).collect(),
        ),
        (FaderAction::CrosspointGain, _) => (
            "/levels/crosspoint/gain".into(),
            "fader",
            f.crosspoints
                .iter()
                .map(|k| ("xp", ports::encode_xp(k)))
                .collect(),
        ),
        (action, Some(t)) => (
            dev_path(
                &t.device,
                &format!("gain/{}/{}", t.dir_word(), t.channel_list()),
            ),
            "fader",
            if action == FaderAction::OutputGain {
                vec![("stage", "output".to_owned())]
            } else {
                Vec::new()
            },
        ),
        (_, None) => ("/mixer".into(), "fader", Vec::new()),
    }
}

/// A fader: a form that posts to the handler of its action, with its
/// scale, readout and owner.
#[must_use]
pub fn fader(s: &Strip, f: &Fader, back_base: &str, csrf: &str) -> Markup {
    let r = f.range;
    let v = position(f.level, r);
    let text = readout(f.action, f.level);
    let input_id = format!("{}-fader", s.id);
    let owned_id = format!("{}-owner", s.id);
    let what = match f.action {
        FaderAction::Gain | FaderAction::PortGain => "Level",
        FaderAction::OutputGain => "Output level",
        FaderAction::AmpVolume | FaderAction::HostVolume => "Volume",
        FaderAction::CrosspointGain => "Gain",
    };
    let (action, kind, hidden) = fader_post(f);
    let usable =
        f.target.is_some() || !matches!(f.action, FaderAction::Gain | FaderAction::OutputGain);
    html! {
        form.fader.locked[f.owner.is_some()] method="post" action=(action) data-live-slider=(kind) {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="back" value=(back(back_base, &s.id));
            @for (k, v) in &hidden { input type="hidden" name=(k) value=(v); }
            label.sr for=(input_id) { (what) " of " (s.label) " (" (s.detail) ")" }
            div.track {
                (scale(r))
                input id=(input_id) type="range" name="db" min=(r.min) max=(r.max) step=(r.step)
                    value=(format!("{v:.1}")) class=(fill_class(v, r.min, r.max))
                    aria-orientation="vertical" aria-valuetext=(text)
                    aria-describedby=[f.owner.as_ref().map(|_| owned_id.as_str())]
                    disabled[f.owner.is_some() || !usable];
            }
            output.readout data-readout for=(input_id) { (text) }
            span.note role="status" aria-live="polite" data-live-local {}
            @if let Some(o) = &f.owner {
                span.owned id=(owned_id) {
                    "Set by the " a href=(surface_anchor(o)) { bdi { (o) } " fader" }
                }
            } @else if usable {
                button.btn.sm.nojs type="submit" { "Set" }
            }
        }
    }
}

/// Where a button posts, with the hidden fields naming its target.
fn button_post(b: &Button) -> Option<(String, Vec<(&'static str, String)>)> {
    Some(match b.action {
        ButtonAction::Mute | ButtonAction::OutputMute => {
            let t = b.target.as_ref()?;
            let mut v = vec![
                ("direction", t.dir_word().to_owned()),
                ("channel", t.channel_list()),
            ];
            if b.action == ButtonAction::OutputMute {
                v.insert(0, ("stage", "output".to_owned()));
            }
            (dev_path(&t.device, "mute"), v)
        }
        ButtonAction::HostMute => ("/host/mute".into(), Vec::new()),
        ButtonAction::PortMute => (
            "/levels/port/mute".into(),
            b.ports.iter().map(|p| ("port", ports::encode(p))).collect(),
        ),
        ButtonAction::CrosspointMute => (
            "/levels/crosspoint/mute".into(),
            b.crosspoints
                .iter()
                .map(|k| ("xp", ports::encode_xp(k)))
                .collect(),
        ),
    })
}

/// A button: a mute of the strip's target, toggled or held (momentary:
/// the script posts on press and again on release).
#[must_use]
pub fn button(s: &Strip, b: &Button, back_base: &str, csrf: &str) -> Markup {
    let pressed = if b.on { "true" } else { "false" };
    let caption = if b.on {
        format!("{}d", b.label)
    } else {
        b.label.clone()
    };
    let what = match b.action {
        ButtonAction::OutputMute => format!("{} the output of {} ({})", b.label, s.label, s.detail),
        _ => format!("{} {} ({})", b.label, s.label, s.detail),
    };
    let Some((action, hidden)) = button_post(b) else {
        return html! {};
    };
    html! {
        div.press {
            form.inline method="post" action=(action) data-live-post
                data-momentary=[(b.mode == ButtonMode::Momentary).then_some(if b.on { "false" } else { "true" })] {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="back" value=(back(back_base, &s.id));
                @for (k, v) in &hidden { input type="hidden" name=(k) value=(v); }
                input type="hidden" name="muted" value=(if b.on { "false" } else { "true" });
                button.mute.on[b.on].mixed[b.mixed] type="submit" aria-pressed=(pressed) aria-label=(what) {
                    (caption)
                    @if b.mixed { span.sr { " (some channels)" } }
                }
            }
            span.note role="status" aria-live="polite" data-live-local {}
        }
    }
}

/// The amplifier's Unmute, while its playback switch is off: a plain form
/// (its handler answers with a redirect only).
#[must_use]
pub fn amp_unmute(csrf: &str) -> Markup {
    html! {
        form.inline method="post" action="/speaker/volume/unmute" {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="back" value="/mixer#strip-master";
            button.btn.sm.primary type="submit" { "Unmute the amplifier" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gain_scale_has_the_desk_marks_and_off() {
        let m: Vec<String> = marks(Range::GAIN).into_iter().map(|(_, t)| t).collect();
        assert_eq!(
            m,
            [
                "+12",
                "0",
                "\u{2212}10",
                "\u{2212}20",
                "\u{2212}30",
                "\u{2212}40",
                "\u{2212}60",
                "off"
            ]
        );
        let amp = Range {
            min: -60.0,
            max: 0.0,
            step: 0.5,
            off_at_bottom: false,
        };
        let m: Vec<String> = marks(amp).into_iter().map(|(_, t)| t).collect();
        assert_eq!(
            m,
            [
                "0",
                "\u{2212}10",
                "\u{2212}20",
                "\u{2212}30",
                "\u{2212}40",
                "\u{2212}60"
            ]
        );
    }

    #[test]
    fn readouts_and_positions() {
        assert_eq!(readout(FaderAction::Gain, Level::Off), "off");
        assert_eq!(
            readout(FaderAction::Gain, Level::Db(-6.0)),
            "\u{2212}6.0 dB"
        );
        assert_eq!(
            readout(FaderAction::AmpVolume, Level::Db(-12.0)),
            "\u{2212}12.0 dB"
        );
        assert!((position(Level::Off, Range::GAIN) - Range::GAIN.min).abs() < f64::EPSILON);
        assert!(
            (position(Level::Db(-200.0), Range::GAIN) - (Range::GAIN.min + 0.5)).abs()
                < f64::EPSILON
        );
    }
}
