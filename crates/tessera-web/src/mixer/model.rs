//! The view model of a mixer panel: what the widgets render, independent of
//! where a panel comes from. [`default_panel`] derives one from the
//! daemon's status while none is saved; [`from_spec`] maps a saved panel
//! (`Query::Mixers`, a `MixerSpec`) to the same [`Panel`]. See
//! `docs/plans/2026-11-22-mixer-panel.md`.

use tessera_control_types::{
    BindingAction, BridgeDirection, BridgeState, ButtonMode as SpecMode, Channel, ChannelSet,
    ChannelStatus, CrosspointKey, DeviceKey, DeviceStatus, Direction, Gain, HostVolumeStatus,
    MixerSpec, MixerStrip, ReceiverCarrier, RouterPort, RouterReport, Status, StripControl,
    SurfacesReport,
};

use crate::pages::{SLIDER_MAX_DB, SLIDER_OFF_DB, path_segment};
use crate::ports;

/// One desk.
#[derive(Debug, Clone, PartialEq)]
pub struct Panel {
    /// Unique on the page (letters, digits, `-`).
    pub id: String,
    /// Shown above the strips.
    pub title: String,
    /// In order; the master group, if any, last.
    pub groups: Vec<Group>,
    /// Where its forms return without the script (`/mixer`,
    /// `/mixer?panel=<id>`), before the strip's fragment.
    pub back: String,
}

/// Strips under one heading.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// Unique on the page.
    pub id: String,
    /// The heading.
    pub title: String,
    /// Shown folded (a `<details>`), for channels not in use.
    pub collapsed: bool,
    /// Drawn apart from the other groups (Main).
    pub master: bool,
    /// In order.
    pub strips: Vec<Strip>,
}

/// One channel strip: a fader, its buttons and the scribble-strip label.
#[derive(Debug, Clone, PartialEq)]
pub struct Strip {
    /// The element id, unique on the page; forms return to it.
    pub id: String,
    /// The scribble strip (a channel label, `Main L/R`).
    pub label: String,
    /// What it acts on, in a few words (`TX 1`, `host output 1–2`,
    /// `CQ input → Speaker`).
    pub detail: String,
    /// What it acts on in full (the detail's tooltip and the strip's
    /// accessible description); empty: the detail says it all.
    pub about: String,
    /// A master strip (drawn in the master colour).
    pub master: bool,
    /// The fader, if the strip has one.
    pub fader: Option<Fader>,
    /// Buttons, top to bottom.
    pub buttons: Vec<Button>,
    /// Something the strip cannot show (the card cannot be read, a target
    /// that is not there).
    pub problem: Option<String>,
    /// The amplifier's playback switch is off (portal amp volume): offer
    /// its Unmute.
    pub amp_unmute: bool,
}

/// The channels of one device in one direction (a `ChannelSet`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The device key.
    pub device: String,
    /// RX (network → this host) or TX (this host → network).
    pub direction: Direction,
    /// 1-based channel numbers, in order, at least one.
    pub channels: Vec<u16>,
}

impl Target {
    /// `rx` or `tx`, as the forms and paths say it.
    #[must_use]
    pub const fn dir_word(&self) -> &'static str {
        match self.direction {
            Direction::Rx => "rx",
            Direction::Tx => "tx",
        }
    }

    /// `1` or `1,2,3`, as the gain path and the mute form take it.
    #[must_use]
    pub fn channel_list(&self) -> String {
        self.channels
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// `RX 1`, `TX 1–2`, `RX 1, 3`: the channels in words.
    #[must_use]
    pub fn words(&self) -> String {
        let dir = self.dir_word().to_uppercase();
        let c = &self.channels;
        let contiguous = c.windows(2).all(|w| w[1] == w[0] + 1);
        match c.as_slice() {
            [one] => format!("{dir} {one}"),
            [first, .., last] if contiguous => format!("{dir} {first}\u{2013}{last}"),
            _ => format!("{dir} {}", self.channel_list().replace(',', ", ")),
        }
    }

    /// The same channels as the contract's `ChannelSet`.
    #[must_use]
    pub fn set(&self) -> ChannelSet {
        ChannelSet {
            device: DeviceKey(self.device.clone()),
            direction: self.direction,
            channels: self
                .channels
                .iter()
                .filter_map(|c| Channel::new(*c))
                .collect(),
        }
    }

    /// The target of a `ChannelSet`.
    #[must_use]
    pub fn of(set: &ChannelSet) -> Self {
        Self {
            device: set.device.0.clone(),
            direction: set.direction,
            channels: set.channels.iter().map(|c| c.get()).collect(),
        }
    }
}

/// What a fader sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaderAction {
    /// The channels' own (input) level: `SetChannelGain`.
    Gain,
    /// RX channels' output level, after the local mix: `SetOutputGain`.
    OutputGain,
    /// The appliance amplifier's volume set by the portal with `amixer`
    /// (`[appliance.volume]`), while the daemon does not own it.
    AmpVolume,
    /// The host volume the daemon owns: `SetHostVolume`.
    HostVolume,
    /// Router port levels: `SetPortGain`.
    PortGain,
    /// Router crosspoint gains: `SetCrosspointGain`.
    CrosspointGain,
}

/// Where a fader sits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Level {
    /// A gain, dB.
    Db(f64),
    /// Off: the bottom of the travel.
    Off,
    /// The amplifier's lowest step, its own mute.
    Floor,
}

/// A fader's travel, dB. `off_at_bottom`: the bottom position means off
/// (channel gains), not `min` (the amplifier's range).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Range {
    /// Bottom, dB.
    pub min: f64,
    /// Top, dB.
    pub max: f64,
    /// Step, dB.
    pub step: f64,
    /// The bottom is off.
    pub off_at_bottom: bool,
}

impl Range {
    /// A channel gain's travel: off, then up to +12 dB in 0.5 dB steps.
    pub const GAIN: Self = Self {
        min: SLIDER_OFF_DB,
        max: SLIDER_MAX_DB,
        step: 0.5,
        off_at_bottom: true,
    };

    /// A volume's travel, `min`..`max` dB (the bottom is `min`, not off).
    #[must_use]
    pub const fn volume(min: f64, max: f64) -> Self {
        Self {
            min,
            max,
            step: 0.5,
            off_at_bottom: false,
        }
    }
}

/// A fader: its action and target, where it is, who owns it.
#[derive(Debug, Clone, PartialEq)]
pub struct Fader {
    /// What it sets.
    pub action: FaderAction,
    /// The channels of a channel action.
    pub target: Option<Target>,
    /// The router ports of [`FaderAction::PortGain`].
    pub ports: Vec<RouterPort>,
    /// The crosspoints of [`FaderAction::CrosspointGain`].
    pub crosspoints: Vec<CrosspointKey>,
    /// Where it is now.
    pub level: Level,
    /// Its travel.
    pub range: Range,
    /// The connected surface whose non-motorized fader sets this level:
    /// the fader is read-only.
    pub owner: Option<String>,
    /// Surface controls bound to the same action and target.
    pub links: Vec<Link>,
}

/// What a button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonAction {
    /// Mute the channels: `SetChannelMute`.
    Mute,
    /// Mute RX channels' output, after the local mix: `SetOutputMute`.
    OutputMute,
    /// Mute the host volume: `SetHostMute`.
    HostMute,
    /// Mute router ports: `SetPortMute`.
    PortMute,
    /// Mute router crosspoints: `SetCrosspointMute`.
    CrosspointMute,
}

/// How a button acts (the surface binding's modes, as far as the portal
/// offers them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonMode {
    /// Each press flips it.
    Toggle,
    /// On while held; release restores the state from before the press.
    Momentary,
}

/// A button on a strip.
#[derive(Debug, Clone, PartialEq)]
pub struct Button {
    /// What it does.
    pub action: ButtonAction,
    /// The channels of a channel action.
    pub target: Option<Target>,
    /// The router ports of [`ButtonAction::PortMute`].
    pub ports: Vec<RouterPort>,
    /// The crosspoints of [`ButtonAction::CrosspointMute`].
    pub crosspoints: Vec<CrosspointKey>,
    /// Toggle or momentary.
    pub mode: ButtonMode,
    /// On now (muted). For several targets: all of them.
    pub on: bool,
    /// Some of several targets are on, not all.
    pub mixed: bool,
    /// The caption (`Mute`); the pressed caption adds `d` (`Muted`).
    pub label: String,
    /// Surface controls bound to the same action and target.
    pub links: Vec<Link>,
}

/// A surface control that drives the same action and target as a strip
/// control (derived from the surface's binding, `StripControl::links_to`;
/// nothing is stored in the mixer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The surface's name.
    pub surface: String,
    /// The control's id.
    pub control: String,
    /// The control's label (its id when it has none).
    pub label: String,
    /// The surface is open now.
    pub connected: bool,
    /// A motorized fader: it follows the strip and owns nothing.
    pub motorized: bool,
    /// Name the surface beside the control: more than one surface is
    /// enabled. With one, the control's label alone says it (the surface
    /// stays in the link's title and accessible name).
    pub surface_shown: bool,
}

/// What the Main strip controls ([`crate::mixer::main_target`]).
#[derive(Debug, Clone, PartialEq)]
pub enum MainTarget {
    /// The host volume the daemon owns (`Status::host_volume`).
    HostVolume(HostVolumeStatus),
    /// The appliance amplifier's volume set by the portal, as read now
    /// (`Err`: why not), and its range.
    AmpVolume {
        /// The level read, or why it could not be.
        level: Result<crate::appliance::volume::Level, String>,
        /// Bottom of the range, dB.
        min: f64,
        /// Top of the range, dB.
        max: f64,
    },
    /// The output stage of every RX channel of each device.
    OutputStage,
}

/// What a panel is built from.
#[derive(Debug, Clone, Copy)]
pub struct Sources<'a> {
    /// The daemon's status.
    pub status: &'a Status,
    /// The host router, when the daemon reports one.
    pub router: Option<&'a RouterReport>,
    /// The surfaces, for the links.
    pub surfaces: Option<&'a SurfacesReport>,
    /// What Main is.
    pub main: &'a MainTarget,
}

/// The element id of a channel strip.
#[must_use]
pub fn strip_id(device: &str, direction: Direction, channel: u16) -> String {
    format!(
        "strip-{}-{}-{channel}",
        path_segment(device),
        match direction {
            Direction::Rx => "rx",
            Direction::Tx => "tx",
        }
    )
}

/// Surface controls bound to the same action and an overlapping target as
/// `control` (`StripControl::links_to`), on enabled surfaces.
#[must_use]
pub fn links_for(surfaces: Option<&SurfacesReport>, control: &StripControl) -> Vec<Link> {
    let Some(r) = surfaces else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let surface_shown = r.surfaces.iter().filter(|s| s.spec.enabled).count() > 1;
    for s in &r.surfaces {
        if !s.spec.enabled {
            continue;
        }
        for b in &s.spec.bindings {
            if !control.links_to(b) {
                continue;
            }
            let c = s.spec.controls.iter().find(|c| c.id == b.control);
            out.push(Link {
                surface: s.spec.name.clone(),
                control: b.control.clone(),
                label: c
                    .and_then(|c| c.label.clone())
                    .unwrap_or_else(|| b.control.clone()),
                connected: s.connected,
                motorized: c.is_some_and(|c| c.motorized),
                surface_shown,
            });
        }
    }
    out
}

/// Surface controls bound to `action` on any of `target`'s channels.
#[must_use]
pub fn links(
    surfaces: Option<&SurfacesReport>,
    action: BindingAction,
    target: &Target,
) -> Vec<Link> {
    let control = if action.is_mute() {
        StripControl::channel_button(action, target.set())
    } else {
        StripControl::channel_fader(action, target.set())
    };
    links_for(surfaces, &control)
}

fn level_of(g: Gain) -> Level {
    g.milli_db()
        .map_or(Level::Off, |m| Level::Db(f64::from(m) / 1000.0))
}

/// Is this RX channel in use: a subscription, a saved routing intent or a
/// carrier?
fn rx_in_use(d: &DeviceStatus, c: &ChannelStatus) -> bool {
    d.subscriptions.iter().any(|s| s.rx_channel == c.channel)
        || c.routing
            .as_ref()
            .is_some_and(|r| r.intent.is_some() || !matches!(r.carrier, ReceiverCarrier::None))
}

/// Is this TX channel in use: fed by a capture bridge that is not disabled?
fn tx_in_use(s: &Status, d: &DeviceStatus, c: &ChannelStatus) -> bool {
    let n = c.channel.get();
    s.bridges.iter().any(|b| {
        b.device == d.key
            && b.direction == BridgeDirection::Capture
            && !b.local_only
            && b.state != BridgeState::Disabled
            && n >= b.first_channel
            && u32::from(n) < u32::from(b.first_channel) + u32::from(b.channels)
    })
}

/// The strip of one channel: its input level and mute.
fn channel_strip(
    d: &DeviceStatus,
    direction: Direction,
    c: &ChannelStatus,
    several_devices: bool,
    surfaces: Option<&SurfacesReport>,
) -> Strip {
    let n = c.channel.get();
    let target = Target {
        device: d.key.0.clone(),
        direction,
        channels: vec![n],
    };
    let mut detail = target.words();
    if several_devices {
        detail = format!("{detail} \u{b7} {}", d.name);
    }
    Strip {
        id: strip_id(&d.key.0, direction, n),
        label: c.label.clone(),
        detail,
        about: String::new(),
        master: false,
        fader: Some(Fader {
            action: FaderAction::Gain,
            level: level_of(c.gain),
            range: Range::GAIN,
            owner: c.gain_owner.clone(),
            links: links(surfaces, BindingAction::Gain, &target),
            target: Some(target.clone()),
            ports: Vec::new(),
            crosspoints: Vec::new(),
        }),
        buttons: vec![Button {
            action: ButtonAction::Mute,
            mode: ButtonMode::Toggle,
            on: c.muted,
            mixed: false,
            label: "Mute".into(),
            links: links(surfaces, BindingAction::Mute, &target),
            target: Some(target),
            ports: Vec::new(),
            crosspoints: Vec::new(),
        }],
        problem: None,
        amp_unmute: false,
    }
}

/// How many host outputs the output-stage Main covers: the main stereo
/// pair, host outputs 1 and 2 (on the speaker, what its amplifier plays).
/// Other host outputs (a desk's USB returns, ...) keep their own levels.
pub const MAIN_OUTPUTS: usize = 2;

/// The Main strip of one device's output stage: the output level and
/// output mute of its main host outputs (1–2: network inputs 1–2 after
/// the local mix), with one command each.
fn output_main(
    d: &DeviceStatus,
    several_devices: bool,
    surfaces: Option<&SurfacesReport>,
) -> Option<Strip> {
    let first = d.rx_channels.first()?;
    let main: Vec<&ChannelStatus> = d.rx_channels.iter().take(MAIN_OUTPUTS).collect();
    let target = Target {
        device: d.key.0.clone(),
        direction: Direction::Rx,
        channels: main.iter().map(|c| c.channel.get()).collect(),
    };
    let all = main.iter().all(|c| c.output_muted);
    let any = main.iter().any(|c| c.output_muted);
    let same_level = main.iter().all(|c| c.output_gain == first.output_gain);
    let outputs = target.words().replacen("RX", "host output", 1);
    let mut detail = outputs.clone();
    if several_devices {
        detail = format!("{detail} \u{b7} {}", d.name);
    }
    let about = format!(
        "The output level and mute of {outputs}: network inputs after the local mix, as this host plays them{}.",
        if d.rx_channels.len() > MAIN_OUTPUTS {
            "; the other host outputs keep their own levels"
        } else {
            ""
        }
    );
    Some(Strip {
        id: format!("strip-{}-master", path_segment(&d.key.0)),
        label: if several_devices {
            format!("Main \u{b7} {}", d.name)
        } else if main.len() > 1 {
            "Main L/R".into()
        } else {
            "Main".into()
        },
        detail,
        about,
        master: true,
        fader: Some(Fader {
            action: FaderAction::OutputGain,
            level: level_of(first.output_gain),
            range: Range::GAIN,
            owner: main.iter().find_map(|c| c.output_gain_owner.clone()),
            links: links(surfaces, BindingAction::OutputGain, &target),
            target: Some(target.clone()),
            ports: Vec::new(),
            crosspoints: Vec::new(),
        }),
        buttons: vec![Button {
            action: ButtonAction::OutputMute,
            mode: ButtonMode::Toggle,
            on: all,
            mixed: any && !all,
            label: "Mute".into(),
            links: links(surfaces, BindingAction::OutputMute, &target),
            target: Some(target),
            ports: Vec::new(),
            crosspoints: Vec::new(),
        }],
        problem: (!same_level).then(|| {
            "The channels' output levels differ; the fader shows the first and sets them all."
                .to_owned()
        }),
        amp_unmute: false,
    })
}

/// The amplifier's Main strip, set by the portal.
fn amp_main(level: &Result<crate::appliance::volume::Level, String>, min: f64, max: f64) -> Strip {
    let (fader, problem) = match level {
        Ok(l) => (
            Some(Fader {
                action: FaderAction::AmpVolume,
                target: None,
                ports: Vec::new(),
                crosspoints: Vec::new(),
                level: if crate::appliance::volume::is_mute_step(l.db) {
                    Level::Floor
                } else {
                    Level::Db(l.db)
                },
                range: Range::volume(min, max),
                owner: None,
                links: Vec::new(),
            }),
            l.muted.then(|| {
                "The amplifier's output is muted (its playback switch is off): nothing is heard at any volume."
                    .to_owned()
            }),
        ),
        Err(e) => (None, Some(format!("The volume cannot be read: {e}"))),
    };
    Strip {
        id: "strip-master".into(),
        label: "Main L/R".into(),
        detail: "amplifier volume".into(),
        about: "The speaker amplifier's volume, which this portal sets.".into(),
        master: true,
        fader,
        buttons: Vec::new(),
        problem,
        amp_unmute: level.as_ref().is_ok_and(|l| l.muted),
    }
}

/// The host volume's fader as the daemon reports it: the reading, else
/// the saved value, else the bottom.
fn host_fader(hv: &HostVolumeStatus, surfaces: Option<&SurfacesReport>) -> Fader {
    let shown = hv.reading.map(|r| r.gain).or(hv.gain);
    let (min, max) = (
        f64::from(hv.min_mdb) / 1000.0,
        f64::from(hv.max_mdb) / 1000.0,
    );
    Fader {
        action: FaderAction::HostVolume,
        target: None,
        ports: Vec::new(),
        crosspoints: Vec::new(),
        level: match shown.and_then(Gain::milli_db) {
            Some(m) => Level::Db(f64::from(m) / 1000.0),
            None => Level::Floor,
        },
        range: Range::volume(min, max),
        owner: hv.gain_owner.clone(),
        links: links_for(surfaces, &StripControl::host_volume()),
    }
}

fn host_button(hv: &HostVolumeStatus, surfaces: Option<&SurfacesReport>) -> Button {
    Button {
        action: ButtonAction::HostMute,
        target: None,
        ports: Vec::new(),
        crosspoints: Vec::new(),
        mode: ButtonMode::Toggle,
        on: hv.muted,
        mixed: false,
        label: "Mute".into(),
        links: links_for(surfaces, &StripControl::host_mute()),
    }
}

/// Why the host volume needs attention, if it does.
fn host_problem(hv: &HostVolumeStatus) -> Option<String> {
    if let Some(p) = &hv.problem {
        return Some(format!("The speaker volume: {p}"));
    }
    if hv.reading.is_some_and(|r| r.muted) && !hv.muted {
        return Some(
            "The amplifier's playback switch is off: nothing is heard at any volume.".into(),
        );
    }
    None
}

/// The Main strip of the daemon's host volume.
fn host_main(hv: &HostVolumeStatus, surfaces: Option<&SurfacesReport>) -> Strip {
    Strip {
        id: "strip-master".into(),
        label: "Main L/R".into(),
        detail: "speaker volume".into(),
        about: format!(
            "The speaker amplifier's volume tesserad sets (card {}, control {}): everything this host plays on it.",
            hv.card, hv.control
        ),
        master: true,
        fader: Some(host_fader(hv, surfaces)),
        buttons: vec![host_button(hv, surfaces)],
        problem: host_problem(hv),
        amp_unmute: false,
    }
}

/// The panel shown while none is saved: per device the channels in use
/// (TX first: local inputs such as a desk's return; then RX: network
/// sources), the other channels in a folded group, and Main last. A device
/// with no channel in use shows all of them.
#[must_use]
pub fn default_panel(
    status: &Status,
    surfaces: Option<&SurfacesReport>,
    main: &MainTarget,
) -> Panel {
    let several = status.devices.len() > 1;
    let mut groups = Vec::new();
    let mut masters = Vec::new();
    for d in &status.devices {
        let key = path_segment(&d.key.0);
        let mut used = Vec::new();
        let mut rest = Vec::new();
        for (direction, chans) in [
            (Direction::Tx, &d.tx_channels),
            (Direction::Rx, &d.rx_channels),
        ] {
            for c in chans {
                let in_use = match direction {
                    Direction::Tx => tx_in_use(status, d, c),
                    Direction::Rx => rx_in_use(d, c),
                };
                let strip = channel_strip(d, direction, c, several, surfaces);
                if in_use {
                    used.push(strip)
                } else {
                    rest.push(strip)
                }
            }
        }
        if used.is_empty() {
            used = std::mem::take(&mut rest);
        }
        let name = if several {
            format!(" \u{b7} {}", d.name)
        } else {
            String::new()
        };
        if !used.is_empty() {
            groups.push(Group {
                id: format!("group-dev-{key}"),
                title: format!("Channels{name}"),
                collapsed: false,
                master: false,
                strips: used,
            });
        }
        if !rest.is_empty() {
            groups.push(Group {
                id: format!("group-dev-{key}-more"),
                title: format!("More channels{name}"),
                collapsed: true,
                master: false,
                strips: rest,
            });
        }
        if matches!(main, MainTarget::OutputStage) {
            masters.extend(output_main(d, several, surfaces));
        }
    }
    match main {
        MainTarget::AmpVolume { level, min, max } => masters.push(amp_main(level, *min, *max)),
        MainTarget::HostVolume(hv) => masters.push(host_main(hv, surfaces)),
        MainTarget::OutputStage => {}
    }
    if !masters.is_empty() {
        groups.push(master_group(masters));
    }
    Panel {
        id: "default".into(),
        title: "Mixer".into(),
        groups,
        back: "/mixer".into(),
    }
}

fn master_group(strips: Vec<Strip>) -> Group {
    Group {
        id: "group-master".into(),
        title: "Main".into(),
        collapsed: false,
        master: true,
        strips,
    }
}

/// The default panel as a saved panel (`id = "main"`): its strips in use
/// (fader then mute per channel) and Main (the output stage or the
/// daemon's host volume; the portal's amp volume is not a daemon action
/// and stays on every panel by itself). The folded channels are left out.
#[must_use]
pub fn default_spec(status: &Status, main: &MainTarget) -> MixerSpec {
    let panel = default_panel(status, None, main);
    let mut strips: Vec<MixerStrip> = Vec::new();
    let mut n = 0;
    let mut next_id = |what: &str| {
        n += 1;
        format!("{what}-{n}")
    };
    for g in panel.groups.iter().filter(|g| !g.collapsed) {
        let mut first = !g.master;
        for s in &g.strips {
            let master = s.master;
            if let Some(f) = &s.fader {
                let control = match (f.action, &f.target) {
                    (FaderAction::Gain, Some(t)) => {
                        StripControl::channel_fader(BindingAction::Gain, t.set())
                    }
                    (FaderAction::OutputGain, Some(t)) => {
                        StripControl::channel_fader(BindingAction::OutputGain, t.set())
                    }
                    (FaderAction::HostVolume, _) => StripControl::host_volume(),
                    _ => continue,
                };
                let mut m = MixerStrip::new(next_id("fader"), control);
                if master {
                    m.master = true;
                    m.label = s.label.clone();
                }
                if first && g.title != "Channels" {
                    m.group = Some(g.title.chars().take(31).collect());
                }
                first = false;
                strips.push(m);
            }
            for b in &s.buttons {
                let control = match (b.action, &b.target) {
                    (ButtonAction::Mute, Some(t)) => {
                        StripControl::channel_button(BindingAction::Mute, t.set())
                    }
                    (ButtonAction::OutputMute, Some(t)) => {
                        StripControl::channel_button(BindingAction::OutputMute, t.set())
                    }
                    (ButtonAction::HostMute, _) => StripControl::host_mute(),
                    _ => continue,
                };
                let mut m = MixerStrip::new(next_id("button"), control);
                if master {
                    m.master = true;
                    m.label = s.label.clone();
                }
                strips.push(m);
            }
        }
    }
    MixerSpec::new("main", "Mixer", strips)
}

/// Port words shortened for a narrow scribble strip: `input` → `in`,
/// `output` → `out` (`CQ input 15` → `CQ in 15`). Labels someone
/// gave are not passed through it.
fn scribble_words(s: &str) -> String {
    s.split(' ')
        .map(|w| match w {
            "input" => "in",
            "output" => "out",
            w => w,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Several names in a few words, for a scribble strip: one name as it is;
/// names that differ only in a short last part as `Speaker L/R`,
/// `CQ input 15/16`; names sharing their first words as `CQ (3)`; else
/// `3 ports`.
fn summary(names: &[String]) -> String {
    let mut v: Vec<&str> = Vec::new();
    for n in names {
        if !v.contains(&n.as_str()) {
            v.push(n);
        }
    }
    match v.as_slice() {
        [] => String::new(),
        [one] => (*one).to_owned(),
        _ => {
            let words: Vec<Vec<&str>> = v.iter().map(|n| n.split(' ').collect()).collect();
            let mut common = 0;
            while words
                .iter()
                .all(|w| w.len() > common && w[common] == words[0][common])
            {
                common += 1;
            }
            let prefix = words[0][..common].join(" ");
            let rests: Vec<String> = words.iter().map(|w| w[common..].join(" ")).collect();
            if !prefix.is_empty()
                && rests
                    .iter()
                    .all(|r| !r.is_empty() && r.chars().count() <= 3)
            {
                format!("{prefix} {}", rests.join("/"))
            } else if !prefix.is_empty() {
                format!("{prefix} ({})", v.len())
            } else {
                format!("{} ports", v.len())
            }
        }
    }
}

/// What a saved strip acts on, in full: every port or crosspoint by name.
fn target_about(src: &Sources<'_>, c: &StripControl) -> String {
    match c.action() {
        BindingAction::PortGain | BindingAction::PortMute => c
            .ports()
            .iter()
            .map(|p| {
                let name = ports::label(src.router, p);
                let place = ports::place(p);
                if name == place {
                    name
                } else {
                    format!("{name} ({place})")
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
        BindingAction::CrosspointGain | BindingAction::CrosspointMute => c
            .crosspoints()
            .iter()
            .map(|k| ports::xp_words(src.router, k))
            .collect::<Vec<_>>()
            .join(", "),
        _ => String::new(),
    }
}

/// The words of a strip control's target and its default label.
fn target_words(src: &Sources<'_>, c: &StripControl) -> (String, String) {
    match c.action() {
        BindingAction::HostVolume | BindingAction::HostMute => {
            ("speaker volume".into(), "Main".into())
        }
        BindingAction::PortGain | BindingAction::PortMute => {
            let labels: Vec<String> = c
                .ports()
                .iter()
                .map(|p| ports::short(src.router, p))
                .collect();
            let places: Vec<String> = c.ports().iter().map(ports::place).collect();
            (summary(&places), labels.join(" / "))
        }
        BindingAction::CrosspointGain | BindingAction::CrosspointMute => {
            let sources: Vec<String> = c
                .crosspoints()
                .iter()
                .map(|k| ports::short(src.router, &k.source))
                .collect();
            let first = sources.first().cloned().unwrap_or_default();
            // A scribble strip is narrow: the short words (`CQ in 15/16 →
            // Speaker L/R`); the strip's description has them in full.
            let narrow = |p: &RouterPort| {
                src.router
                    .and_then(|r| ports::status(r, p))
                    .filter(|s| ports::named(s))
                    .map_or_else(|| scribble_words(&ports::place(p)), |s| s.label.clone())
            };
            let from: Vec<String> = c.crosspoints().iter().map(|k| narrow(&k.source)).collect();
            let to: Vec<String> = c
                .crosspoints()
                .iter()
                .map(|k| narrow(&k.destination))
                .collect();
            (
                format!("{} \u{2192} {}", summary(&from), summary(&to)),
                first,
            )
        }
        _ => {
            let Some(set) = c.target() else {
                return (String::new(), String::new());
            };
            let t = Target::of(set);
            let mut words = t.words();
            if matches!(
                c.action(),
                BindingAction::OutputGain | BindingAction::OutputMute
            ) {
                // The output stage of RX n is host output n (as the
                // default panel's Main says).
                words = words.replacen("RX", "host output", 1);
            }
            let dev = src.status.devices.iter().find(|d| d.key == set.device);
            let list = dev.map(|d| match set.direction {
                Direction::Rx => &d.rx_channels,
                Direction::Tx => &d.tx_channels,
            });
            let labels: Vec<String> = set
                .channels
                .iter()
                .filter_map(|n| {
                    list.and_then(|l| l.iter().find(|c| c.channel == *n))
                        .map(|c| c.label.clone())
                })
                .collect();
            (words, labels.join(" / "))
        }
    }
}

/// The channels a set names that the device has, or why not.
fn channels_of<'a>(src: &Sources<'a>, set: &ChannelSet) -> Result<Vec<&'a ChannelStatus>, String> {
    let Some(d) = src.status.devices.iter().find(|d| d.key == set.device) else {
        return Err(format!(
            "Not available: there is no device `{}`.",
            set.device.0
        ));
    };
    let list = match set.direction {
        Direction::Rx => &d.rx_channels,
        Direction::Tx => &d.tx_channels,
    };
    let found: Vec<&ChannelStatus> = set
        .channels
        .iter()
        .filter_map(|n| list.iter().find(|c| c.channel == *n))
        .collect();
    if found.len() < set.channels.len() || found.is_empty() {
        return Err(format!(
            "Not available: {} has no {} now.",
            set.device.0,
            Target::of(set).words()
        ));
    }
    Ok(found)
}

/// A saved fader as the widgets show it, with a problem when its target is
/// not there (the strip is kept, as saved).
fn spec_fader(src: &Sources<'_>, c: &StripControl) -> (Option<Fader>, Option<String>) {
    let StripControl::Fader {
        action,
        target,
        ports: ps,
        crosspoints,
        range,
        ..
    } = c
    else {
        return (None, None);
    };
    let links = links_for(src.surfaces, c);
    let custom =
        range.map(|r| Range::volume(f64::from(r.min_mdb) / 1000.0, f64::from(r.max_mdb) / 1000.0));
    let mut f = Fader {
        action: FaderAction::Gain,
        target: target.as_ref().map(Target::of),
        ports: ps.clone(),
        crosspoints: crosspoints.clone(),
        level: Level::Off,
        range: custom.unwrap_or(Range::GAIN),
        owner: None,
        links,
    };
    let problem = match action {
        BindingAction::Gain | BindingAction::OutputGain => {
            let output = *action == BindingAction::OutputGain;
            f.action = if output {
                FaderAction::OutputGain
            } else {
                FaderAction::Gain
            };
            match target.as_ref().map(|t| channels_of(src, t)) {
                Some(Ok(chans)) => {
                    let first = chans[0];
                    f.level = level_of(if output {
                        first.output_gain
                    } else {
                        first.gain
                    });
                    f.owner = chans.iter().find_map(|c| {
                        if output {
                            c.output_gain_owner.clone()
                        } else {
                            c.gain_owner.clone()
                        }
                    });
                    None
                }
                Some(Err(e)) => Some(e),
                None => Some("No channels.".into()),
            }
        }
        BindingAction::HostVolume => {
            f.action = FaderAction::HostVolume;
            match src.main {
                MainTarget::HostVolume(hv) => {
                    let h = host_fader(hv, src.surfaces);
                    f.level = h.level;
                    if custom.is_none() {
                        f.range = h.range;
                    }
                    f.owner = h.owner;
                    host_problem(hv)
                }
                _ => Some("Not available: this tesserad does not set the speaker volume.".into()),
            }
        }
        BindingAction::PortGain => {
            f.action = FaderAction::PortGain;
            match src
                .router
                .map(|r| ps.iter().map(|p| ports::status(r, p)).collect::<Vec<_>>())
            {
                Some(found)
                    if found.iter().all(|s| s.is_some_and(|s| s.available))
                        && !found.is_empty() =>
                {
                    let first = found[0].expect("checked");
                    f.level = level_of(first.gain);
                    f.owner = found.iter().flatten().find_map(|s| s.gain_owner.clone());
                    None
                }
                Some(_) => Some("Not available: a router port is missing now.".into()),
                None => Some("Not available: this tesserad does not report the router.".into()),
            }
        }
        BindingAction::CrosspointGain => {
            f.action = FaderAction::CrosspointGain;
            match src.router.map(|r| {
                crosspoints
                    .iter()
                    .map(|k| ports::crosspoint(r, k))
                    .collect::<Vec<_>>()
            }) {
                Some(found)
                    if found.iter().all(|x| x.is_some_and(|x| x.active)) && !found.is_empty() =>
                {
                    let first = found[0].expect("checked");
                    f.level = level_of(first.crosspoint.gain);
                    f.owner = found.iter().flatten().find_map(|x| x.gain_owner.clone());
                    None
                }
                Some(_) => Some("Not available: the crosspoint is not patched or inactive.".into()),
                None => Some("Not available: this tesserad does not report the router.".into()),
            }
        }
        _ => Some("This portal cannot show that fader action.".into()),
    };
    (Some(f), problem)
}

/// A saved button as the widgets show it.
fn spec_button(src: &Sources<'_>, c: &StripControl) -> (Option<Button>, Option<String>) {
    let StripControl::Button {
        action,
        target,
        ports: ps,
        crosspoints,
        mode,
        ..
    } = c
    else {
        return (None, None);
    };
    let mut b = Button {
        action: ButtonAction::Mute,
        target: target.as_ref().map(Target::of),
        ports: ps.clone(),
        crosspoints: crosspoints.clone(),
        mode: if *mode == SpecMode::Toggle {
            ButtonMode::Toggle
        } else {
            ButtonMode::Momentary
        },
        on: false,
        mixed: false,
        label: "Mute".into(),
        links: links_for(src.surfaces, c),
    };
    let states: Result<Vec<bool>, String> = match action {
        BindingAction::Mute | BindingAction::OutputMute => {
            let output = *action == BindingAction::OutputMute;
            b.action = if output {
                ButtonAction::OutputMute
            } else {
                ButtonAction::Mute
            };
            match target.as_ref().map(|t| channels_of(src, t)) {
                Some(Ok(chans)) => Ok(chans
                    .iter()
                    .map(|c| if output { c.output_muted } else { c.muted })
                    .collect()),
                Some(Err(e)) => Err(e),
                None => Err("No channels.".into()),
            }
        }
        BindingAction::HostMute => {
            b.action = ButtonAction::HostMute;
            match src.main {
                MainTarget::HostVolume(hv) => Ok(vec![hv.muted]),
                _ => Err("Not available: this tesserad does not set the speaker volume.".into()),
            }
        }
        BindingAction::PortMute => {
            b.action = ButtonAction::PortMute;
            match src.router.map(|r| {
                ps.iter()
                    .map(|p| ports::status(r, p).map(|s| s.muted))
                    .collect::<Option<Vec<bool>>>()
            }) {
                Some(Some(v)) if !v.is_empty() => Ok(v),
                Some(_) => Err("Not available: a router port is missing now.".into()),
                None => Err("Not available: this tesserad does not report the router.".into()),
            }
        }
        BindingAction::CrosspointMute => {
            b.action = ButtonAction::CrosspointMute;
            match src.router.map(|r| {
                crosspoints
                    .iter()
                    .map(|k| ports::crosspoint(r, k).map(|x| x.crosspoint.muted))
                    .collect::<Option<Vec<bool>>>()
            }) {
                Some(Some(v)) if !v.is_empty() => Ok(v),
                Some(_) => Err("Not available: the crosspoint is not patched.".into()),
                None => Err("Not available: this tesserad does not report the router.".into()),
            }
        }
        _ => Err("This portal cannot show that button action.".into()),
    };
    match states {
        Ok(v) => {
            b.on = v.iter().all(|m| *m);
            b.mixed = !b.on && v.iter().any(|m| *m);
            (Some(b), None)
        }
        Err(e) => (Some(b), Some(e)),
    }
}

/// Do two strip controls act on the same thing (a button that belongs
/// under the fader before it)?
fn same_target(a: &StripControl, b: &StripControl) -> bool {
    let host = |c: &StripControl| {
        matches!(
            c.action(),
            BindingAction::HostVolume | BindingAction::HostMute
        )
    };
    let output = |c: &StripControl| {
        matches!(
            c.action(),
            BindingAction::OutputGain | BindingAction::OutputMute
        )
    };
    if host(a) || host(b) {
        return host(a) && host(b);
    }
    output(a) == output(b)
        && a.target() == b.target()
        && a.ports() == b.ports()
        && a.crosspoints() == b.crosspoints()
}

/// The element id of a saved strip.
#[must_use]
pub fn spec_strip_id(panel: &str, strip: &str) -> String {
    format!("strip-{panel}-{strip}")
}

/// A saved panel as the widgets show it. A button strip right after a
/// fader strip on the same target, with no label or group of its own,
/// is drawn under that fader (one channel strip, as on a desk). Strips
/// whose target is gone are kept and say so.
#[must_use]
pub fn from_spec(spec: &MixerSpec, src: &Sources<'_>) -> Panel {
    let mut groups: Vec<Group> = Vec::new();
    let mut masters: Vec<Strip> = Vec::new();
    let mut prev: Option<(&MixerStrip, bool)> = None;
    let mut group_n = 0;
    for m in &spec.strips {
        let (words, default_label) = target_words(src, &m.control);
        // A button that belongs under the fader before it.
        if let (StripControl::Button { .. }, Some((p, master))) = (&m.control, prev) {
            if matches!(p.control, StripControl::Fader { .. })
                && master == m.master
                && m.group.is_none()
                && (m.label.is_empty() || m.label == p.label)
                && same_target(&p.control, &m.control)
            {
                let (button, problem) = spec_button(src, &m.control);
                let target = if m.master {
                    masters.last_mut()
                } else {
                    groups.last_mut().and_then(|g| g.strips.last_mut())
                };
                if let (Some(s), Some(b)) = (target, button) {
                    s.buttons.push(b);
                    if s.problem.is_none() {
                        s.problem = problem;
                    }
                    prev = None;
                    continue;
                }
            }
        }
        let (fader, mut problem) = spec_fader(src, &m.control);
        let mut buttons = Vec::new();
        if fader.is_none() {
            let (b, p) = spec_button(src, &m.control);
            buttons.extend(b);
            problem = p;
        }
        let strip = Strip {
            id: spec_strip_id(&spec.id, &m.id),
            label: if m.label.is_empty() {
                if default_label.is_empty() {
                    m.id.clone()
                } else {
                    default_label
                }
            } else {
                m.label.clone()
            },
            about: {
                let full = target_about(src, &m.control);
                if full == words { String::new() } else { full }
            },
            detail: words,
            master: m.master,
            fader,
            buttons,
            problem,
            amp_unmute: false,
        };
        if m.master {
            masters.push(strip);
        } else {
            if m.group.is_some() || groups.is_empty() {
                group_n += 1;
                groups.push(Group {
                    id: format!("group-{}-{group_n}", spec.id),
                    title: m.group.clone().unwrap_or_default(),
                    collapsed: false,
                    master: false,
                    strips: Vec::new(),
                });
            }
            if let Some(g) = groups.last_mut() {
                g.strips.push(strip);
            }
        }
        prev = Some((m, m.master));
    }
    // The portal's amp volume is no daemon action: it stays on every panel
    // until the daemon owns the host volume.
    if let MainTarget::AmpVolume { level, min, max } = src.main {
        masters.push(amp_main(level, *min, *max));
    }
    if !masters.is_empty() {
        groups.push(master_group(masters));
    }
    Panel {
        id: format!("panel-{}", spec.id),
        title: spec.title.clone(),
        groups,
        back: format!("/mixer?panel={}", spec.id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_control_types::{
        BridgeStatus, Channel, DeviceKey, Encoding, HostVolumeReading, LatencyUs, SampleRate,
        SubscriptionState, SubscriptionStatus,
    };

    fn ch(n: u16, label: &str) -> ChannelStatus {
        ChannelStatus::new(Channel::new(n).unwrap(), label)
    }

    fn speaker() -> Status {
        let d = DeviceStatus::new(
            DeviceKey("main".into()),
            "speaker",
            SampleRate(48_000),
            Encoding(24),
            LatencyUs(1_000),
        )
        .with_rx_channels(vec![
            ch(1, "ndimon L"),
            ch(2, "ndimon R"),
            ch(3, "CQ USB ret 1"),
            ch(4, "CQ USB ret 2"),
        ])
        .with_tx_channels(vec![ch(1, "CQ Main L"), ch(2, "CQ Main R")])
        .with_subscriptions(vec![
            SubscriptionStatus::new(
                Channel::new(1).unwrap(),
                "L",
                "ndimon",
                SubscriptionState::Receiving,
            ),
            SubscriptionStatus::new(
                Channel::new(2).unwrap(),
                "R",
                "ndimon",
                SubscriptionState::Receiving,
            ),
        ]);
        let mut b = BridgeStatus::new(
            "cq",
            DeviceKey("main".into()),
            "hw:CARD=CQ,DEV=0",
            BridgeDirection::Capture,
            BridgeState::Running,
        );
        b.first_channel = 1;
        b.channels = 2;
        Status::new("0").with_devices(vec![d]).with_bridges(vec![b])
    }

    fn ids(g: &Group) -> Vec<&str> {
        g.strips.iter().map(|s| s.id.as_str()).collect()
    }

    #[test]
    fn the_default_panel_puts_channels_in_use_first_and_main_last() {
        let p = default_panel(&speaker(), None, &MainTarget::OutputStage);
        let titles: Vec<_> = p
            .groups
            .iter()
            .map(|g| (g.title.as_str(), g.collapsed))
            .collect();
        assert_eq!(
            titles,
            [
                ("Channels", false),
                ("More channels", true),
                ("Main", false)
            ]
        );
        assert_eq!(
            ids(&p.groups[0]),
            [
                "strip-main-tx-1",
                "strip-main-tx-2",
                "strip-main-rx-1",
                "strip-main-rx-2"
            ]
        );
        assert_eq!(ids(&p.groups[1]), ["strip-main-rx-3", "strip-main-rx-4"]);
        let main = &p.groups[2].strips[0];
        assert_eq!(main.label, "Main L/R");
        let f = main.fader.as_ref().unwrap();
        assert_eq!(f.action, FaderAction::OutputGain);
        assert_eq!(f.target.as_ref().unwrap().channels, [1, 2]);
        assert_eq!(main.buttons[0].action, ButtonAction::OutputMute);
        assert_eq!(main.detail, "host output 1\u{2013}2");
    }

    #[test]
    fn several_names_are_summarised_for_the_scribble_strip() {
        let s = |v: &[&str]| summary(&v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>());
        assert_eq!(s(&["Speaker L"]), "Speaker L");
        assert_eq!(s(&["Speaker L", "Speaker R"]), "Speaker L/R");
        assert_eq!(s(&["CQ input 15", "CQ input 16"]), "CQ input 15/16");
        assert_eq!(s(&["CQ input 15", "CQ input 15"]), "CQ input 15");
        assert_eq!(s(&["CQ Main Left", "CQ Main Right"]), "CQ Main (2)");
        assert_eq!(s(&["ndimon L", "Desk"]), "2 ports");
    }

    #[test]
    fn a_device_with_nothing_in_use_shows_every_channel() {
        let mut s = speaker();
        s.bridges.clear();
        s.devices[0].subscriptions.clear();
        let p = default_panel(&s, None, &MainTarget::OutputStage);
        assert_eq!(p.groups.len(), 2);
        assert_eq!(p.groups[0].strips.len(), 6);
    }

    #[test]
    fn a_local_only_bridge_does_not_put_tx_channels_in_use() {
        let mut s = speaker();
        s.bridges[0].local_only = true;
        let p = default_panel(&s, None, &MainTarget::OutputStage);
        assert_eq!(ids(&p.groups[0]), ["strip-main-rx-1", "strip-main-rx-2"]);
    }

    #[test]
    fn the_amp_main_uses_its_range_and_reports_a_muted_card() {
        let main = MainTarget::AmpVolume {
            level: Ok(crate::appliance::volume::Level {
                db: -12.0,
                muted: true,
            }),
            min: -60.0,
            max: 0.0,
        };
        let p = default_panel(&speaker(), None, &main);
        let g = p.groups.last().unwrap();
        assert_eq!(g.strips.len(), 1);
        let f = g.strips[0].fader.as_ref().unwrap();
        assert_eq!(f.action, FaderAction::AmpVolume);
        assert_eq!(f.level, Level::Db(-12.0));
        assert!(!f.range.off_at_bottom && f.range.min == -60.0);
        assert!(g.strips[0].problem.as_deref().unwrap().contains("muted"));
    }

    #[test]
    fn the_host_main_is_the_daemons_volume_and_mute() {
        let mut hv = HostVolumeStatus::new("Headphones", "Digital", -60_000, 0);
        hv.gain = Some(Gain::MilliDb(-20_000));
        hv.reading = Some(HostVolumeReading::new(Gain::MilliDb(-21_000), false));
        hv.muted = true;
        hv.gain_owner = Some("cq".into());
        let p = default_panel(&speaker(), None, &MainTarget::HostVolume(hv));
        let main = &p.groups.last().unwrap().strips[0];
        let f = main.fader.as_ref().unwrap();
        assert_eq!(f.action, FaderAction::HostVolume);
        assert_eq!(f.level, Level::Db(-21.0));
        assert_eq!(f.owner.as_deref(), Some("cq"));
        assert_eq!(main.buttons[0].action, ButtonAction::HostMute);
        assert!(main.buttons[0].on);
    }

    #[test]
    fn the_default_spec_saves_channels_in_use_and_main() {
        let spec = default_spec(&speaker(), &MainTarget::OutputStage);
        assert_eq!(spec.id, "main");
        // 4 channels × (fader + mute) + Main (fader + mute).
        assert_eq!(spec.strips.len(), 10);
        let main: Vec<_> = spec.strips.iter().filter(|s| s.master).collect();
        assert_eq!(main.len(), 2);
        assert_eq!(main[0].control.action(), BindingAction::OutputGain);
        // Round trip: the saved panel looks like the default one.
        let src = Sources {
            status: &speaker(),
            router: None,
            surfaces: None,
            main: &MainTarget::OutputStage,
        };
        let p = from_spec(&spec, &src);
        assert_eq!(p.groups.len(), 2);
        assert_eq!(p.groups[0].strips.len(), 4);
        assert_eq!(p.groups[0].strips[0].label, "CQ Main L");
        assert_eq!(p.groups[0].strips[0].buttons.len(), 1);
        assert_eq!(p.groups[1].strips[0].label, "Main L/R");
    }

    #[test]
    fn a_saved_strip_whose_target_is_gone_is_kept_and_says_so() {
        let spec = MixerSpec::new(
            "p",
            "P",
            vec![MixerStrip::new(
                "x",
                StripControl::channel_fader(
                    BindingAction::Gain,
                    Target {
                        device: "main".into(),
                        direction: Direction::Rx,
                        channels: vec![9],
                    }
                    .set(),
                ),
            )],
        );
        let src = Sources {
            status: &speaker(),
            router: None,
            surfaces: None,
            main: &MainTarget::OutputStage,
        };
        let p = from_spec(&spec, &src);
        let s = &p.groups[0].strips[0];
        assert!(
            s.problem.as_deref().unwrap().contains("Not available"),
            "{s:?}"
        );
        assert_eq!(s.id, "strip-p-x");
    }

    #[test]
    fn target_words_name_runs_and_lists() {
        let t = |c: &[u16]| Target {
            device: "d".into(),
            direction: Direction::Rx,
            channels: c.to_vec(),
        };
        assert_eq!(t(&[3]).words(), "RX 3");
        assert_eq!(t(&[1, 2, 3]).words(), "RX 1\u{2013}3");
        assert_eq!(t(&[1, 3]).words(), "RX 1, 3");
        assert_eq!(t(&[1, 3]).channel_list(), "1,3");
    }
}
