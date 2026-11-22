//! Raw HID input reports → [`SurfaceEvent`]s.
//!
//! The layout comes from the `[[surface.control]]` entries (report id, bit
//! offset, bit count), so the decoder knows nothing about any particular
//! device. It keeps the last value of every control and reports only
//! changes: a device that repeats its whole state in every report produces
//! one event per actual movement.
//!
//! Sources other than `hidraw` (evdev keys, ALSA mixer elements) already
//! deliver one control at a time; the daemon builds [`SurfaceEvent`]s for
//! them directly and skips this module.

use crate::config::{ControlKind, SurfaceConfig};

/// One control change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceEvent {
    /// The `control.id`.
    pub control: String,
    pub kind: SurfaceEventKind,
}

/// What changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceEventKind {
    /// A button went down (`true`) or up (`false`).
    Button { pressed: bool },
    /// A fader moved to `value` of `0..=max` (0 is the bottom of travel,
    /// after `inverted` was applied).
    Fader { value: u32, max: u32 },
    /// An encoder (or a volume up/down key) moved by `delta` steps.
    Encoder { delta: i32 },
}

#[derive(Debug)]
struct Field {
    control: String,
    kind: ControlKind,
    report_id: Option<u8>,
    offset: u32,
    bits: u8,
    max: u32,
    inverted: bool,
    active_low: bool,
    last: Option<u32>,
}

impl Field {
    /// The value a control starts from: a button is released (its bit reads
    /// `active_low`), other controls have no value yet.
    fn baseline(&self) -> Option<u32> {
        (self.kind == ControlKind::Button).then_some(u32::from(self.active_low))
    }
}

/// `bits` bits (1..=32) at bit `offset` of `data`, little-endian (HID
/// order). `None` if the report is too short.
pub(crate) fn extract(data: &[u8], offset: u32, bits: u8) -> Option<u32> {
    let end = u64::from(offset) + u64::from(bits);
    if bits == 0 || bits > 32 || end > (data.len() as u64) * 8 {
        return None;
    }
    let mut v = 0u64;
    for i in 0..u32::from(bits) {
        let pos = offset + i;
        let byte = *data.get(usize::try_from(pos / 8).ok()?)?;
        v |= u64::from((byte >> (pos % 8)) & 1) << i;
    }
    u32::try_from(v).ok()
}

/// Sign-extends the low `bits` bits of `raw`.
fn signed(raw: u32, bits: u8) -> i32 {
    let shift = 32 - u32::from(bits.clamp(1, 32));
    ((raw << shift) as i32) >> shift
}

/// Decoder for one `hidraw` surface.
#[derive(Debug)]
pub struct Decoder {
    fields: Vec<Field>,
    numbered: bool,
}

impl Decoder {
    /// A decoder for the controls of `cfg` that have a report layout
    /// (controls without `offset` are ignored). Expects a validated config.
    /// Buttons start released.
    #[must_use]
    pub fn new(cfg: &SurfaceConfig) -> Self {
        let fields: Vec<Field> = cfg
            .controls
            .iter()
            .filter_map(|c| {
                let mut f = Field {
                    control: c.id.clone(),
                    kind: c.kind,
                    report_id: c.report_id,
                    offset: c.offset?,
                    bits: c.field_bits(),
                    max: c.max.unwrap_or_else(|| c.field_max()),
                    inverted: c.inverted,
                    active_low: c.active_low,
                    last: None,
                };
                f.last = f.baseline();
                Some(f)
            })
            .collect();
        let numbered = fields.iter().any(|f| f.report_id.is_some());
        Self { fields, numbered }
    }

    /// Forgets every control's last value: buttons count as released again,
    /// faders report their next position. Call after the device was
    /// reopened (unplug, replug), then [`prime`](Self::prime) it if the
    /// device's current state can be read.
    pub fn reset(&mut self) {
        for f in &mut self.fields {
            f.last = f.baseline();
        }
    }

    /// The report id and data of `report`; `None` for an empty numbered
    /// report.
    fn split<'r>(&self, report: &'r [u8]) -> Option<(Option<u8>, &'r [u8])> {
        if self.numbered {
            report.split_first().map(|(id, rest)| (Some(*id), rest))
        } else {
            Some((None, report))
        }
    }

    /// Takes `report`, the device's current state as `HIDIOCGINPUT` returns it
    /// on open, as the starting point. That buffer always starts with the
    /// report number, 0 for a device without numbered reports (unlike a
    /// `read()`, which has no leading byte then). Buttons held in it are
    /// not presses, faders report their position, encoder fields are
    /// ignored (a state read is not a movement). Without a prime, buttons start
    /// released, so on a device that reports only changes the first press
    /// counts.
    pub fn prime(&mut self, report: &[u8], out: &mut Vec<SurfaceEvent>) {
        let Some((&number, data)) = report.split_first() else {
            return;
        };
        let id = self.numbered.then_some(number);
        // The same report in `read()` layout, for `feed`.
        let as_read = if self.numbered { report } else { data };
        for f in self.fields.iter_mut().filter(|f| f.report_id == id) {
            if f.kind == ControlKind::Button {
                if let Some(raw) = extract(data, f.offset, f.bits) {
                    f.last = Some(raw);
                }
            }
        }
        let mut events = Vec::new();
        self.feed(as_read, &mut events);
        let moved = |e: &SurfaceEvent| !matches!(e.kind, SurfaceEventKind::Encoder { .. });
        out.extend(events.into_iter().filter(moved));
    }

    /// Decodes one input report as read from `/dev/hidraw*` (with its report
    /// id byte first when the device numbers its reports), appending the
    /// changes to `out`.
    ///
    /// Buttons report every edge from their baseline (released, or what
    /// [`prime`](Self::prime) read); encoders report any non-zero step
    /// (their fields are relative); faders report their first position and
    /// every change after (the first position is the baseline the mapper
    /// applies to the gain at once for a fader without a motor). Fields
    /// beyond the end of a short report are skipped.
    pub fn feed(&mut self, report: &[u8], out: &mut Vec<SurfaceEvent>) {
        let Some((id, data)) = self.split(report) else {
            return;
        };
        for f in self.fields.iter_mut().filter(|f| f.report_id == id) {
            let Some(raw) = extract(data, f.offset, f.bits) else {
                continue;
            };
            let kind = match f.kind {
                ControlKind::Button => {
                    let prev = f.last.replace(raw);
                    if prev == Some(raw) {
                        continue;
                    }
                    SurfaceEventKind::Button {
                        pressed: (raw != 0) != f.active_low,
                    }
                }
                ControlKind::Fader => {
                    // Compared after clamping: readings above `max` are all
                    // the top of travel, not movements.
                    let v = raw.min(f.max);
                    if f.last.replace(v) == Some(v) {
                        continue;
                    }
                    let value = if f.inverted { f.max - v } else { v };
                    SurfaceEventKind::Fader { value, max: f.max }
                }
                ControlKind::Encoder => {
                    // A relative field: the report carries the signed step
                    // count since the previous report (not a running total).
                    let delta = signed(raw, f.bits);
                    if delta == 0 {
                        continue;
                    }
                    let delta = if f.inverted {
                        delta.saturating_neg()
                    } else {
                        delta
                    };
                    SurfaceEventKind::Encoder { delta }
                }
            };
            out.push(SurfaceEvent {
                control: f.control.clone(),
                kind,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ControlConfig, SourceConfig};

    fn control(id: &str, kind: ControlKind, offset: u32) -> ControlConfig {
        ControlConfig {
            id: id.into(),
            kind,
            report_id: Some(1),
            offset: Some(offset),
            bits: None,
            max: None,
            inverted: false,
            active_low: false,
            motorized: false,
            code: None,
            midi: None,
        }
    }

    fn cfg(controls: Vec<ControlConfig>) -> SurfaceConfig {
        SurfaceConfig {
            name: "t".into(),
            enabled: true,
            source: SourceConfig::Hidraw {
                vendor_id: 1,
                product_id: 2,
                interface: None,
            },
            controls,
            bindings: vec![],
        }
    }

    fn feed(d: &mut Decoder, r: &[u8]) -> Vec<SurfaceEvent> {
        let mut out = vec![];
        d.feed(r, &mut out);
        out
    }

    #[test]
    fn extract_unaligned_little_endian() {
        // 12 bits at offset 4: low nibble of byte 0 is skipped.
        assert_eq!(extract(&[0xA0, 0xBC], 4, 12), Some(0xBCA));
        assert_eq!(extract(&[0xFF; 4], 0, 32), Some(u32::MAX));
        assert_eq!(extract(&[0x01], 1, 8), None, "past the end");
        assert_eq!(extract(&[0x00], u32::MAX, 1), None, "no overflow");
        assert_eq!(signed(0xFFE, 12), -2);
        assert_eq!(signed(3, 8), 3);
    }

    #[test]
    fn first_press_counts_and_edges_only() {
        let mut d = Decoder::new(&cfg(vec![control("m", ControlKind::Button, 2)]));
        // A device that reports only changes: its first report is the press.
        let ev = feed(&mut d, &[1, 0b100]);
        assert_eq!(ev[0].kind, SurfaceEventKind::Button { pressed: true });
        assert!(feed(&mut d, &[1, 0b100]).is_empty(), "no change");
        let ev = feed(&mut d, &[1, 0]);
        assert_eq!(ev[0].kind, SurfaceEventKind::Button { pressed: false });
        feed(&mut d, &[1, 0b100]);
        d.reset();
        let ev = feed(&mut d, &[1, 0b100]);
        assert_eq!(
            ev[0].kind,
            SurfaceEventKind::Button { pressed: true },
            "released after reset"
        );
    }

    #[test]
    fn prime_takes_held_buttons_and_reports_faders() {
        let mut f = control("f", ControlKind::Fader, 8);
        f.max = Some(255);
        let controls = vec![
            control("m", ControlKind::Button, 2),
            f,
            control("e", ControlKind::Encoder, 16),
        ];
        let mut d = Decoder::new(&cfg(controls));
        let mut out = vec![];
        d.prime(&[1, 0b100, 40, 3], &mut out);
        assert_eq!(
            out.len(),
            1,
            "fader only: held button is no press, encoder no step"
        );
        assert_eq!(
            out[0].kind,
            SurfaceEventKind::Fader {
                value: 40,
                max: 255
            }
        );
        let ev = feed(&mut d, &[1, 0, 40, 0]);
        assert_eq!(ev[0].kind, SurfaceEventKind::Button { pressed: false });
    }

    #[test]
    fn active_low_button() {
        let mut b = control("m", ControlKind::Button, 0);
        b.active_low = true;
        let mut d = Decoder::new(&cfg(vec![b]));
        feed(&mut d, &[1, 1]);
        let ev = feed(&mut d, &[1, 0]);
        assert_eq!(ev[0].kind, SurfaceEventKind::Button { pressed: true });
    }

    #[test]
    fn fader_reports_baseline_and_changes_inverted_and_clamped() {
        let mut f = control("f", ControlKind::Fader, 8);
        f.max = Some(100);
        f.inverted = true;
        let mut d = Decoder::new(&cfg(vec![f]));
        let ev = feed(&mut d, &[1, 0, 100]);
        assert_eq!(ev[0].kind, SurfaceEventKind::Fader { value: 0, max: 100 });
        assert!(feed(&mut d, &[1, 0, 100]).is_empty());
        assert!(
            feed(&mut d, &[1, 0, 250]).is_empty(),
            "above max is still the top"
        );
        let ev = feed(&mut d, &[1, 0, 25]);
        assert_eq!(
            ev[0].kind,
            SurfaceEventKind::Fader {
                value: 75,
                max: 100
            }
        );
    }

    #[test]
    fn twelve_bit_fader() {
        let mut f = control("f", ControlKind::Fader, 4);
        f.bits = Some(12);
        let mut d = Decoder::new(&cfg(vec![f]));
        let ev = feed(&mut d, &[1, 0x40, 0x23]);
        assert_eq!(
            ev[0].kind,
            SurfaceEventKind::Fader {
                value: 0x234,
                max: 4095
            }
        );
    }

    #[test]
    fn encoder_signed_deltas_and_inversion() {
        let mut d = Decoder::new(&cfg(vec![control("e", ControlKind::Encoder, 0)]));
        assert_eq!(
            feed(&mut d, &[1, 0xFE])[0].kind,
            SurfaceEventKind::Encoder { delta: -2 }
        );
        assert!(feed(&mut d, &[1, 0]).is_empty());
        assert_eq!(
            feed(&mut d, &[1, 3])[0].kind,
            SurfaceEventKind::Encoder { delta: 3 }
        );
        let mut e = control("e", ControlKind::Encoder, 0);
        e.inverted = true;
        let mut d = Decoder::new(&cfg(vec![e]));
        assert_eq!(
            feed(&mut d, &[1, 3])[0].kind,
            SurfaceEventKind::Encoder { delta: -3 }
        );
    }

    #[test]
    fn other_report_ids_and_short_reports_ignored() {
        let mut d = Decoder::new(&cfg(vec![control("f", ControlKind::Fader, 24)]));
        assert!(feed(&mut d, &[2, 0, 0, 0, 9]).is_empty(), "other id");
        assert!(feed(&mut d, &[1, 0]).is_empty(), "short");
        assert!(feed(&mut d, &[]).is_empty(), "empty");
    }

    #[test]
    fn prime_unnumbered_skips_the_report_number_byte() {
        let mut b = control("m", ControlKind::Button, 0);
        b.report_id = None;
        let mut f = control("f", ControlKind::Fader, 8);
        f.report_id = None;
        let mut d = Decoder::new(&cfg(vec![b, f]));
        let mut out = vec![];
        // HIDIOCGINPUT layout: report number 0, then button held, fader at 7.
        d.prime(&[0, 1, 7], &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, SurfaceEventKind::Fader { value: 7, max: 255 });
        // read() layout from then on: the release is seen.
        let ev = feed(&mut d, &[0, 7]);
        assert_eq!(ev[0].kind, SurfaceEventKind::Button { pressed: false });
    }

    #[test]
    fn unnumbered_reports() {
        let mut b = control("m", ControlKind::Button, 7);
        b.report_id = None;
        let mut d = Decoder::new(&cfg(vec![b]));
        feed(&mut d, &[0]);
        assert_eq!(
            feed(&mut d, &[0x80])[0].kind,
            SurfaceEventKind::Button { pressed: true }
        );
    }
}
