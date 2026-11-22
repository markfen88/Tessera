//! [`SurfaceEvent`]s → [`SurfaceAction`]s.
//!
//! The mapper holds the per-binding state a surface needs and nothing else.
//! Its view of the device is **per channel** (`device`, `direction`,
//! `channel`), fed only by the daemon through [`Mapper::observe_mute`] and
//! [`Mapper::observe_gain`] (and, for the output stage of RX channels,
//! [`Mapper::observe_output_mute`] and [`Mapper::observe_output_gain`]); it
//! never assumes its own requests succeeded. A refused command (locked
//! device, unknown channel) therefore leaves the mapper's view correct, and
//! the next press acts on the real state.
//!
//! It decides; it does not apply. Each [`SurfaceAction`] is a request the
//! daemon turns into a typed command (or, for [`SurfaceAction::MoveFader`],
//! into output to the device).
//!
//! # Fader priority
//!
//! * A fader **without a motor** is authoritative while its surface is
//!   connected: the state engine refuses gain changes from other origins on
//!   its channels. It is engaged from the start: its first report is the
//!   baseline and applies the fader position at once (unless the gain is
//!   already there, within the deadband). A gain change made elsewhere
//!   cannot normally happen; if one is observed anyway (made while the
//!   surface was away, a configuration reload) the fader stays engaged, and
//!   its next movement beyond the deadband applies its position again.
//! * A **motorized** fader owns nothing. When the gain changes elsewhere
//!   (and at start-up) the mapper asks for the fader to be moved to match
//!   ([`SurfaceAction::MoveFader`]) and ignores its reports while the motor
//!   travels, until the fader reports a position within the deadband of the
//!   target, or passes it.
//! * Encoders own nothing; they step from the observed gain.
//!
//! # Contract with the daemon
//!
//! * Before the first event, observe the current mute and gain of every
//!   channel the surface's bindings cover, then send
//!   [`Mapper::startup_actions`].
//! * Observe every later change, with `own = true` when the change came from
//!   this surface's own commands (its origin), so a fader's late echoes do
//!   not count as a change made elsewhere.

use std::collections::HashMap;

use tessera_control_types::Direction;

use crate::config::{
    BindingAction, BindingConfig, ButtonMode, ControlKind, CrosspointKey, CurvePoint, FollowTarget,
    GAIN_CEILING_DB, Level, RouterPort, SurfaceConfig, Taper, Target,
};
use crate::decode::{SurfaceEvent, SurfaceEventKind};

/// What a `follow` binding sends to the control it drives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FollowValue {
    /// A fader's level, dB (`-inf`: off), by this binding's fader law.
    Fader { gain_db: f32 },
    /// A button's state (its light on the followed control).
    Button { on: bool },
}

/// A change the surface asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceAction {
    SetMute {
        target: Target,
        muted: bool,
    },
    /// `gain_db` is `f32::NEG_INFINITY` for "off" (fader at the bottom).
    SetGain {
        target: Target,
        gain_db: f32,
    },
    /// Mute the OUTPUT of RX channels (after the local mix):
    /// `Command::SetOutputMute`.
    SetOutputMute {
        target: Target,
        muted: bool,
    },
    /// OUTPUT level of RX channels (after the local mix):
    /// `Command::SetOutputGain`. `gain_db` as for [`SurfaceAction::SetGain`].
    SetOutputGain {
        target: Target,
        gain_db: f32,
    },
    /// Drive the motorized fader `control` to `value`, on the same scale as
    /// [`SurfaceEventKind::Fader`]: 0 is the bottom of travel, the
    /// control's `max` the top. For an `inverted` control the device wants
    /// `max - value`.
    MoveFader {
        control: String,
        value: u32,
    },
    /// `Command::SetHostVolume`; `gain_db` as for [`SurfaceAction::SetGain`].
    SetHostVolume {
        gain_db: f32,
    },
    /// `Command::SetHostMute`.
    SetHostMute {
        muted: bool,
    },
    /// `Command::SetPortGain`.
    SetPortGain {
        ports: Vec<RouterPort>,
        gain_db: f32,
    },
    /// `Command::SetPortMute`.
    SetPortMute {
        ports: Vec<RouterPort>,
        muted: bool,
    },
    /// `Command::SetCrosspointGain`.
    SetCrosspointGain {
        crosspoints: Vec<CrosspointKey>,
        gain_db: f32,
    },
    /// `Command::SetCrosspointMute`.
    SetCrosspointMute {
        crosspoints: Vec<CrosspointKey>,
        muted: bool,
    },
    /// Drive control `to` of another surface (a `follow` binding); the
    /// daemon turns the value into that control's position with its own
    /// fader law and writes it to that device.
    Follow {
        to: FollowTarget,
        value: FollowValue,
    },
}

impl SurfaceAction {
    /// The levels a level request sets (`None` for any other request).
    #[must_use]
    pub fn levels(&self) -> Option<Vec<Level>> {
        let chans = |output: bool, t: &Target| {
            t.channels
                .iter()
                .map(|&channel| Level::Channel {
                    output,
                    device: t.device.clone(),
                    direction: t.direction,
                    channel,
                })
                .collect()
        };
        Some(match self {
            Self::SetGain { target, .. } => chans(false, target),
            Self::SetOutputGain { target, .. } => chans(true, target),
            Self::SetHostVolume { .. } => vec![Level::Host],
            Self::SetPortGain { ports, .. } => ports.iter().map(Level::of_port).collect(),
            Self::SetCrosspointGain { crosspoints, .. } => {
                crosspoints.iter().cloned().map(Level::Crosspoint).collect()
            }
            _ => return None,
        })
    }
}

#[derive(Debug, Default)]
struct BindingState {
    /// Faders: gain the previous fader position meant.
    prev_db: Option<f32>,
    /// Faders: the previous raw position.
    prev_value: Option<u32>,
    /// Faders: the fader drives the gain. Only a motorized fader (moving to
    /// a gain set elsewhere, or after a refused request) is ever
    /// disengaged.
    engaged: bool,
    /// Gain bindings: last gain this binding asked for (or, for a fader that
    /// just took over, the gain it took over at), since the last change made
    /// elsewhere. The deadband and encoder steps count from it.
    sent_db: Option<f32>,
    /// Disengaged faders: the gain they take over at once they reach it.
    pickup_db: Option<f32>,
    /// Motorized faders: the position the motor was sent to.
    move_to: Option<u32>,
    /// Disengaged faders: the fader has been below the top of travel (by
    /// more than the deadband) since it disengaged, or was when it did, so
    /// reaching the top is a movement, not jitter at the end stop.
    left_top: bool,
    /// Momentary buttons: mute state before the press.
    saved_mute: Option<bool>,
    /// Toggle `follow` bindings of a press-and-release button: the state
    /// it last sent (each press flips it).
    follow_on: bool,
}

impl BindingState {
    fn fresh() -> Self {
        Self {
            engaged: true,
            ..Self::default()
        }
    }
}

/// Mapper for one surface.
#[derive(Debug)]
pub struct Mapper {
    bindings: Vec<BindingConfig>,
    /// Per binding: its control is a motorized fader.
    motorized: Vec<bool>,
    /// Per binding: its control is a desk's state parameter (an NRPN mute
    /// that reports on and off, not press and release): mute follows it.
    latching: Vec<bool>,
    /// Per binding: the fader's largest raw value (from the configuration,
    /// then from the latest report).
    fader_max: Vec<u32>,
    state: Vec<BindingState>,
    /// Per binding: [`BindingConfig::levels`].
    levels: Vec<Vec<Level>>,
    mute: HashMap<Level, bool>,
    gain: HashMap<Level, f32>,
}

fn approx(a: f32, b: f32, tol: f32) -> bool {
    a == b || (a - b).abs() <= tol
}

fn channel(output: bool, device: &str, direction: Direction, channel: u16) -> Level {
    Level::Channel {
        output,
        device: device.to_owned(),
        direction,
        channel,
    }
}

/// The request that mutes (or unmutes) what binding `b` acts on (`b` is a
/// mute binding).
#[must_use]
pub fn mute_request(b: &BindingConfig, muted: bool) -> SurfaceAction {
    match b.action {
        BindingAction::OutputMute => SurfaceAction::SetOutputMute {
            target: b.target.clone(),
            muted,
        },
        BindingAction::HostMute => SurfaceAction::SetHostMute { muted },
        BindingAction::PortMute => SurfaceAction::SetPortMute {
            ports: b.ports.clone(),
            muted,
        },
        BindingAction::CrosspointMute => SurfaceAction::SetCrosspointMute {
            crosspoints: b.crosspoints.clone(),
            muted,
        },
        BindingAction::Follow => match &b.follow {
            Some(to) => SurfaceAction::Follow {
                to: to.clone(),
                value: FollowValue::Button { on: muted },
            },
            None => SurfaceAction::SetMute {
                target: b.target.clone(),
                muted,
            },
        },
        _ => SurfaceAction::SetMute {
            target: b.target.clone(),
            muted,
        },
    }
}

/// The request that sets what binding `b` acts on to `gain_db` (`b` is a
/// level or a fader `follow` binding).
#[must_use]
pub fn level_request(b: &BindingConfig, gain_db: f32) -> SurfaceAction {
    match b.action {
        BindingAction::OutputGain => SurfaceAction::SetOutputGain {
            target: b.target.clone(),
            gain_db,
        },
        BindingAction::HostVolume => SurfaceAction::SetHostVolume { gain_db },
        BindingAction::PortGain => SurfaceAction::SetPortGain {
            ports: b.ports.clone(),
            gain_db,
        },
        BindingAction::CrosspointGain => SurfaceAction::SetCrosspointGain {
            crosspoints: b.crosspoints.clone(),
            gain_db,
        },
        BindingAction::Follow => match &b.follow {
            Some(to) => SurfaceAction::Follow {
                to: to.clone(),
                value: FollowValue::Fader { gain_db },
            },
            None => SurfaceAction::SetGain {
                target: b.target.clone(),
                gain_db,
            },
        },
        _ => SurfaceAction::SetGain {
            target: b.target.clone(),
            gain_db,
        },
    }
}

fn mute_action(b: &BindingConfig, muted: bool) -> SurfaceAction {
    mute_request(b, muted)
}

fn gain_action(b: &BindingConfig, gain_db: f32) -> SurfaceAction {
    level_request(b, gain_db)
}

/// A binding a fader drives: a level, or a fader following another.
fn drives_fader(a: BindingAction) -> bool {
    a.is_gain() || a == BindingAction::Follow
}

impl Mapper {
    /// A mapper for the bindings of `cfg` (expects a validated config).
    /// Every fader starts engaged: one without a motor applies its first
    /// position, a motorized one is moved by [`startup_actions`].
    ///
    /// [`startup_actions`]: Self::startup_actions
    #[must_use]
    pub fn new(cfg: &SurfaceConfig) -> Self {
        let mut bindings = cfg.bindings.clone();
        for b in &mut bindings {
            b.apply_curve_range();
        }
        let control = |b: &BindingConfig| cfg.controls.iter().find(|c| c.id == b.control);
        let motorized = bindings
            .iter()
            .map(|b| control(b).is_some_and(|c| c.kind == ControlKind::Fader && c.motorized))
            .collect();
        let latching = bindings
            .iter()
            .map(|b| control(b).is_some_and(|c| crate::midi::is_latching(c.kind, c.midi.as_ref())))
            .collect();
        let fader_max = bindings
            .iter()
            .map(|b| control(b).map_or(0, |c| c.max.unwrap_or_else(|| c.field_max())))
            .collect();
        let state = bindings.iter().map(|_| BindingState::fresh()).collect();
        let levels = bindings.iter().map(BindingConfig::levels).collect();
        Self {
            bindings,
            motorized,
            latching,
            fader_max,
            state,
            levels,
            mute: HashMap::new(),
            gain: HashMap::new(),
        }
    }

    /// Every value the bindings act on (the daemon observes these).
    #[must_use]
    pub fn covered(&self) -> Vec<Level> {
        let mut v: Vec<Level> = Vec::new();
        for l in self.levels.iter().flatten() {
            if !v.contains(l) {
                v.push(l.clone());
            }
        }
        v
    }

    /// Does a binding act on `level`?
    #[must_use]
    pub fn covers(&self, level: &Level) -> bool {
        self.levels.iter().any(|ls| ls.contains(level))
    }

    /// Requests to send when the surface starts (after the current mute and
    /// gain were observed): push-to-talk targets are muted until the button
    /// is held, motorized faders move to the gain of their target.
    pub fn startup_actions(&mut self) -> Vec<SurfaceAction> {
        let mut out: Vec<SurfaceAction> = self
            .bindings
            .iter()
            .filter(|b| b.action.is_mute() && b.mode == ButtonMode::PushToTalk)
            .map(|b| mute_action(b, true))
            .collect();
        for i in 0..self.bindings.len() {
            if !self.motorized[i] || !self.bindings[i].action.is_gain() {
                continue;
            }
            if let Some(g) = self.target_gain(i) {
                out.extend(self.move_fader(i, g));
            }
        }
        out
    }

    /// The device went away or was reopened: forget positions and pending
    /// state, so the next report is a new baseline (applied at once by a
    /// fader without a motor). Returns the requests that make the targets
    /// safe and the faders right: a held momentary button restores its
    /// pre-press state, push-to-talk targets are muted, motorized faders
    /// move to the current gain. Observations are kept.
    pub fn reset(&mut self) -> Vec<SurfaceAction> {
        let mut out = Vec::new();
        for (b, s) in self.bindings.iter().zip(&mut self.state) {
            if let Some(muted) = s.saved_mute.take() {
                out.push(mute_action(b, muted));
            }
            *s = BindingState::fresh();
        }
        out.extend(self.startup_actions());
        out
    }

    /// The daemon refused `action` (locked device, unknown channel): gain
    /// bindings on its target count from the observed gain again. A
    /// motorized fader is now away from the real gain and takes over again
    /// once it reaches it; a fader without a motor stays the authority and
    /// asks again on its next movement.
    pub fn rejected(&mut self, action: &SurfaceAction) {
        let Some(levels) = action.levels() else {
            return;
        };
        for i in 0..self.bindings.len() {
            let b = &self.bindings[i];
            if !b.action.is_gain() || self.levels[i] != levels {
                continue;
            }
            let observed = self.target_gain(i);
            let s = &mut self.state[i];
            s.sent_db = None;
            if self.motorized[i] {
                s.engaged = false;
                s.move_to = None;
                s.pickup_db = observed;
                // Already below the top: reaching it from here is a movement.
                s.left_top = s.prev_db.is_some_and(|p| p < b.max_db - b.deadband_db);
            }
        }
    }

    /// The daemon reports one channel's mute (from any origin).
    pub fn observe_mute(&mut self, device: &str, direction: Direction, ch: u16, muted: bool) {
        self.mute
            .insert(channel(false, device, direction, ch), muted);
    }

    /// The daemon reports the mute of any value (host volume, router port,
    /// crosspoint, channel), from any origin.
    pub fn observe_level_mute(&mut self, level: Level, muted: bool) {
        self.mute.insert(level, muted);
    }

    /// The daemon reports the gain of any value; otherwise as
    /// [`observe_gain`](Self::observe_gain).
    pub fn observe_level_gain(
        &mut self,
        level: Level,
        gain_db: f32,
        own: bool,
        out: &mut Vec<SurfaceAction>,
    ) {
        self.observe_key(level, gain_db, own, out);
    }

    /// The daemon reports whether one RX channel's OUTPUT is muted (from any
    /// origin), for [`BindingAction::OutputMute`] bindings.
    pub fn observe_output_mute(
        &mut self,
        device: &str,
        direction: Direction,
        ch: u16,
        muted: bool,
    ) {
        self.mute
            .insert(channel(true, device, direction, ch), muted);
    }

    /// The daemon reports one channel's gain. `own`: the change came from
    /// this surface's commands. A change made elsewhere that differs from
    /// what a binding last asked for (beyond its deadband) makes it count
    /// from the new gain; a motorized fader is asked to move there
    /// ([`SurfaceAction::MoveFader`], appended to `out`, once per move even
    /// when several channels report).
    pub fn observe_gain(
        &mut self,
        device: &str,
        direction: Direction,
        channel: u16,
        gain_db: f32,
        own: bool,
        out: &mut Vec<SurfaceAction>,
    ) {
        self.observe_level(false, device, direction, channel, gain_db, own, out);
    }

    /// The daemon reports one RX channel's OUTPUT level, for
    /// [`BindingAction::OutputGain`] bindings; otherwise as
    /// [`observe_gain`](Self::observe_gain).
    pub fn observe_output_gain(
        &mut self,
        device: &str,
        direction: Direction,
        channel: u16,
        gain_db: f32,
        own: bool,
        out: &mut Vec<SurfaceAction>,
    ) {
        self.observe_level(true, device, direction, channel, gain_db, own, out);
    }

    #[allow(clippy::too_many_arguments)]
    fn observe_level(
        &mut self,
        output: bool,
        device: &str,
        direction: Direction,
        ch: u16,
        gain_db: f32,
        own: bool,
        out: &mut Vec<SurfaceAction>,
    ) {
        self.observe_key(channel(output, device, direction, ch), gain_db, own, out);
    }

    fn observe_key(&mut self, level: Level, gain_db: f32, own: bool, out: &mut Vec<SurfaceAction>) {
        self.gain.insert(level.clone(), gain_db);
        if own {
            return;
        }
        for i in 0..self.bindings.len() {
            let b = &self.bindings[i];
            if !b.action.is_gain() || !self.levels[i].contains(&level) {
                continue;
            }
            let ours = self.state[i]
                .sent_db
                .is_some_and(|p| approx(p, gain_db, b.deadband_db));
            if ours {
                continue;
            }
            if self.motorized[i] {
                out.extend(self.move_fader(i, gain_db));
            } else {
                self.state[i].sent_db = None;
            }
        }
    }

    /// Sends motorized fader `i` to the position of `gain_db` and
    /// disengages it until it gets there. Nothing if it is already moving
    /// there, or already there (within one raw step, which the off-point
    /// hysteresis and converter jitter cannot tell apart).
    fn move_fader(&mut self, i: usize, gain_db: f32) -> Option<SurfaceAction> {
        let b = &self.bindings[i];
        let max = self.fader_max[i];
        if max == 0 {
            return None;
        }
        let value = fader_position(b, gain_db, max);
        let s = &mut self.state[i];
        if !s.engaged && s.move_to == Some(value) {
            return None;
        }
        if s.prev_value.is_some_and(|p| p.abs_diff(value) <= 1) {
            s.engaged = true;
            s.pickup_db = None;
            s.move_to = None;
            // Count from where the fader is.
            s.sent_db = s.prev_db;
            return None;
        }
        s.engaged = false;
        s.move_to = Some(value);
        s.pickup_db = Some(fader_db(b, value, max));
        s.sent_db = None;
        s.left_top = s.prev_db.is_some_and(|p| p < b.max_db - b.deadband_db);
        Some(SurfaceAction::MoveFader {
            control: b.control.clone(),
            value,
        })
    }

    /// Muted if every channel of the binding's target is, on the binding's
    /// stage (unknown channels count as unmuted).
    fn target_muted(&self, i: usize) -> bool {
        self.levels[i]
            .iter()
            .all(|k| self.mute.get(k).copied().unwrap_or(false))
    }

    /// The binding's target's gain on the binding's stage: its first channel
    /// with a known gain (a binding sets all of them alike).
    fn target_gain(&self, i: usize) -> Option<f32> {
        self.levels[i]
            .iter()
            .find_map(|k| self.gain.get(k).copied())
    }

    /// Handles one event, appending what it asks for to `out`.
    pub fn handle(&mut self, ev: &SurfaceEvent, out: &mut Vec<SurfaceAction>) {
        // Toggle bindings of one button flip together: decided once per
        // press (anything bound still live: mute all; else unmute all), so
        // targets that drifted apart are brought back in step.
        let toggle_to = match ev.kind {
            SurfaceEventKind::Button { pressed: true } => {
                let mut toggles = (0..self.bindings.len()).filter(|&i| {
                    let b = &self.bindings[i];
                    b.control == ev.control && b.action.is_mute() && b.mode == ButtonMode::Toggle
                });
                Some(toggles.any(|i| !self.target_muted(i)))
            }
            _ => None,
        };
        for i in 0..self.bindings.len() {
            if self.bindings[i].control != ev.control {
                continue;
            }
            let action = match ev.kind {
                SurfaceEventKind::Button { pressed } => self.button(i, pressed, toggle_to),
                SurfaceEventKind::Fader { value, max } => self.fader(i, value, max),
                SurfaceEventKind::Encoder { delta } => self.encoder(i, delta),
            };
            out.extend(action);
        }
    }

    /// A `follow` binding of another surface moved this control to `ev`
    /// (the daemon wrote the position to the device): handled as this
    /// control's own movement (the followed control acts as the control
    /// that follows it), its faders taking over at once: a motorized fader
    /// on its way to a gain set elsewhere is overruled by the move, as a
    /// hand on it would.
    pub fn driven(&mut self, ev: &SurfaceEvent, out: &mut Vec<SurfaceAction>) {
        if matches!(ev.kind, SurfaceEventKind::Fader { .. }) {
            for i in 0..self.bindings.len() {
                if self.bindings[i].control != ev.control || !drives_fader(self.bindings[i].action)
                {
                    continue;
                }
                let s = &mut self.state[i];
                if !s.engaged {
                    s.engaged = true;
                    s.pickup_db = None;
                    s.move_to = None;
                    s.sent_db = None;
                }
            }
        }
        self.handle(ev, out);
    }

    fn button(
        &mut self,
        i: usize,
        pressed: bool,
        toggle_to: Option<bool>,
    ) -> Option<SurfaceAction> {
        let b = &self.bindings[i];
        if b.action == BindingAction::Follow {
            // The followed button takes this one's state. A desk's state
            // parameter (an NRPN mute) is a state already; a button that
            // reports presses and releases is one in momentary or
            // push-to-talk mode (on while held), and in toggle mode flips
            // its own state on each press (starting off) and sends nothing
            // on release.
            let on = if self.latching[i] || b.mode != ButtonMode::Toggle {
                pressed
            } else if pressed {
                let s = &mut self.state[i];
                s.follow_on = !s.follow_on;
                s.follow_on
            } else {
                return None;
            };
            return Some(mute_action(b, on));
        }
        if !b.action.is_mute() {
            return None;
        }
        let current = self.target_muted(i);
        if self.latching[i] {
            // A state, not a press: "on" mutes (push-to-talk: "on" is live).
            // Nothing to ask for when every channel already is that way.
            let muted = pressed != (b.mode == ButtonMode::PushToTalk);
            let already = self.levels[i]
                .iter()
                .all(|k| self.mute.get(k).copied().unwrap_or(false) == muted);
            return (!already).then(|| mute_action(b, muted));
        }
        let s = &mut self.state[i];
        let muted = match (b.mode, pressed) {
            (ButtonMode::Toggle, true) => toggle_to.unwrap_or(!current),
            (ButtonMode::Toggle, false) => return None,
            (ButtonMode::Momentary, true) => {
                s.saved_mute = Some(current);
                true
            }
            (ButtonMode::Momentary, false) => s.saved_mute.take()?,
            (ButtonMode::PushToTalk, p) => !p,
        };
        Some(mute_action(b, muted))
    }

    fn fader(&mut self, i: usize, value: u32, max: u32) -> Option<SurfaceAction> {
        let b = &self.bindings[i];
        if !drives_fader(b.action) || max == 0 {
            return None;
        }
        self.fader_max[i] = max;
        let db = fader_db(b, value, max);
        let current = self.state[i].pickup_db.or_else(|| self.target_gain(i));
        let s = &mut self.state[i];
        let prev = s.prev_db;
        // Hysteresis at the off point: leaving off needs two raw steps of
        // travel (one on a one-bit fader) and the deadband above `min_db`,
        // so a reading flickering at the bottom can neither switch the
        // channel on and off nor count as passing a gain.
        let off_ok = value >= 2.min(max) && db >= b.min_db + b.deadband_db;
        let leaving_off = prev == Some(f32::NEG_INFINITY) && db.is_finite();
        // A reading that has not left off by the hysteresis is still off:
        // nothing moved, whatever the binding's state.
        if leaving_off && !off_ok {
            return None;
        }
        s.prev_db = Some(db);
        s.prev_value = Some(value);
        if !s.engaged {
            if db < b.max_db - b.deadband_db {
                s.left_top = true;
            }
            // Take over once the fader reaches the target, passes it between
            // two reports (closed interval: leaving a target it sat on counts
            // too), or (for a gain above `max_db`, which travel cannot reach)
            // comes back up to the top. Landing on the off point passes no
            // finite target: a motor overshooting to the bottom, or a
            // flicker there, must not cut the channel.
            let reached = match current {
                None => true,
                Some(cur) => {
                    approx(db, cur, b.deadband_db)
                        || ((db.is_finite() || !cur.is_finite())
                            && prev.is_some_and(|p| {
                                (p <= cur && cur <= db) || (db <= cur && cur <= p)
                            }))
                        || (s.left_top && db >= b.max_db && cur >= b.max_db)
                }
            };
            if !reached {
                return None;
            }
            s.engaged = true;
            s.pickup_db = None;
            s.move_to = None;
            // Jitter after taking over counts from the gain taken over at,
            // not from an observed gain travel cannot show.
            s.sent_db = current;
        }
        let at_end = db == f32::NEG_INFINITY || db >= b.max_db;
        let reference = s.sent_db.or(current);
        let small = reference.is_some_and(|r| approx(r, db, b.deadband_db));
        if small && !(at_end && reference != Some(db)) {
            // Compared with the last sent gain, so slow travel still adds up
            // past the deadband.
            return None;
        }
        s.sent_db = Some(db);
        Some(gain_action(b, db))
    }

    fn encoder(&mut self, i: usize, delta: i32) -> Option<SurfaceAction> {
        let b = &self.bindings[i];
        if !b.action.is_gain() || delta == 0 {
            return None;
        }
        let current = self.state[i].sent_db.or_else(|| self.target_gain(i));
        #[allow(clippy::cast_precision_loss)]
        let steps = delta as f32 * b.step_db;
        let from_bottom = (b.min_db + steps - b.step_db).min(b.max_db);
        let db = match current.filter(|c| c.is_finite()) {
            // Off or unknown: up starts at the bottom of the range.
            None if delta > 0 => from_bottom,
            None => return None,
            // Beyond the range (set elsewhere): never step the wrong way.
            Some(c) if delta > 0 && c >= b.max_db => return None,
            Some(c) if delta > 0 && c < b.min_db => from_bottom,
            Some(c) if delta > 0 => (c + steps).min(b.max_db),
            Some(c) if c + steps < b.min_db => f32::NEG_INFINITY,
            Some(c) => c + steps,
        };
        if current == Some(db) {
            return None;
        }
        self.state[i].sent_db = Some(db);
        Some(gain_action(b, db))
    }
}

/// Gain for a fader position. The bottom of travel is off, the top is
/// exactly `max_db`; the rest follows the binding's taper between `min_db`
/// and `max_db`.
#[must_use]
pub fn fader_db(b: &BindingConfig, value: u32, max: u32) -> f32 {
    if value == 0 || max == 0 {
        return f32::NEG_INFINITY;
    }
    if !b.curve.is_empty() {
        return curve_mdb(&b.curve, value) / 1000.0;
    }
    // Exact at the top: the taper arithmetic can round just below `max_db`
    // in f32, and the top must still read as the end of travel.
    if value >= max {
        return b.max_db.min(GAIN_CEILING_DB);
    }
    #[allow(clippy::cast_precision_loss)]
    let p = (value as f32) / (max as f32);
    let db = match b.taper {
        Taper::Db => b.min_db + p * (b.max_db - b.min_db),
        Taper::Linear => (b.max_db + 20.0 * p.log10()).max(b.min_db),
    };
    db.min(b.max_db).min(GAIN_CEILING_DB)
}

/// Fader position for a gain (the inverse of [`fader_db`]): 0 for off,
/// otherwise `1..=max`, clamped to the travel (a gain above `max_db` is the
/// top, one below `min_db` the lowest position that is not off).
#[must_use]
pub fn fader_position(b: &BindingConfig, gain_db: f32, max: u32) -> u32 {
    if max == 0 || gain_db.is_nan() || gain_db == f32::NEG_INFINITY {
        return 0;
    }
    if !b.curve.is_empty() {
        return curve_raw(&b.curve, gain_db * 1000.0).clamp(1, max);
    }
    let p = match b.taper {
        Taper::Db => (gain_db - b.min_db) / (b.max_db - b.min_db),
        Taper::Linear => 10f32.powf((gain_db - b.max_db) / 20.0),
    };
    #[allow(clippy::cast_precision_loss)]
    let v = (p.clamp(0.0, 1.0) * max as f32).round();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let v = v as u32;
    v.clamp(1, max)
}

/// milli-dB of raw position `value` on a fader law (ascending points,
/// linear between them, clamped to the first and last), as f32.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn curve_mdb(curve: &[CurvePoint], value: u32) -> f32 {
    let (Some(first), Some(last)) = (curve.first(), curve.last()) else {
        return f32::NEG_INFINITY;
    };
    let mdb = if value <= first.raw {
        f64::from(first.mdb)
    } else if value >= last.raw {
        f64::from(last.mdb)
    } else {
        let i = curve.partition_point(|p| p.raw <= value);
        let (a, b) = (curve[i - 1], curve[i]);
        let t = f64::from(value - a.raw) / f64::from(b.raw - a.raw);
        f64::from(a.mdb) + t * f64::from(b.mdb - a.mdb)
    };
    (mdb as f32).min(GAIN_CEILING_DB * 1000.0)
}

/// Raw position of `mdb` on a fader law (the inverse of [`curve_mdb`]),
/// clamped to the first and last point.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn curve_raw(curve: &[CurvePoint], mdb: f32) -> u32 {
    let (Some(first), Some(last)) = (curve.first(), curve.last()) else {
        return 0;
    };
    let mdb = f64::from(mdb);
    if mdb <= f64::from(first.mdb) {
        return first.raw;
    }
    if mdb >= f64::from(last.mdb) {
        return last.raw;
    }
    let i = curve.partition_point(|p| f64::from(p.mdb) <= mdb);
    let (a, b) = (curve[i - 1], curve[i]);
    let t = (mdb - f64::from(a.mdb)) / f64::from(b.mdb - a.mdb);
    (f64::from(a.raw) + t * f64::from(b.raw - a.raw)).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ControlConfig, SourceConfig};

    fn target() -> Target {
        Target {
            device: "main".into(),
            direction: Direction::Tx,
            channels: vec![1, 2],
        }
    }

    fn binding(control: &str, action: BindingAction) -> BindingConfig {
        BindingConfig {
            control: control.into(),
            action,
            target: target(),
            ports: Vec::new(),
            crosspoints: Vec::new(),
            follow: None,
            mode: ButtonMode::Toggle,
            min_db: -60.0,
            max_db: 0.0,
            taper: Taper::Db,
            step_db: 1.0,
            deadband_db: 0.25,
            curve: vec![],
        }
    }

    /// Controls `b` (button), `f` (fader, 0..=100), `e` (encoder); `f` has
    /// a motor if `motor`.
    fn mapper_with(bindings: Vec<BindingConfig>, motor: bool) -> Mapper {
        let control = |id: &str, kind, max| ControlConfig {
            id: id.into(),
            kind,
            report_id: None,
            offset: None,
            bits: None,
            max,
            inverted: false,
            active_low: false,
            motorized: motor && kind == ControlKind::Fader,
            code: Some("x".into()),
            midi: None,
        };
        Mapper::new(&SurfaceConfig {
            name: "t".into(),
            enabled: true,
            source: SourceConfig::Evdev { name: "t".into() },
            controls: vec![
                control("b", ControlKind::Button, None),
                control("f", ControlKind::Fader, Some(100)),
                control("e", ControlKind::Encoder, None),
            ],
            bindings,
        })
    }

    fn mapper(bindings: Vec<BindingConfig>) -> Mapper {
        mapper_with(bindings, false)
    }

    fn motorized() -> Mapper {
        mapper_with(vec![binding("f", BindingAction::Gain)], true)
    }

    fn run(m: &mut Mapper, control: &str, kind: SurfaceEventKind) -> Vec<SurfaceAction> {
        let mut out = vec![];
        m.handle(
            &SurfaceEvent {
                control: control.into(),
                kind,
            },
            &mut out,
        );
        out
    }

    fn press(m: &mut Mapper, pressed: bool) -> Vec<SurfaceAction> {
        run(m, "b", SurfaceEventKind::Button { pressed })
    }

    fn fader(m: &mut Mapper, value: u32) -> Vec<SurfaceAction> {
        run(m, "f", SurfaceEventKind::Fader { value, max: 100 })
    }

    fn enc(m: &mut Mapper, delta: i32) -> Vec<SurfaceAction> {
        run(m, "e", SurfaceEventKind::Encoder { delta })
    }

    fn mute(muted: bool) -> SurfaceAction {
        SurfaceAction::SetMute {
            target: target(),
            muted,
        }
    }

    fn gain(gain_db: f32) -> SurfaceAction {
        SurfaceAction::SetGain {
            target: target(),
            gain_db,
        }
    }

    fn move_to(value: u32) -> SurfaceAction {
        SurfaceAction::MoveFader {
            control: "f".into(),
            value,
        }
    }

    /// The daemon applies an action and echoes it back per channel; returns
    /// what the mapper asks for in response (motor moves).
    fn echo(m: &mut Mapper, a: &SurfaceAction, own: bool) -> Vec<SurfaceAction> {
        let mut out = vec![];
        match a {
            SurfaceAction::SetMute { target, muted } => {
                for &c in &target.channels {
                    m.observe_mute(&target.device, target.direction, c, *muted);
                }
            }
            SurfaceAction::SetGain { target, gain_db } => {
                for &c in &target.channels {
                    m.observe_gain(&target.device, target.direction, c, *gain_db, own, &mut out);
                }
            }
            SurfaceAction::SetOutputMute { target, muted } => {
                for &c in &target.channels {
                    m.observe_output_mute(&target.device, target.direction, c, *muted);
                }
            }
            SurfaceAction::SetOutputGain { target, gain_db } => {
                for &c in &target.channels {
                    m.observe_output_gain(
                        &target.device,
                        target.direction,
                        c,
                        *gain_db,
                        own,
                        &mut out,
                    );
                }
            }
            _ => {}
        }
        out
    }

    fn set_gain_elsewhere(m: &mut Mapper, db: f32) -> Vec<SurfaceAction> {
        echo(m, &gain(db), false)
    }

    // ---- before the device reports -----------------------------------------

    /// At connect the daemon observes every covered channel (gain, mute)
    /// and sends the start-up requests. A control that has not reported has
    /// no value: nothing is asked for its channels from a default position
    /// (no gain from a fader "at 0", no mute from a button "released").
    #[test]
    fn nothing_is_asked_for_a_control_that_has_not_reported() {
        for (gain_db, muted) in [(0.0, false), (-20.0, true), (f32::NEG_INFINITY, false)] {
            let mut m = mapper(vec![
                binding("b", BindingAction::Mute),
                binding("f", BindingAction::Gain),
                binding("e", BindingAction::Gain),
            ]);
            let mut out = vec![];
            for c in [1, 2] {
                m.observe_mute("main", Direction::Tx, c, muted);
                m.observe_gain("main", Direction::Tx, c, gain_db, false, &mut out);
            }
            out.extend(m.startup_actions());
            assert!(out.is_empty(), "{gain_db} dB: {out:?}");
            // A reconnect (the device lost and found again) asks nothing
            // either.
            assert!(m.reset().is_empty(), "{gain_db} dB");
        }
    }

    /// A motorized fader is moved to the gain at start (the gain owns it);
    /// the move's arrival, which the daemon hands back because a desk need
    /// not echo it, never sets the gain.
    #[test]
    fn a_motorized_fader_moves_at_start_and_its_arrival_sets_nothing() {
        for gain_db in [0.0, -30.0, -75.0, 6.0, f32::NEG_INFINITY] {
            let mut m = motorized();
            let mut out = vec![];
            for c in [1, 2] {
                m.observe_gain("main", Direction::Tx, c, gain_db, false, &mut out);
            }
            out.extend(m.startup_actions());
            let moves: Vec<u32> = out
                .iter()
                .map(|a| match a {
                    SurfaceAction::MoveFader { value, .. } => *value,
                    other => panic!("{gain_db} dB: only moves at start: {other:?}"),
                })
                .collect();
            assert_eq!(moves.len(), 1, "{gain_db} dB: {out:?}");
            assert!(fader(&mut m, moves[0]).is_empty(), "{gain_db} dB");
        }
    }

    // ---- buttons ----------------------------------------------------------

    #[test]
    fn toggle_acts_on_observed_state_only() {
        let mut m = mapper(vec![binding("b", BindingAction::Mute)]);
        let a = press(&mut m, true);
        assert_eq!(a, vec![mute(true)]);
        // Refused (no echo): the next press asks again.
        assert!(press(&mut m, false).is_empty());
        assert_eq!(press(&mut m, true), vec![mute(true)]);
        echo(&mut m, &a[0], true);
        assert_eq!(press(&mut m, true), vec![mute(false)]);
    }

    #[test]
    fn toggle_with_partially_muted_target_mutes_all() {
        let mut m = mapper(vec![binding("b", BindingAction::Mute)]);
        m.observe_mute("main", Direction::Tx, 1, true);
        assert_eq!(
            press(&mut m, true),
            vec![mute(true)],
            "channel 2 still live"
        );
    }

    #[test]
    fn momentary_restores_previous_state() {
        let mut b = binding("b", BindingAction::Mute);
        b.mode = ButtonMode::Momentary;
        let mut m = mapper(vec![b]);
        assert_eq!(press(&mut m, true), vec![mute(true)]);
        assert_eq!(press(&mut m, false), vec![mute(false)]);
        // Muted from the portal: a cough must not unmute it.
        echo(&mut m, &mute(true), false);
        assert_eq!(press(&mut m, true), vec![mute(true)]);
        assert_eq!(press(&mut m, false), vec![mute(true)]);
        // Release without a seen press (surface started while held): nothing.
        assert!(press(&mut m, false).is_empty());
    }

    #[test]
    fn push_to_talk_starts_muted() {
        let mut b = binding("b", BindingAction::Mute);
        b.mode = ButtonMode::PushToTalk;
        let mut m = mapper(vec![b]);
        assert_eq!(m.startup_actions(), vec![mute(true)]);
        assert_eq!(press(&mut m, true), vec![mute(false)]);
        assert_eq!(press(&mut m, false), vec![mute(true)]);
        assert!(
            mapper(vec![binding("b", BindingAction::Mute)])
                .startup_actions()
                .is_empty()
        );
    }

    #[test]
    fn one_button_several_targets() {
        let mut rx = binding("b", BindingAction::Mute);
        rx.target.direction = Direction::Rx;
        let mut m = mapper(vec![binding("b", BindingAction::Mute), rx]);
        assert_eq!(press(&mut m, true).len(), 2);
    }

    #[test]
    fn one_button_brings_drifted_targets_back_in_step() {
        let mut rx = binding("b", BindingAction::Mute);
        rx.target.direction = Direction::Rx;
        let rx_target = rx.target.clone();
        let mut m = mapper(vec![binding("b", BindingAction::Mute), rx]);
        echo(&mut m, &mute(true), false); // TX muted from the portal, RX live
        let a = press(&mut m, true);
        let rx_mute = |muted| SurfaceAction::SetMute {
            target: rx_target.clone(),
            muted,
        };
        assert_eq!(
            a,
            vec![mute(true), rx_mute(true)],
            "anything live: mute all"
        );
        for x in &a {
            echo(&mut m, x, true);
        }
        assert_eq!(press(&mut m, true), vec![mute(false), rx_mute(false)]);
    }

    // ---- fader curve -----------------------------------------------------

    #[test]
    fn fader_db_taper_ends_and_middle() {
        let b = binding("f", BindingAction::Gain);
        assert_eq!(fader_db(&b, 0, 100), f32::NEG_INFINITY);
        assert_eq!(fader_db(&b, 100, 100), 0.0);
        assert_eq!(fader_db(&b, 150, 100), 0.0, "above max is the top");
        assert!((fader_db(&b, 50, 100) + 30.0).abs() < 1e-4);
        let mut l = b.clone();
        l.taper = Taper::Linear;
        assert!((fader_db(&l, 50, 100) + 6.0206).abs() < 1e-3);
        assert_eq!(fader_db(&l, 1, 100_000), -60.0, "floored at min_db");
    }

    #[test]
    fn the_top_of_travel_is_exactly_max_db() {
        // In f32, -60 + (4095/4095) * 60.1 rounds to 0.099998.
        let mut b = binding("f", BindingAction::Gain);
        b.max_db = 0.1;
        assert_eq!(fader_db(&b, 4095, 4095), 0.1);
        assert_eq!(fader_db(&b, 100, 100), 0.1);
        // A disengaged fader that comes back up to the top takes over there.
        let mut m = mapper_with(vec![b], true);
        let at = |m: &mut Mapper, value| run(m, "f", SurfaceEventKind::Fader { value, max: 4095 });
        let a = at(&mut m, 2000);
        assert_eq!(a.len(), 1);
        m.observe_gain("main", Direction::Tx, 1, 6.0, true, &mut vec![]);
        m.rejected(&a[0]); // takes over again at +6 dB: only the top reaches it
        assert!(at(&mut m, 3000).is_empty());
        assert_eq!(
            at(&mut m, 4095),
            vec![gain(0.1)],
            "the top engages and is sent"
        );
    }

    #[test]
    fn fader_position_inverts_fader_db() {
        let b = binding("f", BindingAction::Gain);
        assert_eq!(fader_position(&b, f32::NEG_INFINITY, 100), 0);
        assert_eq!(fader_position(&b, 0.0, 100), 100);
        assert_eq!(fader_position(&b, 6.0, 100), 100, "above the range: top");
        assert_eq!(fader_position(&b, -80.0, 100), 1, "below: lowest not off");
        assert_eq!(fader_position(&b, -30.0, 100), 50);
        assert_eq!(fader_position(&b, -12.0, 4095), 3276);
        let mut l = b.clone();
        l.taper = Taper::Linear;
        for v in [1u32, 7, 50, 99, 100] {
            let db = fader_db(&l, v, 100);
            if db > l.min_db {
                assert_eq!(fader_position(&l, db, 100), v, "linear {v}");
            }
        }
        for v in [1u32, 2, 33, 50, 99, 100] {
            assert_eq!(fader_position(&b, fader_db(&b, v, 100), 100), v, "db {v}");
        }
        assert_eq!(fader_position(&b, -12.0, 0), 0);
    }

    // ---- faders without a motor: authoritative ------------------------------

    #[test]
    fn authoritative_baseline_applies_at_once() {
        let mut m = mapper(vec![binding("f", BindingAction::Gain)]);
        set_gain_elsewhere(&mut m, -12.0); // the saved gain
        assert_eq!(fader(&mut m, 20), vec![gain(-48.0)], "first report applies");
        assert_eq!(fader(&mut m, 30), vec![gain(-42.0)]);

        let mut m = mapper(vec![binding("f", BindingAction::Gain)]);
        set_gain_elsewhere(&mut m, -12.1);
        assert!(fader(&mut m, 80).is_empty(), "already there (deadband)");

        let mut m = mapper(vec![binding("f", BindingAction::Gain)]);
        set_gain_elsewhere(&mut m, 6.0);
        assert_eq!(fader(&mut m, 100), vec![gain(0.0)], "parked at the top");
        let mut m = mapper(vec![binding("f", BindingAction::Gain)]);
        set_gain_elsewhere(&mut m, -80.0);
        assert_eq!(
            fader(&mut m, 0),
            vec![gain(f32::NEG_INFINITY)],
            "parked at the bottom"
        );
    }

    #[test]
    fn unknown_gain_engages_at_once() {
        let mut m = mapper(vec![binding("f", BindingAction::Gain)]);
        assert_eq!(fader(&mut m, 50), vec![gain(-30.0)]);
    }

    #[test]
    fn authoritative_change_elsewhere_does_not_disengage() {
        let mut m = mapper(vec![binding("f", BindingAction::Gain)]);
        assert_eq!(fader(&mut m, 20), vec![gain(-48.0)]);
        assert!(
            set_gain_elsewhere(&mut m, -12.0).is_empty(),
            "no motor to move"
        );
        assert_eq!(
            fader(&mut m, 21),
            vec![gain(-47.4)],
            "the next movement applies the fader again"
        );
    }

    #[test]
    fn fader_deadband_but_ends_always_sent() {
        let mut b = binding("f", BindingAction::Gain);
        b.deadband_db = 1.0;
        let mut m = mapper(vec![b]);
        assert_eq!(fader(&mut m, 50).len(), 1);
        assert!(fader(&mut m, 51).is_empty(), "0.6 dB < 1 dB deadband");
        assert_eq!(fader(&mut m, 52).len(), 1, "1.2 dB from the last sent");
        assert_eq!(fader(&mut m, 0), vec![gain(f32::NEG_INFINITY)]);
        assert_eq!(fader(&mut m, 100), vec![gain(0.0)]);
    }

    #[test]
    fn late_own_echoes_keep_fader_engaged() {
        let mut m = mapper(vec![binding("f", BindingAction::Gain)]);
        let a = fader(&mut m, 50); // -30
        fader(&mut m, 51);
        fader(&mut m, 52); // -28.8 sent
        echo(&mut m, &a[0], true); // -30 arrives late
        assert_eq!(fader(&mut m, 53).len(), 1, "still engaged");
    }

    #[test]
    fn reset_rebaselines_and_makes_safe() {
        let mut m = mapper(vec![binding("f", BindingAction::Gain)]);
        let a = fader(&mut m, 80); // -12
        echo(&mut m, &a[0], true);
        assert!(m.reset().is_empty());
        // Replugged with the fader moved: its position applies at once.
        assert_eq!(fader(&mut m, 10), vec![gain(-54.0)]);

        let mut c = binding("b", BindingAction::Mute);
        c.mode = ButtonMode::Momentary;
        let mut p = binding("b", BindingAction::Mute);
        p.mode = ButtonMode::PushToTalk;
        p.target.direction = Direction::Rx;
        let mut m = mapper(vec![c, p]);
        press(&mut m, true); // cough held, talk held
        let rx_mute = SurfaceAction::SetMute {
            target: Target {
                direction: Direction::Rx,
                ..target()
            },
            muted: true,
        };
        assert_eq!(
            m.reset(),
            vec![mute(false), rx_mute.clone()],
            "restore cough, close talk"
        );
        // The held buttons are released after the replug: talk mutes again,
        // the cough has nothing to restore.
        assert_eq!(press(&mut m, false), vec![rx_mute]);
    }

    #[test]
    fn rejected_authoritative_fader_asks_again() {
        let mut m = mapper(vec![binding("f", BindingAction::Gain)]);
        let a = fader(&mut m, 80); // -12
        echo(&mut m, &a[0], true);
        let a = fader(&mut m, 100); // 0 dB, refused (locked)
        m.rejected(&a[0]);
        // Still the authority: counts from the observed -12 dB.
        let a = fader(&mut m, 99);
        assert!(
            matches!(a.as_slice(), [SurfaceAction::SetGain { gain_db, .. }] if (gain_db + 0.6).abs() < 1e-4),
            "{a:?}"
        );
    }

    #[test]
    fn flicker_at_the_bottom_does_not_flood() {
        let mut m = mapper(vec![binding("f", BindingAction::Gain)]);
        assert_eq!(fader(&mut m, 0), vec![gain(f32::NEG_INFINITY)]);
        for _ in 0..5 {
            assert!(
                fader(&mut m, 1).is_empty(),
                "one raw step is not leaving off"
            );
            assert!(fader(&mut m, 0).is_empty());
        }
        assert_eq!(fader(&mut m, 2).len(), 1, "two steps leave off");
    }

    #[test]
    fn bottom_flicker_after_a_change_elsewhere_sends_nothing() {
        let mut m = mapper(vec![binding("f", BindingAction::Gain)]);
        let at = |m: &mut Mapper, value| run(m, "f", SurfaceEventKind::Fader { value, max: 4095 });
        assert_eq!(at(&mut m, 0), vec![gain(f32::NEG_INFINITY)]);
        set_gain_elsewhere(&mut m, -30.0);
        assert!(at(&mut m, 1).is_empty(), "flicker is still off");
    }

    #[test]
    fn one_bit_fader_can_leave_off() {
        let mut m = mapper(vec![binding("f", BindingAction::Gain)]);
        let at = |m: &mut Mapper, value| run(m, "f", SurfaceEventKind::Fader { value, max: 1 });
        assert_eq!(at(&mut m, 0), vec![gain(f32::NEG_INFINITY)]);
        assert_eq!(at(&mut m, 1), vec![gain(0.0)]);
    }

    // ---- motorized faders --------------------------------------------------

    #[test]
    fn motorized_startup_moves_the_fader_and_waits() {
        let mut m = motorized();
        assert!(
            m.startup_actions().is_empty(),
            "gain unknown: nothing to show"
        );
        let mut m = motorized();
        echo(&mut m, &gain(-12.0), true); // observed before start
        assert_eq!(m.startup_actions(), vec![move_to(80)]);
        assert!(fader(&mut m, 30).is_empty(), "the motor travels");
        assert!(fader(&mut m, 60).is_empty());
        assert!(fader(&mut m, 80).is_empty(), "arrived: nothing to send");
        assert!(fader(&mut m, 80).is_empty());
        assert_eq!(fader(&mut m, 85), vec![gain(-9.0)], "the user moves it");
    }

    #[test]
    fn motorized_change_elsewhere_moves_once_and_waits() {
        let mut m = motorized();
        assert_eq!(fader(&mut m, 20), vec![gain(-48.0)]);
        assert_eq!(
            set_gain_elsewhere(&mut m, -12.0),
            vec![move_to(80)],
            "one move for both channels"
        );
        assert!(fader(&mut m, 40).is_empty());
        assert!(fader(&mut m, 60).is_empty());
        assert!(fader(&mut m, 79).is_empty(), "-12.6 dB: not there yet");
        assert!(fader(&mut m, 80).is_empty(), "there: takes over silently");
        assert_eq!(fader(&mut m, 85), vec![gain(-9.0)]);
    }

    #[test]
    fn a_followed_move_acts_at_once_even_on_a_motor_moving_elsewhere() {
        let driven = |m: &mut Mapper, value: u32| {
            let mut out = vec![];
            m.driven(
                &SurfaceEvent {
                    control: "f".into(),
                    kind: SurfaceEventKind::Fader { value, max: 100 },
                },
                &mut out,
            );
            out
        };
        let mut m = motorized();
        assert_eq!(fader(&mut m, 20), vec![gain(-48.0)]);
        // A gain set elsewhere sends the motor away: a hand move would wait
        // for it to arrive, a followed move does not.
        assert_eq!(set_gain_elsewhere(&mut m, -12.0), vec![move_to(80)]);
        assert_eq!(driven(&mut m, 50), vec![gain(-30.0)]);
        // Then as any movement: the deadband, and the hand takes over.
        assert!(driven(&mut m, 50).is_empty());
        assert_eq!(fader(&mut m, 60), vec![gain(-24.0)]);
        // A button: as pressed.
        let mut m = mapper(vec![binding("b", BindingAction::Mute)]);
        let mut out = vec![];
        m.driven(
            &SurfaceEvent {
                control: "b".into(),
                kind: SurfaceEventKind::Button { pressed: true },
            },
            &mut out,
        );
        assert_eq!(out, vec![mute(true)]);
    }

    #[test]
    fn motorized_partial_change_elsewhere_moves() {
        let mut m = motorized();
        fader(&mut m, 20);
        let mut out = vec![];
        m.observe_gain("main", Direction::Tx, 2, -12.0, false, &mut out);
        assert_eq!(out, vec![move_to(80)]);
        assert!(fader(&mut m, 30).is_empty(), "waits for the motor");
    }

    #[test]
    fn motorized_already_there_does_not_move() {
        let mut m = motorized();
        assert_eq!(fader(&mut m, 80), vec![gain(-12.0)]);
        assert!(set_gain_elsewhere(&mut m, -12.3).is_empty());
        assert_eq!(fader(&mut m, 85), vec![gain(-9.0)], "still engaged");
    }

    #[test]
    fn motorized_targets_beyond_the_range_park_at_the_ends() {
        let mut m = motorized();
        fader(&mut m, 50);
        assert_eq!(set_gain_elsewhere(&mut m, 6.0), vec![move_to(100)]);
        assert!(fader(&mut m, 90).is_empty());
        assert!(fader(&mut m, 100).is_empty(), "at the top: +6 dB stays");
        assert_eq!(fader(&mut m, 95), vec![gain(-3.0)]);

        let mut m = motorized();
        fader(&mut m, 50);
        assert_eq!(set_gain_elsewhere(&mut m, -80.0), vec![move_to(1)]);
        assert!(fader(&mut m, 20).is_empty());
        assert!(fader(&mut m, 1).is_empty(), "lowest step: -80 dB stays");

        let mut m = motorized();
        fader(&mut m, 50);
        assert_eq!(
            set_gain_elsewhere(&mut m, f32::NEG_INFINITY),
            vec![move_to(0)]
        );
        assert!(fader(&mut m, 10).is_empty());
        assert!(fader(&mut m, 0).is_empty(), "off and at the bottom");
        assert_eq!(fader(&mut m, 10).len(), 1, "the user brings it up");
    }

    #[test]
    fn motorized_parked_at_top_with_gain_above_range_does_not_cut() {
        let mut m = motorized();
        let at = |m: &mut Mapper, value| run(m, "f", SurfaceEventKind::Fader { value, max: 4095 });
        assert_eq!(at(&mut m, 4095), vec![gain(0.0)]);
        assert!(
            set_gain_elsewhere(&mut m, 6.0).is_empty(),
            "already at the top"
        );
        assert!(at(&mut m, 4094).is_empty(), "-0.015 dB: jitter");
        assert!(at(&mut m, 4095).is_empty());
        assert_eq!(at(&mut m, 3800).len(), 1, "a real movement");
    }

    #[test]
    fn motorized_bottom_flicker_does_not_engage_or_cut() {
        let mut m = motorized();
        let at = |m: &mut Mapper, value| run(m, "f", SurfaceEventKind::Fader { value, max: 4095 });
        assert_eq!(at(&mut m, 2000).len(), 1);
        assert_eq!(set_gain_elsewhere(&mut m, -80.0), vec![move_to(1)]);
        assert!(at(&mut m, 0).is_empty(), "overshot to the bottom");
        assert!(at(&mut m, 1).is_empty(), "flicker is still off");
        assert!(at(&mut m, 0).is_empty(), "no cut to off");
        assert_eq!(at(&mut m, 200).len(), 1, "real travel passes it");
    }

    #[test]
    fn rejected_motorized_fader_waits_for_the_real_gain() {
        let mut m = motorized();
        let a = fader(&mut m, 80); // -12
        echo(&mut m, &a[0], true);
        let a = fader(&mut m, 100); // 0 dB, refused (locked)
        m.rejected(&a[0]);
        assert!(fader(&mut m, 99).is_empty(), "no jump: waits for -12 dB");
        assert_eq!(fader(&mut m, 79).len(), 1, "crossed -12 dB");
    }

    #[test]
    fn rejected_at_the_bottom_takes_over_moving_up() {
        // A disengaged fader whose target is -inf and whose previous
        // reading is -inf: leaving the target upwards passes it (closed
        // interval), so the fader is not dead.
        let mut m = motorized();
        let off = fader(&mut m, 0);
        assert_eq!(off, vec![gain(f32::NEG_INFINITY)]);
        echo(&mut m, &off[0], true);
        m.rejected(&off[0]);
        assert_eq!(
            fader(&mut m, 30),
            vec![gain(-42.0)],
            "moving up from the target engages"
        );
    }

    #[test]
    fn motorized_reset_moves_to_the_current_gain() {
        let mut m = motorized();
        let a = fader(&mut m, 80);
        echo(&mut m, &a[0], true);
        assert_eq!(m.reset(), vec![move_to(80)]);
        assert!(fader(&mut m, 10).is_empty(), "replug: waits for the motor");
        assert!(fader(&mut m, 80).is_empty());
    }

    // ---- encoders ----------------------------------------------------------

    #[test]
    fn rejected_gain_counts_from_observed_again() {
        let mut m = mapper(vec![binding("e", BindingAction::Gain)]);
        set_gain_elsewhere(&mut m, -20.0);
        let a = enc(&mut m, 5); // -15, refused
        m.rejected(&a[0]);
        assert_eq!(enc(&mut m, 1), vec![gain(-19.0)]);
    }

    #[test]
    fn encoder_steps_clamps_and_turns_off() {
        let mut b = binding("e", BindingAction::Gain);
        b.min_db = -3.0;
        let mut m = mapper(vec![b]);
        assert!(enc(&mut m, -1).is_empty(), "down from unknown does nothing");
        assert_eq!(enc(&mut m, 1), vec![gain(-3.0)]);
        // Steps add up before any echo arrives.
        assert_eq!(enc(&mut m, 1), vec![gain(-2.0)]);
        assert_eq!(enc(&mut m, 10), vec![gain(0.0)]);
        assert!(enc(&mut m, 1).is_empty(), "already at max");
        assert_eq!(enc(&mut m, -4), vec![gain(f32::NEG_INFINITY)]);
    }

    #[test]
    fn encoder_never_steps_the_wrong_way_beyond_range() {
        let mut m = mapper(vec![binding("e", BindingAction::Gain)]);
        set_gain_elsewhere(&mut m, -80.0);
        assert_eq!(enc(&mut m, 1), vec![gain(-60.0)], "up from below the range");
        set_gain_elsewhere(&mut m, 6.0);
        assert!(
            enc(&mut m, 1).is_empty(),
            "up from above the range does not cut"
        );
        assert_eq!(
            enc(&mut m, -1),
            vec![gain(5.0)],
            "down steps from where it is"
        );
    }

    fn cq_curve() -> Vec<CurvePoint> {
        crate::presets::allen_heath_cq(1).1
    }

    #[test]
    fn curve_interpolates_clamps_and_inverts() {
        let mut b = binding("f", BindingAction::Gain);
        b.curve = cq_curve();
        b.apply_curve_range();
        assert_eq!((b.min_db, b.max_db), (-89.0, 10.0));
        let max = 16320;
        assert_eq!(fader_db(&b, 0, max), f32::NEG_INFINITY, "raw 0 is off");
        assert_eq!(fader_db(&b, 12544, max), 0.0);
        assert_eq!(fader_db(&b, 16320, max), 10.0);
        assert_eq!(fader_db(&b, 16383, max), 10.0, "clamped at the top");
        assert_eq!(fader_db(&b, 100, max), -89.0, "clamped at the bottom");
        assert!(
            (fader_db(&b, 13664, max) - 2.5).abs() < 0.001,
            "linear between"
        );
        assert_eq!(fader_position(&b, 0.0, max), 12544);
        assert_eq!(fader_position(&b, 2.5, max), 13664);
        assert_eq!(fader_position(&b, 12.0, max), 16320);
        assert_eq!(fader_position(&b, -120.0, max), 192);
        assert_eq!(fader_position(&b, f32::NEG_INFINITY, max), 0);
        assert_eq!(fader_position(&b, 0.0, 1000), 1000, "clamped to the travel");
        for raw in [192u32, 200, 777, 3000, 9000, 12544, 15000, 16320] {
            let back = fader_position(&b, fader_db(&b, raw, max), max);
            assert!(back.abs_diff(raw) <= 1, "{raw} -> {back}");
        }
    }

    fn midi_mapper(motorized: bool, mode: ButtonMode) -> (Mapper, crate::midi::MidiDecoder) {
        use crate::config::{MidiAddress, MidiKind};
        let control = |id: &str, kind, number, max| ControlConfig {
            id: id.into(),
            kind,
            report_id: None,
            offset: None,
            bits: None,
            max,
            inverted: false,
            active_low: false,
            motorized: motorized && kind == ControlKind::Fader,
            code: None,
            midi: Some(MidiAddress::new(MidiKind::Nrpn, 1, number)),
        };
        let mut fader = binding("f", BindingAction::Gain);
        fader.curve = cq_curve();
        let mut mute = binding("m", BindingAction::Mute);
        mute.mode = mode;
        let cfg = SurfaceConfig {
            name: "cq".into(),
            enabled: true,
            source: SourceConfig::Midi {
                card: "CQ".into(),
                device: 0,
            },
            controls: vec![
                control("f", ControlKind::Fader, 0x40 << 7, Some(16320)),
                control("m", ControlKind::Button, 0, None),
            ],
            bindings: vec![fader, mute],
        };
        cfg.validate().unwrap();
        (Mapper::new(&cfg), crate::midi::MidiDecoder::new(&cfg))
    }

    fn feed_midi(
        m: &mut Mapper,
        d: &mut crate::midi::MidiDecoder,
        bytes: &[u8],
    ) -> Vec<SurfaceAction> {
        let mut ev = vec![];
        d.feed(bytes, &mut ev);
        let mut out = vec![];
        for e in &ev {
            m.handle(e, &mut out);
        }
        out
    }

    #[test]
    fn latching_mute_follows_the_desk_state() {
        use crate::midi::encode_nrpn;
        let (mut m, mut d) = midi_mapper(false, ButtonMode::Toggle);
        for c in [1, 2] {
            m.observe_mute("main", Direction::Tx, c, false);
        }
        // The first message on the channel also yields VC 0 (unmuted): no-op.
        assert_eq!(
            feed_midi(&mut m, &mut d, &encode_nrpn(1, 0, 1)),
            vec![mute(true)]
        );
        for c in [1, 2] {
            m.observe_mute("main", Direction::Tx, c, true);
        }
        assert!(
            feed_midi(&mut m, &mut d, &encode_nrpn(1, 0, 1)).is_empty(),
            "already"
        );
        assert_eq!(
            feed_midi(&mut m, &mut d, &encode_nrpn(1, 0, 0)),
            vec![mute(false)]
        );
        // Muted elsewhere (and fed back to the desk): the desk's unmute counts.
        for c in [1, 2] {
            m.observe_mute("main", Direction::Tx, c, true);
        }
        assert_eq!(
            feed_midi(&mut m, &mut d, &encode_nrpn(1, 0, 0)),
            vec![mute(false)]
        );

        let (mut m, mut d) = midi_mapper(false, ButtonMode::PushToTalk);
        feed_midi(&mut m, &mut d, &encode_nrpn(1, 0x40 << 7, 12544));
        assert!(
            feed_midi(&mut m, &mut d, &encode_nrpn(1, 0, 1)).is_empty(),
            "push-to-talk: on is live, as it already is"
        );
        assert_eq!(
            feed_midi(&mut m, &mut d, &encode_nrpn(1, 0, 0)),
            vec![mute(true)]
        );
    }

    #[test]
    fn motorized_midi_fader_takes_over_once_moved_there() {
        use crate::midi::encode_nrpn;
        let (mut m, mut d) = midi_mapper(true, ButtonMode::Toggle);
        let mut out = vec![];
        for c in [1, 2] {
            m.observe_gain("main", Direction::Tx, c, 0.0, false, &mut out);
        }
        assert_eq!(out, vec![move_to(12544)], "a gain set elsewhere moves it");
        assert!(m.startup_actions().is_empty(), "already on its way");
        // The desk does not echo: the daemon hands the mapper the position.
        let e = d.moved("f", 12544).unwrap();
        let mut out = vec![];
        m.handle(&e, &mut out);
        assert!(out.is_empty(), "already at the gain");
        assert!(
            feed_midi(&mut m, &mut d, &encode_nrpn(1, 0x40 << 7, 12544)).is_empty(),
            "an echo is no movement"
        );
        assert_eq!(
            feed_midi(&mut m, &mut d, &encode_nrpn(1, 0x40 << 7, 14784)),
            vec![gain(5.0)]
        );
    }

    // ---- output stage -------------------------------------------------------

    fn rx(b: BindingConfig) -> BindingConfig {
        BindingConfig {
            target: Target {
                device: "main".into(),
                direction: Direction::Rx,
                channels: vec![1, 2],
            },
            ..b
        }
    }

    fn rx_target() -> Target {
        rx(binding("f", BindingAction::Gain)).target
    }

    fn out_gain(gain_db: f32) -> SurfaceAction {
        SurfaceAction::SetOutputGain {
            target: rx_target(),
            gain_db,
        }
    }

    fn out_mute(muted: bool) -> SurfaceAction {
        SurfaceAction::SetOutputMute {
            target: rx_target(),
            muted,
        }
    }

    #[test]
    fn output_bindings_ask_for_the_output_stage() {
        let mut m = mapper(vec![
            rx(binding("f", BindingAction::OutputGain)),
            rx(binding("b", BindingAction::OutputMute)),
        ]);
        assert_eq!(fader(&mut m, 100), vec![out_gain(0.0)]);
        assert_eq!(fader(&mut m, 50), vec![out_gain(-30.0)]);
        // Toggle on the output's observed mute, not the channel's.
        m.observe_mute("main", Direction::Rx, 1, true);
        m.observe_mute("main", Direction::Rx, 2, true);
        assert_eq!(press(&mut m, true), vec![out_mute(true)]);
        echo(&mut m, &out_mute(true), true);
        assert!(press(&mut m, false).is_empty());
        assert_eq!(press(&mut m, true), vec![out_mute(false)]);
        // A refused output gain lets the fader ask again on its next move.
        m.rejected(&out_gain(-30.0));
        assert_eq!(fader(&mut m, 75), vec![out_gain(-15.0)]);
    }

    #[test]
    fn output_and_channel_gain_on_one_channel_are_independent() {
        // Two faders: `f` (output) and `e` used as a second fader (channel).
        let mut m = mapper(vec![
            rx(binding("f", BindingAction::OutputGain)),
            rx(binding("e", BindingAction::Gain)),
        ]);
        let mut out = vec![];
        for c in [1, 2] {
            m.observe_gain("main", Direction::Rx, c, -10.0, false, &mut out);
            m.observe_output_gain("main", Direction::Rx, c, -20.0, false, &mut out);
        }
        assert!(out.is_empty());
        // The encoder steps the channel gain from its own observed value.
        let ch = |gain_db| SurfaceAction::SetGain {
            target: rx_target(),
            gain_db,
        };
        assert_eq!(enc(&mut m, 1), vec![ch(-9.0)]);
        // The output fader counts from the output level.
        assert_eq!(fader(&mut m, 100), vec![out_gain(0.0)]);
    }

    #[test]
    fn motorized_output_fader_follows_output_level_only() {
        let mut m = mapper_with(vec![rx(binding("f", BindingAction::OutputGain))], true);
        let mut out = vec![];
        for c in [1, 2] {
            m.observe_gain("main", Direction::Rx, c, -30.0, false, &mut out);
        }
        assert!(out.is_empty(), "the channel gain does not move it");
        for c in [1, 2] {
            m.observe_output_gain("main", Direction::Rx, c, -30.0, false, &mut out);
        }
        assert_eq!(out, vec![move_to(50)], "once per move");
        // Reports while the motor travels are ignored until it gets there.
        assert!(fader(&mut m, 80).is_empty());
        assert!(fader(&mut m, 50).is_empty(), "arrived: engaged, no change");
        assert_eq!(fader(&mut m, 60), vec![out_gain(-24.0)]);
        // Own echoes are not a change made elsewhere.
        let mut out = vec![];
        m.observe_output_gain("main", Direction::Rx, 1, -24.0, true, &mut out);
        assert!(out.is_empty());
        // Startup moves it to the output level.
        let mut m = mapper_with(vec![rx(binding("f", BindingAction::OutputGain))], true);
        let mut out = vec![];
        m.observe_output_gain("main", Direction::Rx, 1, 0.0, false, &mut out);
        assert_eq!(out, vec![move_to(100)]);
    }

    #[test]
    fn output_mute_button_modes() {
        let mut b = rx(binding("b", BindingAction::OutputMute));
        b.mode = ButtonMode::PushToTalk;
        let mut m = mapper(vec![b.clone()]);
        assert_eq!(m.startup_actions(), vec![out_mute(true)]);
        assert_eq!(press(&mut m, true), vec![out_mute(false)]);
        assert_eq!(press(&mut m, false), vec![out_mute(true)]);
        b.mode = ButtonMode::Momentary;
        let mut m = mapper(vec![b]);
        assert_eq!(press(&mut m, true), vec![out_mute(true)]);
        assert_eq!(
            m.reset(),
            vec![out_mute(false)],
            "release restored on reset"
        );
    }

    // ---- host volume, router levels, follow ----------------------------------

    fn cq_in(n: u16) -> RouterPort {
        RouterPort::CardIn {
            card: "hw:CARD=CQ,DEV=0".into(),
            channel: tessera_control_types::Channel::new(n).unwrap(),
        }
    }

    fn untargeted(control: &str, action: BindingAction) -> BindingConfig {
        BindingConfig {
            target: Target::none(),
            ..binding(control, action)
        }
    }

    #[test]
    fn host_volume_and_port_bindings_request_their_commands() {
        let mut ports = untargeted("b", BindingAction::PortMute);
        ports.ports = vec![cq_in(15)];
        let mut m = mapper(vec![untargeted("f", BindingAction::HostVolume), ports]);
        assert_eq!(
            fader(&mut m, 100),
            vec![SurfaceAction::SetHostVolume { gain_db: 0.0 }]
        );
        // A toggle flips from the observed port mute.
        assert_eq!(
            press(&mut m, true),
            vec![SurfaceAction::SetPortMute {
                ports: vec![cq_in(15)],
                muted: true
            }]
        );
        m.observe_level_mute(Level::Port(cq_in(15)), true);
        assert_eq!(press(&mut m, false), vec![]);
        assert_eq!(
            press(&mut m, true),
            vec![SurfaceAction::SetPortMute {
                ports: vec![cq_in(15)],
                muted: false
            }]
        );
        assert!(m.covers(&Level::Host));
        assert!(m.covers(&Level::Port(cq_in(15))));
    }

    #[test]
    fn a_port_on_a_network_channel_is_that_channel() {
        let mut b = untargeted("f", BindingAction::PortGain);
        b.ports = vec![RouterPort::NetworkTx {
            device: tessera_control_types::DeviceKey("main".into()),
            channel: tessera_control_types::Channel::new(2).unwrap(),
        }];
        let m = mapper(vec![b]);
        assert!(m.covers(&Level::Channel {
            output: false,
            device: "main".into(),
            direction: Direction::Tx,
            channel: 2
        }));
    }

    #[test]
    fn a_motorized_crosspoint_fader_follows_the_gain_set_elsewhere() {
        let key = CrosspointKey::new(
            cq_in(15),
            RouterPort::HostOut {
                device: tessera_control_types::DeviceKey("main".into()),
                channel: tessera_control_types::Channel::new(1).unwrap(),
            },
        );
        let mut b = untargeted("f", BindingAction::CrosspointGain);
        b.crosspoints = vec![key.clone()];
        let mut m = mapper_with(vec![b], true);
        let mut out = vec![];
        m.observe_level_gain(Level::Crosspoint(key.clone()), -30.0, false, &mut out);
        assert_eq!(out, vec![move_to(50)], "moved where the gain is");
        assert_eq!(m.startup_actions(), vec![], "already on its way");
        // A refused request makes the motor take over again from the
        // observed gain.
        m.rejected(&SurfaceAction::SetCrosspointGain {
            crosspoints: vec![key],
            gain_db: -12.0,
        });
    }

    #[test]
    fn follow_sends_db_by_this_law_and_button_states() {
        let to = FollowTarget {
            surface: "desk2".into(),
            control: "f1".into(),
        };
        let mut f = untargeted("f", BindingAction::Follow);
        f.follow = Some(to.clone());
        let mut b = untargeted("b", BindingAction::Follow);
        b.follow = Some(FollowTarget {
            surface: "desk2".into(),
            control: "m1".into(),
        });
        let mut m = mapper(vec![f, b]);
        assert_eq!(
            fader(&mut m, 50),
            vec![SurfaceAction::Follow {
                to: to.clone(),
                value: FollowValue::Fader { gain_db: -30.0 }
            }]
        );
        // Within the deadband: nothing.
        assert_eq!(fader(&mut m, 50), vec![]);
        assert_eq!(
            fader(&mut m, 0),
            vec![SurfaceAction::Follow {
                to,
                value: FollowValue::Fader {
                    gain_db: f32::NEG_INFINITY
                }
            }]
        );
        // A toggle (the default mode) button that reports presses flips on
        // each press and sends nothing on release.
        assert_eq!(follow_button(&mut m, true), vec![true]);
        assert!(follow_button(&mut m, false).is_empty());
        assert_eq!(follow_button(&mut m, true), vec![false]);
        assert!(follow_button(&mut m, false).is_empty());
        assert!(m.covered().is_empty(), "following acts on no Tessera level");
    }

    /// The button states a press (or release) of `b` sends along `follow`.
    fn follow_button(m: &mut Mapper, pressed: bool) -> Vec<bool> {
        press(m, pressed)
            .into_iter()
            .filter_map(|a| match a {
                SurfaceAction::Follow {
                    value: FollowValue::Button { on },
                    ..
                } => Some(on),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_followed_button_takes_the_state_by_the_followers_mode() {
        let follow = |mode| {
            let mut b = untargeted("b", BindingAction::Follow);
            b.follow = Some(FollowTarget {
                surface: "desk2".into(),
                control: "m1".into(),
            });
            b.mode = mode;
            b
        };
        // Momentary and push-to-talk: on while held.
        for mode in [ButtonMode::Momentary, ButtonMode::PushToTalk] {
            let mut m = mapper(vec![follow(mode)]);
            assert_eq!(follow_button(&mut m, true), vec![true], "{mode:?}");
            assert_eq!(follow_button(&mut m, false), vec![false], "{mode:?}");
        }
        // A desk's state parameter (an NRPN button) passes its state on,
        // whatever the mode: on, then off.
        use crate::config::{MidiAddress, MidiKind};
        let cfg_control = ControlConfig {
            id: "b".into(),
            kind: ControlKind::Button,
            report_id: None,
            offset: None,
            bits: None,
            max: None,
            inverted: false,
            active_low: false,
            motorized: false,
            code: None,
            midi: Some(MidiAddress::new(MidiKind::Nrpn, 1, 0)),
        };
        let mut m = Mapper::new(&SurfaceConfig {
            name: "t".into(),
            enabled: true,
            source: SourceConfig::Midi {
                card: "CQ".into(),
                device: 0,
            },
            controls: vec![cfg_control],
            bindings: vec![follow(ButtonMode::Toggle)],
        });
        assert_eq!(follow_button(&mut m, true), vec![true]);
        assert_eq!(follow_button(&mut m, false), vec![false]);
        // A reset starts a toggle over (off).
        let mut m = mapper(vec![follow(ButtonMode::Toggle)]);
        assert_eq!(follow_button(&mut m, true), vec![true]);
        let _ = m.reset();
        assert_eq!(follow_button(&mut m, true), vec![true]);
    }
}
