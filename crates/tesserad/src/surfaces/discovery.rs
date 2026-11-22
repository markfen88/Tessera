//! What the host has: USB HID interfaces (`/dev/hidraw*`) and sound cards.
//!
//! Everything is read from a filesystem root (`/` in production, a fixture
//! directory under `--lab-surface-root`):
//!
//! | Read | For |
//! |---|---|
//! | `sys/class/hidraw/<node>/device/uevent` | `HID_ID=<bus>:<vendor>:<product>` (hex), `HID_NAME`, `HID_PHYS` |
//! | `sys/class/hidraw/<node>/device/../bInterfaceNumber` | the USB interface number (hex); `HID_PHYS`'s `…/inputN` if absent |
//! | `sys/class/hidraw/<node>/device/report_descriptor` | whether the device numbers its reports (learning needs it) |
//! | `proc/asound/cards` | each card's index, id and name |
//! | `proc/asound/card<N>/usbid` | a USB card's `vendor:product` |
//! | `proc/asound/card<N>/stream0` | a USB card's channel counts and rates |
//! | `dev/snd/midiC<N>D<d>` | the card's rawmidi ports (MIDI surfaces) |
//!
//! The parsers are pure functions over text, tested on fixtures. Nothing
//! here opens a device node.

use std::path::{Path, PathBuf};

use tessera_control_types::{
    BridgeStatus, DetectedDevice, DetectedKind, SurfaceSource, SurfaceSpec,
};

/// One `hidraw` node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidNode {
    /// [`DetectedDevice::id`]: `hid:vvvv:pppp:ii` (`hid:vvvv:pppp` without
    /// an interface number), with `@hidrawN` added for a second identical
    /// device.
    pub id: String,
    /// `hidrawN`.
    pub name: String,
    /// The node to open (`<root>/dev/hidrawN`).
    pub node: PathBuf,
    /// HID bus (`0003` USB, `0005` Bluetooth).
    pub bus: u16,
    /// USB (or Bluetooth) vendor id.
    pub vendor_id: u16,
    /// Product id.
    pub product_id: u16,
    /// USB interface number, when known.
    pub interface: Option<u8>,
    /// `HID_NAME`.
    pub product: String,
    /// The report descriptor declares report ids (`None`: unreadable).
    pub numbered: Option<bool>,
}

impl HidNode {
    /// Does a `hidraw` source with these ids select this node?
    #[must_use]
    pub fn matches(&self, vendor_id: u16, product_id: u16, interface: Option<u8>) -> bool {
        self.vendor_id == vendor_id
            && self.product_id == product_id
            && interface.is_none_or(|i| self.interface == Some(i))
    }
}

/// One sound card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    /// ALSA card index.
    pub index: u32,
    /// Card id (`SC3`), what `hw:CARD=<id>` names.
    pub id: String,
    /// Short name (`fifine SC3`).
    pub name: String,
    /// `vendor:product` of a USB card.
    pub usb: Option<(u16, u16)>,
    /// Channel counts and rates from `stream0` (USB cards), when readable.
    pub stream: Option<StreamInfo>,
}

/// What a USB card's `stream0` says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamInfo {
    /// Most capture channels of any altset (`None`: no capture section).
    pub capture_channels: Option<u16>,
    /// Most playback channels of any altset.
    pub playback_channels: Option<u16>,
    /// Every rate listed, ascending (range ends for continuous rates).
    pub rates: Vec<u32>,
}

/// One rawmidi port (`/dev/snd/midiC<card>D<device>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiPort {
    /// [`DetectedDevice::id`]: `midi:<card id>:<device>`.
    pub id: String,
    /// The card's id (`CQ`).
    pub card: String,
    /// The card's short name.
    pub name: String,
    /// Rawmidi device number on the card.
    pub device: u8,
    /// The node to open (`<root>/dev/snd/midiC<N>D<d>`).
    pub node: PathBuf,
    /// `midiC<N>D<d>`.
    pub node_name: String,
    /// `vendor:product` of a USB card.
    pub usb: Option<(u16, u16)>,
}

impl MidiPort {
    /// Does a MIDI source with this card id and device select this port?
    #[must_use]
    pub fn matches(&self, card: &str, device: u8) -> bool {
        self.card == card && self.device == device
    }
}

/// What a scan found, in node / card order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    /// `hidraw` nodes, by node number.
    pub hid: Vec<HidNode>,
    /// Sound cards, by index.
    pub cards: Vec<Card>,
    /// Rawmidi ports, by card index and device.
    pub midi: Vec<MidiPort>,
}

impl Scan {
    /// The MIDI port a source selects.
    #[must_use]
    pub fn find_midi(&self, card: &str, device: u8) -> Option<&MidiPort> {
        self.midi.iter().find(|m| m.matches(card, device))
    }

    /// The MIDI port with this detected id.
    #[must_use]
    pub fn midi_by_id(&self, id: &str) -> Option<&MidiPort> {
        self.midi.iter().find(|m| m.id == id)
    }

    /// The first HID node a source selects.
    #[must_use]
    pub fn find_hid(
        &self,
        vendor_id: u16,
        product_id: u16,
        interface: Option<u8>,
    ) -> Option<&HidNode> {
        self.hid
            .iter()
            .find(|h| h.matches(vendor_id, product_id, interface))
    }

    /// The HID node with this detected id.
    #[must_use]
    pub fn hid_by_id(&self, id: &str) -> Option<&HidNode> {
        self.hid.iter().find(|h| h.id == id)
    }
}

/// The fields of a HID device's `uevent` that matter here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidUevent {
    /// HID bus.
    pub bus: u16,
    /// Vendor id.
    pub vendor_id: u16,
    /// Product id.
    pub product_id: u16,
    /// `HID_NAME`.
    pub name: String,
    /// Interface number from `HID_PHYS` (`usb-…-1/input3` → 3).
    pub phys_interface: Option<u8>,
}

/// Parse a HID device's `uevent` (`KEY=value` lines). `None` without a
/// well-formed `HID_ID`, or for ids that do not fit 16 bits.
#[must_use]
pub fn parse_hid_uevent(text: &str) -> Option<HidUevent> {
    let mut id = None;
    let mut name = String::new();
    let mut phys_interface = None;
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "HID_ID" => {
                let mut it = v.trim().split(':');
                let bus = u32::from_str_radix(it.next()?, 16).ok()?;
                let vendor = u32::from_str_radix(it.next()?, 16).ok()?;
                let product = u32::from_str_radix(it.next()?, 16).ok()?;
                id = Some((
                    u16::try_from(bus).ok()?,
                    u16::try_from(vendor).ok()?,
                    u16::try_from(product).ok()?,
                ));
            }
            "HID_NAME" => v.trim().clone_into(&mut name),
            "HID_PHYS" => {
                phys_interface = v
                    .trim()
                    .rsplit_once("/input")
                    .and_then(|(_, n)| n.parse::<u8>().ok());
            }
            _ => {}
        }
    }
    let (bus, vendor_id, product_id) = id?;
    Some(HidUevent {
        bus,
        vendor_id,
        product_id,
        name,
        phys_interface,
    })
}

/// Parse `bInterfaceNumber` (two hex digits).
#[must_use]
pub fn parse_interface_number(text: &str) -> Option<u8> {
    u8::from_str_radix(text.trim(), 16).ok()
}

/// Does a HID report descriptor declare report ids (a global `Report ID`
/// item, `0x85`)? Walks the items, so a data byte equal to `0x85` does
/// not count.
#[must_use]
pub fn descriptor_numbered(desc: &[u8]) -> bool {
    let mut i = 0usize;
    while let Some(&prefix) = desc.get(i) {
        if prefix == 0xFE {
            // Long item: size byte, tag byte, data.
            let size = desc.get(i + 1).copied().map_or(0, usize::from);
            i += 3 + size;
            continue;
        }
        if prefix & 0xFC == 0x84 {
            return true;
        }
        i += 1 + [0, 1, 2, 4][usize::from(prefix & 3)];
    }
    false
}

/// Parse `/proc/asound/cards`:
///
/// ```text
///  1 [SC3            ]: USB-Audio - fifine SC3
///                       MV-SILICON fifine SC3 at usb-0000:00:14.0-1, full speed
/// ```
#[must_use]
pub fn parse_cards(text: &str) -> Vec<Card> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        let Some((num, rest)) = t.split_once(' ') else {
            continue;
        };
        let Ok(index) = num.parse::<u32>() else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('[') else {
            continue;
        };
        let Some((id, rest)) = rest.split_once(']') else {
            continue;
        };
        let rest = rest.trim_start().strip_prefix(':').unwrap_or(rest).trim();
        let name = rest
            .split_once(" - ")
            .map_or(rest, |(_, short)| short)
            .trim();
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        out.push(Card {
            index,
            id: id.to_owned(),
            name: name.to_owned(),
            usb: None,
            stream: None,
        });
    }
    out
}

/// Parse a USB card's `stream0`:
///
/// ```text
/// Playback:
///   Status: Stop
///   Interface 1
///     Altset 1
///     Format: S24_3LE
///     Channels: 16
///     Rates: 44100, 48000
/// Capture:
///   ...
///     Rates: 8000 - 48000 (continuous)
/// ```
///
/// `None` when it lists neither direction.
#[must_use]
pub fn parse_stream(text: &str) -> Option<StreamInfo> {
    #[derive(PartialEq)]
    enum Sec {
        None,
        Playback,
        Capture,
    }
    let mut sec = Sec::None;
    let mut info = StreamInfo::default();
    let mut any = false;
    for line in text.lines() {
        let t = line.trim();
        if !line.starts_with(char::is_whitespace) {
            sec = match t {
                "Playback:" => Sec::Playback,
                "Capture:" => Sec::Capture,
                _ => Sec::None,
            };
            if sec != Sec::None {
                any = true;
            }
            continue;
        }
        if sec == Sec::None {
            continue;
        }
        if let Some(v) = t.strip_prefix("Channels:") {
            let Ok(n) = v.trim().parse::<u16>() else {
                continue;
            };
            let slot = if sec == Sec::Playback {
                &mut info.playback_channels
            } else {
                &mut info.capture_channels
            };
            *slot = Some(slot.map_or(n, |o| o.max(n)));
        } else if let Some(v) = t.strip_prefix("Rates:") {
            for r in v
                .split([',', '-', '('])
                .filter_map(|s| s.trim().parse::<u32>().ok())
            {
                if !info.rates.contains(&r) {
                    info.rates.push(r);
                }
            }
        }
    }
    info.rates.sort_unstable();
    any.then_some(info)
}

/// Parse a rawmidi node name, `midiC<card>D<device>`.
#[must_use]
pub fn parse_midi_node(name: &str) -> Option<(u32, u8)> {
    let (c, d) = name.strip_prefix("midiC")?.split_once('D')?;
    Some((c.parse().ok()?, d.parse().ok()?))
}

/// Parse a card's `usbid` (`3142:0c33`).
#[must_use]
pub fn parse_usbid(text: &str) -> Option<(u16, u16)> {
    let (v, p) = text.trim().split_once(':')?;
    Some((
        u16::from_str_radix(v, 16).ok()?,
        u16::from_str_radix(p, 16).ok()?,
    ))
}

/// The number in `hidrawN`, for ordering.
fn node_number(name: &str) -> u32 {
    name.strip_prefix("hidraw")
        .and_then(|n| n.parse().ok())
        .unwrap_or(u32::MAX)
}

/// Read everything under `root` (see the module documentation). Missing
/// files are skipped; a scan never fails.
#[must_use]
pub fn scan(root: &Path) -> Scan {
    let mut hid = Vec::new();
    let class = root.join("sys/class/hidraw");
    if let Ok(entries) = std::fs::read_dir(&class) {
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("hidraw"))
            .collect();
        names.sort_by_key(|n| (node_number(n), n.clone()));
        for name in names {
            let dev = class.join(&name).join("device");
            let Some(ev) = std::fs::read_to_string(dev.join("uevent"))
                .ok()
                .and_then(|t| parse_hid_uevent(&t))
            else {
                continue;
            };
            let interface = std::fs::read_to_string(dev.join("../bInterfaceNumber"))
                .ok()
                .and_then(|t| parse_interface_number(&t))
                .or(ev.phys_interface);
            let numbered = std::fs::read(dev.join("report_descriptor"))
                .ok()
                .map(|d| descriptor_numbered(&d));
            let mut id = match interface {
                Some(i) => format!("hid:{:04x}:{:04x}:{i}", ev.vendor_id, ev.product_id),
                None => format!("hid:{:04x}:{:04x}", ev.vendor_id, ev.product_id),
            };
            if hid.iter().any(|h: &HidNode| h.id == id) {
                id = format!("{id}@{name}");
            }
            hid.push(HidNode {
                id,
                node: root.join("dev").join(&name),
                name,
                bus: ev.bus,
                vendor_id: ev.vendor_id,
                product_id: ev.product_id,
                interface,
                product: ev.name,
                numbered,
            });
        }
    }
    let mut cards = std::fs::read_to_string(root.join("proc/asound/cards"))
        .map(|t| parse_cards(&t))
        .unwrap_or_default();
    for c in &mut cards {
        let dir = root.join(format!("proc/asound/card{}", c.index));
        c.usb = std::fs::read_to_string(dir.join("usbid"))
            .ok()
            .and_then(|t| parse_usbid(&t));
        c.stream = std::fs::read_to_string(dir.join("stream0"))
            .ok()
            .and_then(|t| parse_stream(&t));
    }
    let mut midi = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root.join("dev/snd")) {
        let mut ports: Vec<(u32, u8, String)> = entries
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|n| parse_midi_node(&n).map(|(c, d)| (c, d, n)))
            .collect();
        ports.sort();
        for (index, device, node_name) in ports {
            // A port of a card `/proc/asound/cards` does not list is left
            // out: it has no id to match a source by.
            let Some(c) = cards.iter().find(|c| c.index == index) else {
                continue;
            };
            midi.push(MidiPort {
                id: format!("midi:{}:{device}", c.id),
                card: c.id.clone(),
                name: c.name.clone(),
                device,
                node: root.join("dev/snd").join(&node_name),
                node_name,
                usb: c.usb,
            });
        }
    }
    Scan { hid, cards, midi }
}

/// Does an ALSA device string (`hw:CARD=SC3,DEV=0`, `plughw:1,0`, `hw:SC3`)
/// name this card?
#[must_use]
pub fn pcm_names_card(pcm: &str, card: &Card) -> bool {
    sound_card(card).named_by(pcm)
}

/// A card as the state engine checks bridges against it.
#[must_use]
pub fn sound_card(card: &Card) -> tessera_state::engine::SoundCard {
    let stream = card.stream.as_ref();
    tessera_state::engine::SoundCard {
        index: card.index,
        id: card.id.clone(),
        capture_channels: stream.and_then(|s| s.capture_channels),
        playback_channels: stream.and_then(|s| s.playback_channels),
    }
}

impl Scan {
    /// The sound cards, as the state engine checks bridges against them.
    #[must_use]
    pub fn sound_cards(&self) -> Vec<tessera_state::engine::SoundCard> {
        self.cards.iter().map(sound_card).collect()
    }
}

/// The scan as the contract's [`DetectedDevice`]s, with `used_by` from the
/// configured surfaces and bridges.
#[must_use]
pub fn detected(
    scan: &Scan,
    surfaces: &[SurfaceSpec],
    bridges: &[BridgeStatus],
) -> Vec<DetectedDevice> {
    let mut out = Vec::with_capacity(scan.hid.len() + scan.cards.len());
    for h in &scan.hid {
        let used_by = surfaces
            .iter()
            .find(|s| match &s.source {
                SurfaceSource::Hidraw {
                    vendor_id,
                    product_id,
                    interface,
                } => scan.find_hid(*vendor_id, *product_id, *interface) == Some(h),
                _ => false,
            })
            .map(|s| s.name.clone());
        let mut d = DetectedDevice::new(
            h.id.clone(),
            DetectedKind::Hid,
            h.product.clone(),
            format!("/dev/{}", h.name),
        );
        d.vendor_id = Some(h.vendor_id);
        d.product_id = Some(h.product_id);
        d.interface = h.interface;
        d.used_by = used_by;
        d.source = Some(SurfaceSource::Hidraw {
            vendor_id: h.vendor_id,
            product_id: h.product_id,
            interface: h.interface,
        });
        out.push(d);
    }
    for c in &scan.cards {
        // A card is used by the bridge that names it.
        let used_by = bridges
            .iter()
            .find(|b| pcm_names_card(&b.card, c))
            .map(|b| b.name.clone());
        let mut d = DetectedDevice::new(
            format!("card:{}", c.id),
            DetectedKind::SoundCard,
            c.name.clone(),
            c.id.clone(),
        );
        d.vendor_id = c.usb.map(|u| u.0);
        d.product_id = c.usb.map(|u| u.1);
        d.used_by = used_by;
        if let Some(s) = &c.stream {
            d.capture_channels = s.capture_channels;
            d.playback_channels = s.playback_channels;
            d.rates.clone_from(&s.rates);
        }
        d.pcm = Some(format!("hw:CARD={},DEV=0", c.id));
        out.push(d);
    }
    for m in &scan.midi {
        let used_by = surfaces
            .iter()
            .find(|s| match &s.source {
                SurfaceSource::Midi { card, device } => m.matches(card, *device),
                _ => false,
            })
            .map(|s| s.name.clone());
        let mut d = DetectedDevice::new(
            m.id.clone(),
            DetectedKind::Midi,
            m.name.clone(),
            format!("/dev/snd/{}", m.node_name),
        );
        d.vendor_id = m.usb.map(|u| u.0);
        d.product_id = m.usb.map(|u| u.1);
        d.used_by = used_by;
        d.source = Some(SurfaceSource::Midi {
            card: m.card.clone(),
            device: m.device,
        });
        out.push(d);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/surfaces/sc3")
    }

    #[test]
    fn uevent_parses_ids_name_and_phys_interface() {
        let ev = parse_hid_uevent(
            "DRIVER=hid-generic\nHID_ID=0003:00003142:00000C33\nHID_NAME=MV-SILICON fifine SC3\nHID_PHYS=usb-0000:00:14.0-1/input3\nHID_UNIQ=\n",
        )
        .unwrap();
        assert_eq!(
            ev,
            HidUevent {
                bus: 3,
                vendor_id: 0x3142,
                product_id: 0x0c33,
                name: "MV-SILICON fifine SC3".into(),
                phys_interface: Some(3),
            }
        );
        assert_eq!(parse_hid_uevent("HID_NAME=x\n"), None, "no id");
        assert_eq!(
            parse_hid_uevent("HID_ID=0003:1FFFF:0001\n"),
            None,
            "too wide"
        );
        assert_eq!(parse_hid_uevent("HID_ID=zz:1:2\n"), None);
    }

    #[test]
    fn interface_number_is_hex() {
        assert_eq!(parse_interface_number("03\n"), Some(3));
        assert_eq!(parse_interface_number("0a"), Some(10));
        assert_eq!(parse_interface_number(""), None);
    }

    #[test]
    fn descriptor_report_ids_are_found_by_walking_items() {
        // Usage Page (vendor), Usage, Collection, Report ID 1, ..., End.
        let numbered = [0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x85, 0x01, 0xC0];
        assert!(descriptor_numbered(&numbered));
        // 0x85 only as data of a two-byte Usage Page item.
        let plain = [0x06, 0x85, 0x00, 0x09, 0x01, 0xA1, 0x01, 0xC0];
        assert!(!descriptor_numbered(&plain));
        // A long item skipped whole; truncated input ends the walk.
        assert!(!descriptor_numbered(&[0xFE, 0x02, 0x00, 0x85, 0x85]));
        assert!(!descriptor_numbered(&[0x26]));
    }

    #[test]
    fn cards_parse_index_id_and_short_name() {
        let text = " 0 [PCH            ]: HDA-Intel - HDA Intel PCH\n                      HDA Intel PCH at 0xf7f10000 irq 32\n 1 [SC3            ]: USB-Audio - fifine SC3\n                      MV-SILICON fifine SC3 at usb-0000:00:14.0-1, full speed\n";
        let cards = parse_cards(text);
        assert_eq!(cards.len(), 2);
        assert_eq!((cards[1].index, cards[1].id.as_str()), (1, "SC3"));
        assert_eq!(cards[1].name, "fifine SC3");
        assert!(parse_cards("--- no soundcards ---\n").is_empty());
        assert_eq!(parse_usbid("3142:0c33\n"), Some((0x3142, 0x0c33)));
    }

    #[test]
    fn alsa_device_strings_name_cards() {
        let c = Card {
            index: 1,
            id: "SC3".into(),
            name: String::new(),
            usb: None,
            stream: None,
        };
        assert!(pcm_names_card("hw:CARD=SC3,DEV=0", &c));
        assert!(pcm_names_card("plughw:1,0", &c));
        assert!(pcm_names_card("hw:SC3", &c));
        assert!(!pcm_names_card("hw:CARD=PCH,DEV=0", &c));
        assert!(!pcm_names_card("default", &c));
    }

    #[test]
    fn the_sc3_fixture_scans_to_one_hid_interface_and_one_card() {
        let s = scan(&fixture());
        assert_eq!(s.hid.len(), 1, "{s:?}");
        let h = &s.hid[0];
        assert_eq!(h.id, "hid:3142:0c33:3");
        assert_eq!(h.interface, Some(3));
        assert_eq!(h.numbered, Some(true));
        assert_eq!(h.node, fixture().join("dev/hidraw3"));
        assert_eq!(s.cards.len(), 2, "PCH and SC3");
        assert_eq!(s.cards[1].id, "SC3");
        assert_eq!(s.cards[1].usb, Some((0x3142, 0x0c33)));
        assert_eq!(s.cards[0].usb, None, "not a USB card");
        assert!(s.find_hid(0x3142, 0x0c33, Some(3)).is_some());
        assert!(s.find_hid(0x3142, 0x0c33, None).is_some());
        assert!(s.find_hid(0x3142, 0x0c33, Some(4)).is_none());

        let spec = SurfaceSpec::new(
            "desk",
            SurfaceSource::Hidraw {
                vendor_id: 0x3142,
                product_id: 0x0c33,
                interface: Some(3),
            },
            vec![],
            vec![],
        );
        let d = detected(&s, &[spec], &[]);
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].used_by.as_deref(), Some("desk"));
        assert_eq!(d[0].node, "/dev/hidraw3");
        assert!(d[0].source.is_some());
        assert_eq!(d[2].id, "card:SC3");
        assert_eq!(d[2].kind, DetectedKind::SoundCard);
        assert_eq!(d[2].vendor_id, Some(0x3142));
        assert_eq!(d[2].used_by, None);
    }

    #[test]
    fn stream0_gives_channels_and_rates() {
        let s = parse_stream(
            "X at usb-1, high speed : USB Audio\n\nPlayback:\n  Status: Stop\n  Interface 1\n    Altset 1\n    Channels: 2\n    Rates: 44100, 48000\n  Interface 1\n    Altset 2\n    Channels: 4\n    Rates: 96000\n\nCapture:\n  Interface 2\n    Altset 1\n    Channels: 1\n    Rates: 8000 - 48000 (continuous)\n",
        )
        .unwrap();
        assert_eq!(s.playback_channels, Some(4));
        assert_eq!(s.capture_channels, Some(1));
        assert_eq!(s.rates, vec![8000, 44100, 48000, 96000]);
        assert_eq!(parse_stream("nothing here\n"), None);
        assert_eq!(parse_midi_node("midiC1D0"), Some((1, 0)));
        assert_eq!(parse_midi_node("midiC12D3"), Some((12, 3)));
        assert_eq!(parse_midi_node("pcmC1D0p"), None);
        assert_eq!(parse_midi_node("midiC1"), None);
    }

    #[test]
    fn the_cq_fixture_scans_to_a_card_and_its_midi_port() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/surfaces/cq");
        let s = scan(&root);
        assert_eq!(s.midi.len(), 1, "{s:?}");
        let m = &s.midi[0];
        assert_eq!(m.id, "midi:CQ:0");
        assert_eq!((m.card.as_str(), m.device), ("CQ", 0));
        assert_eq!(m.node, root.join("dev/snd/midiC1D0"));
        assert!(s.find_midi("CQ", 0).is_some());
        assert!(s.find_midi("CQ", 1).is_none());
        let spec = SurfaceSpec::new(
            "desk",
            SurfaceSource::Midi {
                card: "CQ".into(),
                device: 0,
            },
            vec![],
            vec![],
        );
        let d = detected(&s, &[spec], &[]);
        let card = d.iter().find(|d| d.id == "card:CQ").unwrap();
        assert_eq!(card.capture_channels, Some(16));
        assert_eq!(card.playback_channels, Some(16));
        assert_eq!(card.rates, vec![48_000]);
        assert_eq!(card.pcm.as_deref(), Some("hw:CARD=CQ,DEV=0"));
        let midi = d.iter().find(|d| d.id == "midi:CQ:0").unwrap();
        assert_eq!(midi.kind, DetectedKind::Midi);
        assert_eq!(midi.name, "Allen&Heath CQ");
        assert_eq!(midi.node, "/dev/snd/midiC1D0");
        assert_eq!(midi.used_by.as_deref(), Some("desk"));
        assert_eq!(midi.vendor_id, Some(0x22f0));
        assert_eq!(
            midi.source,
            Some(SurfaceSource::Midi {
                card: "CQ".into(),
                device: 0
            })
        );
    }

    #[test]
    fn a_missing_root_scans_empty() {
        assert_eq!(scan(Path::new("/nonexistent/tessera")), Scan::default());
    }
}
