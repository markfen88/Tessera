//! The surface runtime: reads each configured control surface and turns its
//! buttons and faders into typed commands (plan §2.4).
//!
//! **Tasks.** [`run`] is a supervised control-plane task. It reads the saved
//! surfaces from the state engine (`Query::Surfaces`) at start, after every
//! `SurfacesChanged` event, after a configuration reload and after a lag, and
//! keeps one task per enabled surface. An edited surface keeps its task: the
//! new spec is handed over (see *Edits*). A removed or disabled surface is
//! stopped.
//!
//! **One surface.** The task looks for its device every
//! [`RESCAN`] (hotplug by polling), opens the `hidraw` node read-only and
//! reads it on a plain thread ([`super::reader`]). A session on the open
//! device:
//!
//! 1. subscribe to the event bus, drain it, then read the status and observe
//!    the mute and gain of every channel the bindings cover (whatever is
//!    queued afterwards is newer than the read);
//! 2. send the mapper's start-up requests (push-to-talk targets muted:
//!    mute is never owned, so this needs no connection);
//! 3. at the **first report**, decode it, then report the connection
//!    (`ReportSurfaceConnection`, `Origin::Internal`: the state engine
//!    decides fader ownership from it) and apply what the report asks for,
//!    so a fader takes ownership together with its real position and the
//!    gain is never held at a stale value;
//! 4. each report → `Decoder` → `Mapper` → one typed command per request,
//!    applied as `Origin::Surface { name }`; a refused command goes back to
//!    `Mapper::rejected`;
//! 5. each `ChannelMuteChanged` / `ChannelGainChanged` on a covered channel
//!    → `Mapper::observe_*` (`own` when the event carries this surface's
//!    origin); a lag drains and re-reads the status.
//!
//! **Endings.** When the device is unplugged (the read fails) the session
//! sends the mapper's reset requests (a held momentary button restores its
//! state, push-to-talk mutes), reports the disconnection and goes back to
//! looking for the device. When the surface is removed or disabled (or the
//! daemon stops) it only reports the disconnection: nothing is muted on the
//! way out, so no channel is left muted with no surface to unmute it. A
//! task that does not stop within [`STOP_GRACE`] is aborted and the
//! disconnection reported for it.
//!
//! **Edits.** A new spec with the same source is applied on the open device:
//! a new decoder primed with the last report of each report id (held
//! buttons are not presses) and a new mapper fed the status; no reset
//! requests, no disconnection, so ownership is never dropped and taken
//! again. Bindings to push-to-talk that are new are muted. Every fader is
//! evaluated again under its new binding (a new curve or range) at once: a
//! motorized one is moved to the current gain (the gain owns it), one
//! without a motor applies its last reported position (it owns the gain;
//! for MIDI, the position the desk last sent). A held momentary button's
//! saved state does not survive an edit (its release then leaves the mute
//! as it is). Only a different source closes the device and opens the new
//! one.
//!
//! **Learning.** While `Query::LearnControl` watches a device, the surface
//! using it does not act on its reports (they are not even decoded), so
//! moving a control to learn it changes nothing; it takes no `follow`
//! values and sends no liveness probe meanwhile (what it writes to the
//! device would come back and be learned). A learn treats the first
//! [`LEARN_IDLE`] as idle (the portal asks for hands off) to learn the
//! device's noise, then reports the first control that moves, at once for a
//! button pressed and released, or once the learner's result has stopped
//! changing for [`LEARN_SETTLE`].
//!
//! **Live state** for `Query::Surfaces` is kept per surface in [`Surfaces`]:
//! connected or not, the device in use or why none, and the last value of
//! every control the device has reported (`None` until it has: a control
//! never shows a default such as 0). A position tessera sends a motorized
//! fader to is its value once the write was taken (queued to the port, or
//! the desk shows it already), also before the control has reported: the
//! desk's echo of that move is no change, so it would never show
//! otherwise. Such a move is not a hand move: it sets nothing.
//!
//! **MIDI surfaces** (`SurfaceSource::Midi`, a rawmidi port of a card) run
//! the same session on a byte stream: the port is opened read-write, its
//! bytes go through the MIDI decoder (by each control's `MidiAddress`) into
//! the same mapper. Differences:
//!
//! * a button carried by an NRPN (a desk's mute: value 1 muted, 0 not)
//!   reports a state, not presses: bound in toggle mode it sets the mute to
//!   that state (it never flips a mute that is already right);
//! * feedback to the desk: a motorized fader the mapper asks to move gets
//!   its value written; a mute button's state is written whenever the mute
//!   of its target changes, unless the desk already shows it (what the desk
//!   sent last, or what was written last: no echo loops);
//! * on open, the current value of every NRPN control is asked for (the
//!   Allen & Heath "get"), so the desk's faders and mutes apply at once;
//! * a learn on a port the session holds is fed from the session (a rawmidi
//!   input is usually open once only);
//! * output is queued whole to the port's writer thread (never written on
//!   an async worker); a write that fails ends the session as lost, and a
//!   value is taken as shown on the desk only once it was queued;
//! * **liveness** (off by default; a surface with a control that has a
//!   "get", when switched on with `Surfaces::set_liveness`): after a
//!   third of [`LIVENESS`] without bytes from the device, one control is
//!   asked for its value; after all of it the session is lost (faders are
//!   released, the disconnection is reported) and the device is opened (or
//!   connected) again. Silence only counts once the device has answered
//!   one of our value requests in this session (a message addressing the
//!   control asked, within half a second, an unchanged value included; an
//!   edit asks again and starts over): a desk that talks only when a
//!   control moves
//!   and does not answer a get (the Allen & Heath CQ over USB) is never
//!   lost for being idle; it is probed for one liveness period, then not
//!   again, and only its device errors (or TCP keep-alive) end the
//!   session. Paused while a learn watches the device (the answer to a
//!   probe would be learned as a control that moved); it starts over when
//!   the learn ends.
//!
//! **`tcp_midi` surfaces** (`SurfaceSource::TcpMidi`) run the same MIDI
//! session on a TCP connection tessera opens to `host:port` (it only
//! connects out, ADR 0017): raw MIDI bytes both ways, read on a plain thread
//! like a rawmidi port. A connection that fails is tried again after 1 s,
//! doubling to 30 s; one that worked and was lost is tried again after 1 s
//! (one that was accepted but never sent anything counts as failed). The
//! socket has TCP keep-alive and a user timeout of a few seconds.
//! The device id is `tcp:<host>:<port>`.
//!
//! **Levels.** Bindings act on channel gains and mutes (and the output
//! stage), the host volume, router port levels and crosspoint gains; the
//! session observes each of them from the status, `Query::Router` and their
//! events (`HostVolumeChanged`, `PortGainChanged`, `CrosspointMuteChanged`,
//! ...), so motorized faders and mute lights follow every kind.
//!
//! **Control routing (`follow`).** A control that follows another acts
//! exactly like it. A `follow` binding's value (a fader's level in dB by
//! this binding's law, a button's state: a desk's NRPN state as it is, a
//! press-and-release button on while held, or in toggle mode flipped at
//! each press) goes to the session of the
//! followed surface, which (a) writes it to its device as the followed
//! control's position by that control's first fader binding's law (the
//! default -60..0 dB without one) and (b) hands that position to its mapper
//! as the followed control's own movement: its bindings apply (origin: the
//! followed surface, through each binding's law; a motorized fader takes
//! the move at once) and its own `follow` bindings pass the value on, so
//! A → B → C reaches C. A value carries the controls it came through and
//! never goes through one twice, so a ring of follows never loops. The
//! device's echo of a followed move (a motor reporting where it went:
//! within 0.5 dB by the control's law or 1 % of its travel, for 3 s) is
//! dropped before mapping; any other report of that control meanwhile is a
//! movement (its bindings apply) but is not followed back along the path
//! that drove it, so a pair whose motors land a little off never
//! ping-pongs. A HID surface takes no output: a follow into one shows as
//! its problem (its bindings still apply).
//!
//! **Connection reports and epochs.** The state engine issues a new
//! connection epoch (`SurfaceStatus::connection_epoch`) and drops the
//! connection whenever a surface is added, enabled, disabled, moved to
//! another source or reloaded. A session reads the epoch when it starts and
//! reports its connection under it (`ReportSurfaceConnection { epoch }`);
//! refused for a stale epoch, it reads the surface again and, while it is
//! still enabled with the session's source, reports again under the new
//! epoch (a session on an old source never claims it).
//! On every `SurfacesChanged` (and after a lag) a session that has reported
//! reads the state again: an epoch that is not the one it reported under,
//! or the surface shown not connected, while it is enabled with the same
//! source (a disable and enable the runtime saw as the same spec), is
//! reported again under the new epoch, though the device stayed open; the
//! levels are read again (one set while the connection was dropped is not
//! taken for a fader's own) and every fader without a motor applies its
//! last position (it owns its levels again). Its disconnection is reported
//! under its own epoch (one the engine has moved past changes nothing),
//! also by the runtime for a session it had to abort.
//!
//! **Lost sessions and push-to-talk.** A session lost for any reason (the
//! device unplugged or silent past its liveness, its input or its output
//! failing, a full output queue included) applies the push-to-talk safety
//! mutes: once the session has ended nothing sees the button any more, so a
//! released button could never mute, and the session that opens the device
//! again mutes push-to-talk targets at start in any case (until the desk
//! reports the button held), so keeping the mute for an output stall loses
//! nothing.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tessera_control_types::{
    Command, CommandError, ControlState, Direction, Event, LearnedControl, MidiAddress, Origin,
    Query, Reply, SurfaceProfileInfo, SurfaceSource, SurfaceSpec, SurfaceStatus, SurfacesReport,
};
use tessera_state::{EventBus, Received, Subscription};
use tessera_surface::config::{BindingConfig, ButtonMode, FollowTarget, Level};
use tessera_surface::mapper::{FollowValue, mute_request};
use tessera_surface::{
    Decoder, Mapper, SurfaceAction, SurfaceConfig, SurfaceEvent, SurfaceEventKind, fader_position,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::compat;
use super::discovery::{self, Scan};
use super::reader::{self, Output, Reports};
use crate::engine_thread::EngineHandle;
use crate::media::gains::drain;
use crate::sanitize;

/// How often a surface whose device is missing looks again.
pub const RESCAN: Duration = Duration::from_secs(2);
/// Longest `Query::LearnControl` wait, milliseconds (the contract's 30 s).
pub const MAX_LEARN_MS: u32 = 30_000;
/// The start of a learn taken as idle: the device's own noise.
pub const LEARN_IDLE: Duration = Duration::from_secs(1);
/// A fader being learned is reported once the learner's result stopped
/// changing for this long.
pub const LEARN_SETTLE: Duration = Duration::from_millis(600);
/// How long a stopped surface may take to release its device.
const STOP_GRACE: Duration = Duration::from_secs(3);
/// Shown while the device is open but has not reported yet.
const WAITING: &str = "waiting for the device's first report (move a control)";
/// A suggested liveness for MIDI sessions whose controls can be asked for
/// their value, when it is switched on with [`Surfaces::set_liveness`]
/// (off by default).
pub const LIVENESS: Duration = Duration::from_secs(10);

/// What one surface is doing now.
#[derive(Debug, Clone, Default)]
struct Live {
    connected: bool,
    device: Option<String>,
    problem: Option<String>,
    controls: Vec<ControlState>,
    /// The connection epoch the session reported its connection under
    /// (`None`: unknown, or not reported), for [`stop`] when it must
    /// report the disconnection for an aborted session.
    epoch: Option<u64>,
}

impl Live {
    fn of(spec: &SurfaceSpec, problem: Option<String>) -> Self {
        Self {
            connected: false,
            device: None,
            problem,
            controls: spec
                .controls
                .iter()
                .map(|c| ControlState::new(c.id.clone(), None))
                .collect(),
            epoch: None,
        }
    }

    /// The controls of an edited spec, keeping the values of those that
    /// stay.
    fn relist(&mut self, spec: &SurfaceSpec) {
        let old = std::mem::take(&mut self.controls);
        self.controls = spec
            .controls
            .iter()
            .map(|c| {
                let value = old.iter().find(|o| o.id == c.id).and_then(|o| o.value);
                ControlState::new(c.id.clone(), value)
            })
            .collect();
    }
}

/// The surface runtime's shared state: where devices are looked for, what
/// each surface is doing, which devices are being learned. The control
/// socket answers `Query::Surfaces` and `Query::LearnControl` through it.
#[derive(Debug)]
pub struct Surfaces {
    root: PathBuf,
    lab: bool,
    live: Mutex<HashMap<String, Live>>,
    learning: Mutex<HashSet<String>>,
    /// MIDI ports a surface session holds open now.
    holding: Mutex<HashSet<String>>,
    /// A learn on a held MIDI port: the session hands its bytes here.
    taps: Mutex<HashMap<String, mpsc::Sender<Vec<u8>>>>,
    /// Lab only: what was written to each scripted MIDI port, by device id.
    lab_out: Mutex<HashMap<String, Arc<Mutex<Vec<u8>>>>>,
    /// The surface profiles (`Query::SurfaceProfiles`).
    profiles: Mutex<Vec<SurfaceProfileInfo>>,
    /// Control routing: the running sessions, by surface name, that take
    /// values for their controls from `follow` bindings of other surfaces.
    follow: Mutex<HashMap<String, mpsc::UnboundedSender<Followed>>>,
    /// MIDI session liveness, milliseconds (0: off).
    liveness_ms: std::sync::atomic::AtomicU64,
    /// How long a followed move's echo is expected, milliseconds.
    echo_ms: std::sync::atomic::AtomicU64,
}

/// A control of a surface: (surface name, control id).
type ControlKey = (String, String);

/// A value a `follow` binding sends to a control of another surface.
#[derive(Debug, Clone)]
struct Followed {
    control: String,
    value: FollowValue,
    /// The controls this value came through, the first mover first (the
    /// control that sent it last): a control never takes a value that
    /// already went through it, so following in a ring never loops.
    path: Vec<ControlKey>,
}

/// Holds a MIDI port in [`Surfaces::holding`] for as long as it lives.
struct HoldGuard<'a> {
    surfaces: &'a Surfaces,
    id: Option<String>,
}

impl Drop for HoldGuard<'_> {
    fn drop(&mut self) {
        if let Some(id) = &self.id {
            lock(&self.surfaces.holding).remove(id);
        }
    }
}

/// Where learned bytes come from: the device itself, or a session's tap.
trait ByteSource {
    async fn next_bytes(&mut self) -> Result<Vec<u8>, String>;
}

impl ByteSource for Reports {
    async fn next_bytes(&mut self) -> Result<Vec<u8>, String> {
        self.next().await
    }
}

impl ByteSource for mpsc::Receiver<Vec<u8>> {
    async fn next_bytes(&mut self) -> Result<Vec<u8>, String> {
        self.recv()
            .await
            .ok_or_else(|| "the surface released the device".to_owned())
    }
}

/// A HID or a MIDI learner.
trait Learn {
    fn idle(&mut self, bytes: &[u8]);
    fn feed(&mut self, bytes: &[u8]) -> Option<LearnedControl>;
    fn result(&self) -> Option<LearnedControl>;
}

impl Learn for tessera_surface::learn::Learner {
    fn idle(&mut self, bytes: &[u8]) {
        tessera_surface::learn::Learner::idle(self, bytes);
    }
    fn feed(&mut self, bytes: &[u8]) -> Option<LearnedControl> {
        tessera_surface::learn::Learner::feed(self, bytes)
    }
    fn result(&self) -> Option<LearnedControl> {
        tessera_surface::learn::Learner::result(self)
    }
}

impl Learn for compat::MidiLearn {
    fn idle(&mut self, bytes: &[u8]) {
        compat::MidiLearn::idle(self, bytes);
    }
    fn feed(&mut self, bytes: &[u8]) -> Option<LearnedControl> {
        compat::MidiLearn::feed(self, bytes)
    }
    fn result(&self) -> Option<LearnedControl> {
        compat::MidiLearn::result(self)
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Removes a device from the learning set when a learn ends.
struct LearnGuard<'a> {
    set: &'a Mutex<HashSet<String>>,
    id: String,
}

impl Drop for LearnGuard<'_> {
    fn drop(&mut self) {
        lock(self.set).remove(&self.id);
    }
}

impl Surfaces {
    /// Devices are looked for under `/`, or under `lab_root` (lab only:
    /// fixtures with scripted nodes, see [`super::reader`]).
    #[must_use]
    pub fn new(lab_root: Option<PathBuf>) -> Self {
        Self {
            lab: lab_root.is_some(),
            root: lab_root.unwrap_or_else(|| PathBuf::from("/")),
            live: Mutex::new(HashMap::new()),
            learning: Mutex::new(HashSet::new()),
            holding: Mutex::new(HashSet::new()),
            taps: Mutex::new(HashMap::new()),
            lab_out: Mutex::new(HashMap::new()),
            profiles: Mutex::new(Vec::new()),
            follow: Mutex::new(HashMap::new()),
            // Off by default (0.6.0): device errors (USB) and TCP keep-alive
            // find a lost desk; an idle desk that does not answer a get (the
            // Allen & Heath CQ over USB) must not be dropped for its silence.
            liveness_ms: std::sync::atomic::AtomicU64::new(0),
            echo_ms: std::sync::atomic::AtomicU64::new(
                u64::try_from(ECHO.as_millis()).unwrap_or(u64::MAX),
            ),
        }
    }

    /// The echo window of followed moves (tests shorten it).
    #[cfg(test)]
    fn set_echo_window(&self, d: Duration) {
        self.echo_ms.store(
            u64::try_from(d.as_millis()).unwrap_or(u64::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    fn echo_window(&self) -> Duration {
        Duration::from_millis(self.echo_ms.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// The liveness of MIDI sessions (`Duration::ZERO`: off, the default): a
    /// session whose device sent nothing for a third of it asks the device
    /// for one control's value (the "get"), and one that sent nothing for
    /// all of it is lost (its faders and the connection are released, the
    /// device is opened again). Only for a surface with a control that has a
    /// "get" (an NRPN control): another device need not send anything while
    /// nobody touches it. Sessions started later use the new value.
    ///
    /// Silence counts only once the device answered a value request in the
    /// session: a message addressing the control asked within half a second
    /// (its value unchanged included; not a control whose followed move's
    /// echo is expected). A device that never answers is probed for one
    /// liveness period, then not again. An edit of the surface asks for
    /// every value again and starts all of this over.
    ///
    /// Limitation (why it is off by default): an answer is recognised by
    /// its address and timing only. A hand move of the control asked within
    /// that half second is taken for an answer, and a desk that answers
    /// later, or with another parameter, is taken for one that never
    /// answers.
    pub fn set_liveness(&self, d: Duration) {
        self.liveness_ms.store(
            u64::try_from(d.as_millis()).unwrap_or(u64::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    fn liveness(&self) -> Option<Duration> {
        let ms = self.liveness_ms.load(std::sync::atomic::Ordering::Relaxed);
        (ms > 0).then(|| Duration::from_millis(ms))
    }

    /// Hand a `follow` value that came through `path` to the session of
    /// surface `to.surface`, if it runs (a follow to a surface that is not
    /// there does nothing) and the value did not go through that control
    /// already.
    fn follow(&self, to: &FollowTarget, value: FollowValue, path: &[ControlKey]) {
        if path
            .iter()
            .any(|(s, c)| *s == to.surface && *c == to.control)
        {
            return;
        }
        if let Some(tx) = lock(&self.follow).get(&to.surface) {
            let _ = tx.send(Followed {
                control: to.control.clone(),
                value,
                path: path.to_vec(),
            });
        }
    }

    /// (Re)load the surface profiles: built in, and the files of
    /// [`compat::PROFILE_DIRS`] under the device root (bad files are logged).
    pub async fn load_profiles(&self) {
        let root = self.root.clone();
        let p = tokio::task::spawn_blocking(move || compat::load_profiles(&root))
            .await
            .unwrap_or_default();
        *lock(&self.profiles) = p;
    }

    /// `Query::SurfaceProfiles`.
    #[must_use]
    pub fn profiles(&self) -> Vec<SurfaceProfileInfo> {
        lock(&self.profiles).clone()
    }

    /// A session hands bytes of a device being learned to the learn's tap
    /// (dropped if there is none or it is full).
    fn tap(&self, device: &str, bytes: &[u8]) {
        if let Some(t) = lock(&self.taps).get(device) {
            let _ = t.try_send(bytes.to_vec());
        }
    }

    /// Scan the devices (file reads, off the async workers).
    pub async fn scan(&self) -> Scan {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || discovery::scan(&root))
            .await
            .unwrap_or_default()
    }

    fn update(&self, name: &str, f: impl FnOnce(&mut Live)) {
        if let Some(l) = lock(&self.live).get_mut(name) {
            f(l);
        }
    }

    /// Is a learn watching this detected device now?
    fn learning(&self, device: &str) -> bool {
        lock(&self.learning).contains(device)
    }

    /// `Query::Surfaces`: the devices present, and the saved surfaces (from
    /// the state engine) with what each is doing.
    ///
    /// # Errors
    /// The state engine's error (a state engine without surfaces answers
    /// with no surfaces, not an error).
    pub async fn report(&self, engine: &EngineHandle) -> Result<SurfacesReport, CommandError> {
        let mut surfaces = saved(engine).await?;
        let bridges = match engine.query(Query::Bridges).await {
            Ok(Reply::Bridges(b)) => b,
            _ => Vec::new(),
        };
        let scan = self.scan().await;
        // The portal reads this before it sets a bridge up: the engine then
        // checks the bridge's card channels against what the card has.
        engine.report_sound_cards(scan.sound_cards());
        {
            let live = lock(&self.live);
            for st in &mut surfaces {
                let l = live.get(&st.spec.name).cloned().unwrap_or_else(|| {
                    Live::of(
                        &st.spec,
                        Some(
                            if st.spec.enabled {
                                "starting"
                            } else {
                                "disabled"
                            }
                            .into(),
                        ),
                    )
                });
                st.connected = l.connected;
                st.device = l.device;
                st.problem = l.problem;
                st.controls = l.controls;
            }
        }
        let specs: Vec<SurfaceSpec> = surfaces.iter().map(|s| s.spec.clone()).collect();
        let mut r = SurfacesReport::default();
        r.detected = discovery::detected(&scan, &specs, &bridges);
        r.surfaces = surfaces;
        Ok(r)
    }

    /// `Query::LearnControl`: watch a detected HID device for up to
    /// `timeout_ms` and report the first control that moves (see the module
    /// documentation). One learn per device at a time; the surface using
    /// the device pauses meanwhile. `moving_up`: the user was asked to move
    /// a fader up, so a falling value means an inverted fader.
    ///
    /// # Errors
    /// `Invalid` for a timeout outside 1..=[`MAX_LEARN_MS`] or a device
    /// already being learned, `NotFound` for a device that is not a
    /// detected HID interface, `Internal` if it cannot be opened.
    pub async fn learn(
        &self,
        device: &str,
        timeout_ms: u32,
        moving_up: bool,
    ) -> Result<Option<LearnedControl>, CommandError> {
        if timeout_ms == 0 || timeout_ms > MAX_LEARN_MS {
            return Err(CommandError::Invalid {
                field: "timeout_ms".into(),
                reason: format!("from 1 to {MAX_LEARN_MS}"),
            });
        }
        let scan = self.scan().await;
        let timeout = Duration::from_millis(u64::from(timeout_ms));
        let (id, node, hid_numbered) = if let Some(n) = scan.hid_by_id(device) {
            (
                n.id.clone(),
                n.node.clone(),
                Some(n.numbered.unwrap_or(false)),
            )
        } else if let Some(m) = scan.midi_by_id(device) {
            (m.id.clone(), m.node.clone(), None)
        } else if device.starts_with("tcp:") && lock(&self.holding).contains(device) {
            // A tcp_midi connection a surface holds: learned from its session.
            (device.to_owned(), PathBuf::from(device), None)
        } else {
            return Err(CommandError::NotFound {
                what: format!(
                    "HID or MIDI device `{}` (see the detected devices)",
                    sanitize::log_safe(device, 64)
                ),
            });
        };
        if !lock(&self.learning).insert(id.clone()) {
            return Err(CommandError::Invalid {
                field: "device".into(),
                reason: "a learn is already running on this device".into(),
            });
        }
        let _guard = LearnGuard {
            set: &self.learning,
            id: id.clone(),
        };
        let open_err = |e: std::io::Error| CommandError::Internal {
            message: format!("cannot open {}: {e}", node.display()),
        };
        if let Some(numbered) = hid_numbered {
            let mut reports = reader::open(&node, self.lab).map_err(open_err)?;
            let learner = tessera_surface::learn::Learner::new(numbered, moving_up);
            return Ok(learn_from(&mut reports, learner, timeout).await);
        }
        let learner = compat::MidiLearn::new(moving_up);
        // A port a surface holds: its session feeds the learn.
        let tapped = {
            let holding = lock(&self.holding);
            holding.contains(&id).then(|| {
                let (tx, rx) = mpsc::channel(256);
                lock(&self.taps).insert(id.clone(), tx);
                rx
            })
        };
        if let Some(mut rx) = tapped {
            let r = learn_from(&mut rx, learner, timeout).await;
            lock(&self.taps).remove(&id);
            return Ok(r);
        }
        if id.starts_with("tcp:") {
            return Err(CommandError::NotFound {
                what: format!(
                    "the MIDI connection `{}` (its surface released it)",
                    sanitize::log_safe(&id, 64)
                ),
            });
        }
        let mut reports = reader::open(&node, self.lab).map_err(open_err)?;
        Ok(learn_from(&mut reports, learner, timeout).await)
    }
}

/// Feed bytes to the learner: the first [`LEARN_IDLE`] as idle, then as
/// movement, until it finds a button, its result settles, or `timeout`
/// (counted from the start, idle included) passes.
async fn learn_from(
    reports: &mut impl ByteSource,
    mut learner: impl Learn,
    timeout: Duration,
) -> Option<LearnedControl> {
    let start = Instant::now();
    let idle_until = start + LEARN_IDLE;
    let deadline = start + timeout;
    let mut settle_at: Option<Instant> = None;
    let mut seen: Option<LearnedControl> = None;
    loop {
        let wake = settle_at.map_or(deadline, |s| s.min(deadline));
        tokio::select! {
            () = tokio::time::sleep_until(wake) => return learner.result(),
            r = reports.next_bytes() => {
                let Ok(rep) = r else {
                    return learner.result();
                };
                if Instant::now() < idle_until {
                    learner.idle(&rep);
                    continue;
                }
                if let Some(found) = learner.feed(&rep) {
                    return Some(found);
                }
                // Settled when the result stops changing: a counter or
                // jitter the learner ignores keeps the raw reports changing
                // but not the result.
                let now = learner.result();
                if now != seen {
                    settle_at = now.as_ref().map(|_| Instant::now() + LEARN_SETTLE);
                    seen = now;
                }
            }
        }
    }
}

/// The saved surfaces, as the state engine lists them.
async fn saved(engine: &EngineHandle) -> Result<Vec<SurfaceStatus>, CommandError> {
    match engine.query(Query::Surfaces).await {
        Ok(Reply::Surfaces(r)) => Ok(r.surfaces),
        Ok(_) | Err(CommandError::Unsupported { .. }) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

struct Running {
    spec: SurfaceSpec,
    specs: watch::Sender<SurfaceSpec>,
    cancel: CancellationToken,
    join: JoinHandle<()>,
}

/// Everything a surface task needs.
#[derive(Clone)]
struct Deps {
    surfaces: Arc<Surfaces>,
    engine: EngineHandle,
    bus: EventBus,
}

/// Run every enabled surface until `shutdown` (see the module
/// documentation). Supervised by the daemon.
pub async fn run(
    surfaces: Arc<Surfaces>,
    engine: EngineHandle,
    bus: EventBus,
    shutdown: CancellationToken,
) {
    let deps = Deps {
        surfaces,
        engine,
        bus,
    };
    let mut sub = deps.bus.subscribe(256);
    let mut running: HashMap<String, Running> = HashMap::new();
    deps.surfaces.load_profiles().await;
    deps.engine
        .report_sound_cards(deps.surfaces.scan().await.sound_cards());
    let mut stale = true;
    loop {
        if stale {
            if !drain(&sub) {
                break;
            }
            match saved(&deps.engine).await {
                Ok(list) => {
                    stale = false;
                    let specs = list.into_iter().map(|s| s.spec).collect();
                    reconcile(&deps, &shutdown, &mut running, specs).await;
                }
                Err(e) => {
                    tracing::debug!("surfaces: cannot read the saved surfaces ({e}); retrying");
                    tokio::select! {
                        () = shutdown.cancelled() => break,
                        () = tokio::time::sleep(Duration::from_secs(1)) => {}
                    }
                    continue;
                }
            }
        }
        tokio::select! {
            () = shutdown.cancelled() => break,
            r = sub.recv_async() => match r {
                // A reload also re-reads the profile files.
                Ok(Received::Event(
                    Event::ConfigReloaded | Event::ConfigReloadedWithOrigin { .. },
                )) => {
                    deps.surfaces.load_profiles().await;
                    stale = true;
                }
                Ok(Received::Event(Event::SurfacesChanged { .. })) | Ok(Received::Lagged(_)) => {
                    stale = true;
                }
                Ok(Received::Event(_)) => {}
                Err(_) => break,
            }
        }
    }
    for (name, r) in running.drain() {
        stop(&deps, &name, r).await;
    }
}

/// Stop a surface task; one that does not end within [`STOP_GRACE`] is
/// aborted, and its disconnection reported for it (it may have been
/// connected).
async fn stop(deps: &Deps, name: &str, r: Running) {
    r.cancel.cancel();
    let mut join = r.join;
    if tokio::time::timeout(STOP_GRACE, &mut join).await.is_err() {
        join.abort();
        let _ = tokio::time::timeout(STOP_GRACE, join).await;
        tracing::warn!(
            "surface `{}` did not stop in time; aborted",
            sanitize::log_safe(name, 64)
        );
        // Under the epoch the session reported its connection under (one
        // the engine has moved past changes nothing); nothing when it never
        // reported one.
        let reported = lock(&deps.surfaces.live)
            .get(name)
            .filter(|l| l.connected)
            .map(|l| l.epoch);
        if let Some(epoch) = reported {
            let _ = report_connection(&deps.engine, name, false, epoch).await;
        }
        deps.surfaces.update(name, |l| {
            l.connected = false;
            l.device = None;
            l.epoch = None;
        });
    }
}

/// Start, update and stop surface tasks to match `specs`.
async fn reconcile(
    deps: &Deps,
    shutdown: &CancellationToken,
    running: &mut HashMap<String, Running>,
    specs: Vec<SurfaceSpec>,
) {
    let names: HashSet<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    let gone: Vec<String> = running
        .keys()
        .filter(|n| !names.contains(n.as_str()))
        .cloned()
        .collect();
    for n in gone {
        if let Some(r) = running.remove(&n) {
            tracing::info!("surface `{}` removed", sanitize::log_safe(&n, 64));
            stop(deps, &n, r).await;
        }
        lock(&deps.surfaces.live).remove(&n);
    }
    for spec in specs {
        if let Some(r) = running.get_mut(&spec.name) {
            if r.spec == spec {
                continue;
            }
            if spec.enabled {
                // Edited: the task applies it on the open device.
                r.spec.clone_from(&spec);
                r.specs.send_replace(spec);
                continue;
            }
        }
        if let Some(r) = running.remove(&spec.name) {
            tracing::info!("surface `{}` disabled", sanitize::log_safe(&spec.name, 64));
            stop(deps, &spec.name, r).await;
        }
        let problem = (!spec.enabled).then(|| "disabled".to_owned());
        lock(&deps.surfaces.live).insert(spec.name.clone(), Live::of(&spec, problem));
        if !spec.enabled {
            continue;
        }
        let cancel = shutdown.child_token();
        let (tx, rx) = watch::channel(spec.clone());
        let join = tokio::spawn(surface_task(deps.clone(), rx, cancel.clone()));
        running.insert(
            spec.name.clone(),
            Running {
                spec,
                specs: tx,
                cancel,
                join,
            },
        );
    }
}

/// Why a session on an open device ended.
enum Ended {
    /// Removed, disabled or the daemon is stopping.
    Stopped,
    /// The device went away (unplugged).
    Lost(String),
    /// An edit names another source.
    Reopen,
    /// An edit is not a usable configuration.
    Invalid(String),
}

/// The device a spec asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Wanted {
    Hid {
        vendor_id: u16,
        product_id: u16,
        interface: Option<u8>,
    },
    Midi {
        card: String,
        device: u8,
    },
    /// Raw MIDI over a TCP connection tessera opens.
    Tcp {
        host: String,
        port: u16,
    },
}

impl Wanted {
    fn describe(&self) -> String {
        match self {
            Wanted::Hid {
                vendor_id,
                product_id,
                interface: Some(i),
            } => format!("device {vendor_id:04x}:{product_id:04x} interface {i}"),
            Wanted::Hid {
                vendor_id,
                product_id,
                interface: None,
            } => format!("device {vendor_id:04x}:{product_id:04x}"),
            Wanted::Midi { card, device } => format!("MIDI port {device} of card {card}"),
            Wanted::Tcp { host, port } => format!("MIDI over TCP at {}", tcp_id(host, *port)),
        }
    }
}

/// The device id of a `tcp_midi` connection (`SurfaceStatus::device`).
fn tcp_id(host: &str, port: u16) -> String {
    format!("tcp:{host}:{port}")
}

/// The wait before connecting again after `failures` failed connections in a
/// row (1 s doubling to 30 s).
fn tcp_backoff(failures: u32) -> Duration {
    Duration::from_secs(1u64 << failures.saturating_sub(1).min(5)).min(Duration::from_secs(30))
}

/// The configuration and the device of a spec, or why it cannot run.
fn prepare(spec: &SurfaceSpec) -> Result<(SurfaceConfig, Wanted), String> {
    let wanted = match &spec.source {
        SurfaceSource::Hidraw {
            vendor_id,
            product_id,
            interface,
        } => Wanted::Hid {
            vendor_id: *vendor_id,
            product_id: *product_id,
            interface: *interface,
        },
        SurfaceSource::Midi { card, device } => {
            if let Some(c) = spec
                .controls
                .iter()
                .find(|c| c.midi.is_none() && spec.bindings.iter().any(|b| b.control == c.id))
            {
                return Err(format!(
                    "control `{}` has no MIDI address",
                    sanitize::log_safe(&c.id, 64)
                ));
            }
            Wanted::Midi {
                card: card.clone(),
                device: *device,
            }
        }
        SurfaceSource::TcpMidi { host, port } => {
            if let Some(c) = spec
                .controls
                .iter()
                .find(|c| c.midi.is_none() && spec.bindings.iter().any(|b| b.control == c.id))
            {
                return Err(format!(
                    "control `{}` has no MIDI address",
                    sanitize::log_safe(&c.id, 64)
                ));
            }
            Wanted::Tcp {
                host: host.clone(),
                port: *port,
            }
        }
        _ => return Err("this daemon cannot read this kind of source".into()),
    };
    let cfg = compat::config_of(spec);
    cfg.validate().map_err(|e| e.to_string())?;
    Ok((cfg, wanted))
}

/// An open device: its detected id and node.
#[derive(Debug, Clone)]
struct DevRef {
    id: String,
    node: PathBuf,
}

/// One surface: find, open, read, repeat (see the module documentation).
async fn surface_task(
    deps: Deps,
    mut specs: watch::Receiver<SurfaceSpec>,
    cancel: CancellationToken,
) {
    let mut last_problem: Option<String> = None;
    // `tcp_midi`: failed connections in a row (the back-off).
    let mut tcp_failures: u32 = 0;
    loop {
        let spec = specs.borrow_and_update().clone();
        let name = spec.name.clone();
        let shown = sanitize::log_safe(&name, 64);
        let mut wait = RESCAN;
        let problem = match prepare(&spec) {
            Err(why) => why,
            Ok((cfg, wanted)) => {
                let opened = match &wanted {
                    Wanted::Hid {
                        vendor_id,
                        product_id,
                        interface,
                    } => {
                        let scan = deps.surfaces.scan().await;
                        scan.find_hid(*vendor_id, *product_id, *interface).map(|n| {
                            let dev = DevRef {
                                id: n.id.clone(),
                                node: n.node.clone(),
                            };
                            let r = reader::open(&dev.node, deps.surfaces.lab).map(|r| {
                                (r, Input::Hid(Decoder::new(&cfg), LastReports::default()))
                            });
                            (dev, false, r)
                        })
                    }
                    Wanted::Midi { card, device } => {
                        let scan = deps.surfaces.scan().await;
                        scan.find_midi(card, *device).map(|m| {
                            let dev = DevRef {
                                id: m.id.clone(),
                                node: m.node.clone(),
                            };
                            let r =
                                reader::open_duplex(&dev.node, deps.surfaces.lab).map(|(r, o)| {
                                    if let Output::Lab(v) = &o {
                                        lock(&deps.surfaces.lab_out)
                                            .insert(dev.id.clone(), Arc::clone(v));
                                    }
                                    (r, Input::Midi(Box::new(MidiInput::new(&cfg, o))))
                                });
                            (dev, true, r)
                        })
                    }
                    Wanted::Tcp { host, port } => {
                        // Connect out (off the async workers: name lookup and
                        // connect block), with a back-off while it fails.
                        let id = tcp_id(host, *port);
                        let (h, p) = (host.clone(), *port);
                        let r = tokio::task::spawn_blocking(move || reader::connect_tcp(&h, p))
                            .await
                            .unwrap_or_else(|e| Err(std::io::Error::other(e.to_string())))
                            .map(|(r, o)| (r, Input::Midi(Box::new(MidiInput::new(&cfg, o)))));
                        if r.is_err() {
                            tcp_failures = tcp_failures.saturating_add(1);
                            wait = tcp_backoff(tcp_failures);
                        }
                        Some((
                            DevRef {
                                node: PathBuf::from(&id),
                                id,
                            },
                            true,
                            r,
                        ))
                    }
                };
                match opened {
                    None => format!("{} not found", wanted.describe()),
                    Some((dev, _, Err(e))) => {
                        format!("cannot open {} ({}): {e}", dev.id, dev.node.display())
                    }
                    Some((dev, held, Ok((reports, input)))) => {
                        last_problem = None;
                        let _hold = HoldGuard {
                            surfaces: &deps.surfaces,
                            id: held.then(|| {
                                lock(&deps.surfaces.holding).insert(dev.id.clone());
                                dev.id.clone()
                            }),
                        };
                        let session = Session::new(&deps, spec.clone(), cfg, dev.clone(), input);
                        let (ended, reported) = session.run(reports, &mut specs, &cancel).await;
                        if matches!(wanted, Wanted::Tcp { .. }) {
                            if reported {
                                // A connection that worked: start the
                                // back-off over.
                                tcp_failures = 0;
                            } else {
                                // Accepted but the desk never spoke: as a
                                // failed connection.
                                tcp_failures = tcp_failures.saturating_add(1);
                            }
                            wait = tcp_backoff(tcp_failures.max(1));
                        }
                        match ended {
                            Ended::Stopped => return,
                            Ended::Reopen => continue,
                            Ended::Lost(why) => format!("{} lost: {why}", dev.id),
                            Ended::Invalid(why) => why,
                        }
                    }
                }
            }
        };
        if last_problem.as_ref() != Some(&problem) {
            tracing::info!("surface `{shown}`: {}", sanitize::log_safe(&problem, 200));
        }
        let latest = specs.borrow().clone();
        deps.surfaces
            .update(&name, |l| *l = Live::of(&latest, Some(problem.clone())));
        last_problem = Some(problem);
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(wait) => {}
            r = specs.changed() => if r.is_err() {
                return;
            },
        }
    }
}

/// The last report of each report id the decoder was fed, to prime a new
/// decoder after an edit.
#[derive(Debug, Default)]
struct LastReports {
    by_first_byte: BTreeMap<u8, Vec<u8>>,
    latest: Vec<u8>,
}

impl LastReports {
    fn keep(&mut self, report: &[u8]) {
        if let Some(&first) = report.first() {
            let kept = self.by_first_byte.entry(first).or_default();
            kept.clear();
            kept.extend_from_slice(report);
        }
        self.latest.clear();
        self.latest.extend_from_slice(report);
    }

    /// Prime `decoder` (numbered reports or not) with the kept state;
    /// fader positions go to `out`.
    fn prime(&self, decoder: &mut Decoder, numbered: bool, out: &mut Vec<SurfaceEvent>) {
        if numbered {
            for r in self.by_first_byte.values() {
                decoder.prime(r, out);
            }
        } else if !self.latest.is_empty() {
            // The state-read layout always starts with a report number.
            let mut r = Vec::with_capacity(self.latest.len() + 1);
            r.push(0);
            r.extend_from_slice(&self.latest);
            decoder.prime(&r, out);
        }
    }
}

/// Does the configuration use numbered reports (as the decoder decides)?
fn numbered(cfg: &SurfaceConfig) -> bool {
    cfg.controls.iter().any(|c| c.report_id.is_some())
}

/// Push-to-talk bindings of `new` that `old` did not have: muted until
/// their button is held.
fn added_push_to_talk(old: &SurfaceConfig, new: &SurfaceConfig) -> Vec<SurfaceAction> {
    new.bindings
        .iter()
        .filter(|b| b.action.is_mute() && b.mode == ButtonMode::PushToTalk)
        .filter(|b| !old.bindings.contains(b))
        .map(|b| mute_request(b, true))
        .collect()
}

/// The fader law a followed control without a fader binding of its own
/// uses: -60..0 dB, dB taper.
fn default_law(control: &str) -> BindingConfig {
    BindingConfig {
        control: control.to_owned(),
        action: tessera_surface::BindingAction::Follow,
        target: tessera_surface::Target::none(),
        ports: Vec::new(),
        crosspoints: Vec::new(),
        follow: None,
        mode: ButtonMode::Toggle,
        min_db: -60.0,
        max_db: 0.0,
        taper: tessera_surface::Taper::Db,
        step_db: 1.0,
        deadband_db: 0.25,
        curve: Vec::new(),
    }
}

/// A MIDI control's address and scale.
#[derive(Debug, Clone, Copy)]
struct MidiCtl {
    addr: MidiAddress,
    button: bool,
}

/// The MIDI side of a session: decoder, the port's output, and what the
/// desk shows of each control (event scale: what it sent last, or what was
/// written to it last).
struct MidiInput {
    decoder: compat::MidiIn,
    out: Output,
    controls: HashMap<String, MidiCtl>,
    desk: HashMap<String, u32>,
    /// Controls a `follow` binding of another surface moved (see [`Echo`]).
    echo: HashMap<String, Echo>,
    /// Why the output failed (the session ends as lost).
    broken: Option<String>,
    /// The next control asked for its value by a liveness probe.
    probe_next: usize,
}

/// A followed move of a control, for [`ECHO`]: the device's report of
/// (about) the position written (a motor reporting where it went) is not a
/// movement, it is dropped; any other report of the control meanwhile is a
/// movement, but is not followed back along `path` (the controls the
/// followed value came through), so a pair of surfaces whose motors land a
/// little off never ping-pong.
#[derive(Debug, Clone)]
struct Echo {
    /// The position written (event scale).
    value: u32,
    at: Instant,
    /// A fader: the law and travel the position was found by.
    law: Option<(BindingConfig, u32)>,
    path: Vec<ControlKey>,
}

/// How long after a followed move the device's reports of the control are
/// its echo (a motor's travel plus a late TCP peer).
const ECHO: Duration = Duration::from_millis(3000);
/// A fader's report within this many dB of the position written (by the
/// control's law) is its echo...
const ECHO_DB: f32 = 0.5;
/// ... or within this fraction of its travel (per mille), wherever the law
/// is steep.
const ECHO_TRAVEL_PERMILLE: u32 = 10;

/// Is a fader at `got` the echo of a move to `want` (see [`ECHO_DB`])?
fn is_fader_echo(law: &BindingConfig, max: u32, want: u32, got: u32) -> bool {
    let travel = (max / (1000 / ECHO_TRAVEL_PERMILLE)).max(2);
    if got.abs_diff(want) <= travel {
        return true;
    }
    let (a, b) = (
        tessera_surface::fader_db(law, got, max),
        tessera_surface::fader_db(law, want, max),
    );
    a.is_finite() && b.is_finite() && (a - b).abs() <= ECHO_DB
}

impl MidiInput {
    fn new(cfg: &SurfaceConfig, out: Output) -> Self {
        let mut m = Self {
            decoder: compat::MidiIn::new(cfg),
            out,
            controls: HashMap::new(),
            desk: HashMap::new(),
            echo: HashMap::new(),
            broken: None,
            probe_next: 0,
        };
        m.respec(cfg);
        m
    }

    /// A new configuration on the same port.
    fn respec(&mut self, cfg: &SurfaceConfig) {
        self.decoder = compat::MidiIn::new(cfg);
        self.controls = cfg
            .controls
            .iter()
            .filter_map(|c| {
                Some((
                    c.id.clone(),
                    MidiCtl {
                        addr: c.midi?,
                        button: c.kind == tessera_surface::ControlKind::Button,
                    },
                ))
            })
            .collect();
        self.desk.retain(|k, _| self.controls.contains_key(k));
    }

    /// Queue one whole message for the device (never blocks: the port's
    /// writer thread writes it). `false` when the output failed: the
    /// session ends as lost, and nothing is taken as shown on the desk.
    fn write(&mut self, what: &str, bytes: &[u8]) -> bool {
        if self.broken.is_some() {
            return false;
        }
        match self.out.write(bytes) {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!("surface MIDI output ({what}): {e}");
                self.broken = Some(format!("the output failed: {e}"));
                false
            }
        }
    }

    /// Ask the desk for the value of every control that has a "get".
    fn request_values(&mut self) {
        let gets: Vec<Vec<u8>> = self
            .controls
            .values()
            .filter_map(|c| compat::midi_get(&c.addr))
            .collect();
        for b in gets {
            if !self.write("get", &b) {
                return;
            }
        }
    }

    /// Whether a control can be asked for its value (a liveness probe gets
    /// an answer).
    fn can_probe(&self) -> bool {
        self.controls
            .values()
            .any(|c| compat::midi_get(&c.addr).is_some())
    }

    /// A liveness probe: ask for the value of the next control that has a
    /// "get" (one message; in turn, so a control the desk does not answer
    /// does not decide). Returns the control asked.
    fn probe(&mut self) -> Option<String> {
        let mut gets: Vec<(&String, Vec<u8>)> = self
            .controls
            .iter()
            .filter_map(|(id, c)| Some((id, compat::midi_get(&c.addr)?)))
            .collect();
        if gets.is_empty() {
            return None;
        }
        gets.sort_by(|a, b| a.0.cmp(b.0));
        let i = self.probe_next % gets.len();
        self.probe_next = i + 1;
        let (id, b) = gets.swap_remove(i);
        let id = id.clone();
        self.write("probe", &b);
        Some(id)
    }

    /// Remember what the desk sent.
    fn seen(&mut self, events: &[SurfaceEvent]) {
        for e in events {
            let v = match e.kind {
                SurfaceEventKind::Button { pressed } => u32::from(pressed),
                SurfaceEventKind::Fader { value, .. } => value,
                _ => continue,
            };
            if self.controls.contains_key(&e.control) {
                self.desk.insert(e.control.clone(), v);
            }
        }
    }

    /// Move a fader (event scale: 0 is the bottom). Returns the event that
    /// tells the mapper the fader got there (the desk need not echo); also
    /// when the desk already shows that position (nothing is written), so a
    /// new mapper that asked for the move takes the fader over.
    fn move_fader(&mut self, control: &str, value: u32) -> Option<SurfaceEvent> {
        if self.desk.get(control) != Some(&value) {
            let bytes = self.decoder.output(control, value)?;
            if !self.write("fader", &bytes) {
                return None;
            }
            self.desk.insert(control.to_owned(), value);
        }
        self.decoder.moved(control, value)
    }

    /// After an edit: the position the desk last sent of every fader
    /// without a motor in `cfg`, as events (the decoder takes them as its
    /// baseline, so the desk's next report of the same position is no
    /// movement).
    fn last_positions(&mut self, cfg: &SurfaceConfig) -> Vec<SurfaceEvent> {
        cfg.controls
            .iter()
            .filter(|c| c.kind == tessera_surface::ControlKind::Fader && !c.motorized)
            .filter_map(|c| {
                let v = *self.desk.get(&c.id)?;
                self.decoder.moved(&c.id, v)
            })
            .collect()
    }

    /// Drive `control` to `value` for a `follow` binding (event scale; a
    /// fader's position found by `law`) that came through `path`, and
    /// expect the device's echo (see [`Echo`]). Returns the event that
    /// shows the new position on the desk (the live value; the session
    /// hands the move to the mapper itself).
    fn drive(
        &mut self,
        control: &str,
        value: u32,
        law: Option<(BindingConfig, u32)>,
        path: &[ControlKey],
    ) -> Option<SurfaceEvent> {
        if !self.controls.contains_key(control) {
            return None;
        }
        if self.desk.get(control) != Some(&value) {
            let bytes = self.decoder.output(control, value)?;
            if !self.write("follow", &bytes) {
                return None;
            }
            self.desk.insert(control.to_owned(), value);
        }
        self.echo.insert(
            control.to_owned(),
            Echo {
                value,
                at: Instant::now(),
                law,
                path: path.to_vec(),
            },
        );
        // The decoder's baseline: the same position reported back is no
        // change.
        self.decoder.moved(control, value)
    }

    /// Remove from `events` the device's echoes of followed moves made
    /// within `window` (see [`Echo`]).
    fn drop_echoes(&mut self, events: &mut Vec<SurfaceEvent>, window: Duration) {
        if self.echo.is_empty() {
            return;
        }
        let now = Instant::now();
        self.echo.retain(|_, e| now.duration_since(e.at) < window);
        events.retain(|ev| {
            let Some(e) = self.echo.get(&ev.control) else {
                return true;
            };
            match (ev.kind, &e.law) {
                (SurfaceEventKind::Fader { value, .. }, Some((law, max))) => {
                    !is_fader_echo(law, *max, e.value, value)
                }
                (SurfaceEventKind::Fader { value, .. }, None) => value.abs_diff(e.value) > 2,
                (SurfaceEventKind::Button { pressed }, _) => u32::from(pressed) != e.value,
                (SurfaceEventKind::Encoder { .. }, _) => true,
            }
        });
    }

    /// The path of the followed move of `control` whose echo window (of
    /// length `window`) is still open: a report of it meanwhile is not
    /// followed back along it.
    fn echo_path(&self, control: &str, window: Duration) -> Option<&[ControlKey]> {
        self.echo
            .get(control)
            .filter(|e| e.at.elapsed() < window)
            .map(|e| e.path.as_slice())
    }

    /// Show a mute button's state (unless the desk already shows it).
    fn show_mute(&mut self, control: &str, muted: bool) {
        if !self.controls.get(control).is_some_and(|c| c.button) {
            return;
        }
        let v = u32::from(muted);
        if self.desk.get(control) == Some(&v) {
            return;
        }
        if let Some(bytes) = self.decoder.output(control, v) {
            if self.write("mute", &bytes) {
                self.desk.insert(control.to_owned(), v);
            }
        }
    }
}

/// How a session reads its device.
enum Input {
    /// HID reports: the decoder and the last report of each id.
    Hid(Decoder, LastReports),
    /// A MIDI byte stream.
    Midi(Box<MidiInput>),
}

/// Mute of every value a binding covers (channel, output, host, router port,
/// crosspoint), as the session last saw it.
type Mutes = HashMap<Level, bool>;

/// Registers a session for `follow` values while it lives.
struct FollowGuard<'a> {
    surfaces: &'a Surfaces,
    name: String,
    tx: mpsc::UnboundedSender<Followed>,
}

impl Drop for FollowGuard<'_> {
    fn drop(&mut self) {
        let mut map = lock(&self.surfaces.follow);
        if map
            .get(&self.name)
            .is_some_and(|t| t.same_channel(&self.tx))
        {
            map.remove(&self.name);
        }
    }
}

/// A surface on an open device.
struct Session<'a> {
    deps: &'a Deps,
    spec: SurfaceSpec,
    cfg: SurfaceConfig,
    dev: DevRef,
    origin: Origin,
    input: Input,
    mapper: Mapper,
    mutes: Mutes,
    /// The connection was reported (after the first report).
    reported: bool,
    /// The state engine's connection epoch the connection is (to be)
    /// reported under: read when the session starts, then the one of the
    /// last report (`None`: unknown, reported without the check).
    epoch: Option<u64>,
}

impl<'a> Session<'a> {
    fn new(
        deps: &'a Deps,
        spec: SurfaceSpec,
        cfg: SurfaceConfig,
        dev: DevRef,
        input: Input,
    ) -> Self {
        Self {
            deps,
            origin: Origin::Surface {
                name: cfg.name.clone(),
            },
            input,
            mapper: Mapper::new(&cfg),
            mutes: Mutes::new(),
            spec,
            cfg,
            dev,
            reported: false,
            epoch: None,
        }
    }

    fn name(&self) -> &str {
        &self.cfg.name
    }

    /// The connection was reported under `epoch`: kept here and in the
    /// live state (for [`stop`]).
    fn set_epoch(&mut self, epoch: Option<u64>) {
        self.epoch = epoch;
        self.deps.surfaces.update(self.name(), |l| l.epoch = epoch);
    }

    /// Requests that come from no control (start-up, reset, an edit).
    async fn submit(&mut self, actions: Vec<SurfaceAction>) {
        self.submit_from(actions, &[]).await;
    }

    /// Requests of a control's movement that came through `path` (its own
    /// key last): a `follow` request goes on with that path.
    async fn submit_from(&mut self, actions: Vec<SurfaceAction>, path: &[ControlKey]) {
        submit_all(
            &self.deps.engine,
            &self.deps.surfaces,
            &self.origin,
            &mut self.mapper,
            actions,
            path,
        )
        .await;
    }

    fn key(&self, control: &str) -> ControlKey {
        (self.name().to_owned(), control.to_owned())
    }

    /// A `follow` binding of another surface drives `control` of this one,
    /// and the control acts as if it had moved there. The value becomes
    /// this control's position by its first fader binding's law (the
    /// default law, -60..0 dB, without one), written to the device (a
    /// button's light shows the state); then the position goes to the
    /// mapper as this control's own movement (origin: this surface), so its
    /// bindings apply through their own laws and its own `follow` bindings
    /// pass the value on (never back through a control it came through).
    /// A HID surface has no output: that is its problem, its bindings still
    /// apply.
    async fn followed(&mut self, f: &Followed) {
        let me = self.key(&f.control);
        // A learn watches the device: the surface pauses (a move written
        // now would come back and be learned).
        if f.path.contains(&me) || self.deps.surfaces.learning(&self.dev.id) {
            return;
        }
        let Some(c) = self.cfg.controls.iter().find(|c| c.id == f.control) else {
            return;
        };
        let (value, kind, law) = match (f.value, c.kind) {
            (FollowValue::Fader { gain_db }, tessera_surface::ControlKind::Fader) => {
                let max = c.max.unwrap_or_else(|| c.field_max());
                let law = self
                    .cfg
                    .bindings
                    .iter()
                    .find(|b| {
                        b.control == c.id
                            && (b.action.is_gain()
                                || b.action == tessera_surface::BindingAction::Follow)
                    })
                    .cloned()
                    .unwrap_or_else(|| default_law(&c.id));
                let value = fader_position(&law, gain_db, max);
                (
                    value,
                    SurfaceEventKind::Fader { value, max },
                    Some((law, max)),
                )
            }
            (FollowValue::Button { on }, tessera_surface::ControlKind::Button) => (
                u32::from(on),
                SurfaceEventKind::Button { pressed: on },
                None,
            ),
            _ => return,
        };
        let mut path = f.path.clone();
        path.push(me);
        match &mut self.input {
            Input::Midi(m) => {
                // Written (or shown already): the control's value, as a
                // motor move's.
                let shown = m.drive(&f.control, value, law, &path);
                record_values(&self.deps.surfaces, self.name(), shown.as_slice(), true);
            }
            Input::Hid(..) => {
                let why = format!(
                    "control `{}` is followed, but a HID surface takes no output",
                    sanitize::log_safe(&f.control, 64)
                );
                self.deps.surfaces.update(self.name(), |l| {
                    if l.problem.is_none() {
                        l.problem = Some(why);
                    }
                });
            }
        }
        let ev = SurfaceEvent {
            control: f.control.clone(),
            kind,
        };
        let mut actions = Vec::new();
        self.mapper.driven(&ev, &mut actions);
        self.submit_from(actions, &path).await;
    }

    /// Until the device goes away (or falls silent, see
    /// [`Surfaces::set_liveness`]), the surface is stopped or an edit needs
    /// another device. Also returns whether the device reported anything.
    async fn run(
        mut self,
        mut reports: Reports,
        specs: &mut watch::Receiver<SurfaceSpec>,
        cancel: &CancellationToken,
    ) -> (Ended, bool) {
        let id = self.dev.id.clone();
        self.deps.surfaces.update(self.name(), |l| {
            l.connected = false;
            l.device = Some(id);
            l.problem = Some(WAITING.into());
            // A device opened afresh has reported nothing yet.
            for c in &mut l.controls {
                c.value = None;
            }
        });
        let mut sub = self.deps.bus.subscribe(1024);
        // The connection epoch the first report is made under (subscribed
        // first: a change after this read is seen as `SurfacesChanged`).
        self.epoch = engine_view(&self.deps.engine, self.name())
            .await
            .map(|s| s.connection_epoch);
        // Values other surfaces' `follow` bindings send to this one.
        let (ftx, mut follows) = mpsc::unbounded_channel();
        lock(&self.deps.surfaces.follow).insert(self.name().to_owned(), ftx.clone());
        let surfaces = Arc::clone(&self.deps.surfaces);
        let _follow = FollowGuard {
            surfaces: &surfaces,
            name: self.name().to_owned(),
            tx: ftx,
        };
        let moves = observe_status(
            &self.deps.engine,
            &self.cfg,
            &mut self.mapper,
            &mut self.mutes,
            &sub,
        )
        .await;
        self.feedback(&moves, false).await;
        // Push-to-talk targets are muted from the start (mute is never
        // owned: no connection needed).
        let startup = self.mapper.startup_actions();
        self.submit(startup).await;
        if let Input::Midi(m) = &mut self.input {
            m.request_values();
        }

        // Liveness: the last bytes from the device and the last probe.
        let mut heard = Instant::now();
        let mut probed = heard;
        // Silence only counts against a device that answers our value
        // requests: an idle desk that talks only on change (the CQ over
        // USB does not answer a get) is not dead. `asked`: the control
        // asked for its value (`None`: all of them, `request_values`) and
        // when; `answers`: it answered one in this session; `quiet`: it
        // never answered within a liveness period, so it is not probed
        // again (device errors and TCP keep-alive still end the session).
        let mut asked: Option<(Option<String>, Instant)> = Some((None, heard));
        let mut answers = false;
        let mut quiet = false;
        // A held 7-bit NRPN button value is passed on at this time.
        let mut held_until: Option<Instant> = None;
        let mut events: Vec<SurfaceEvent> = Vec::new();
        let ended = loop {
            if let Input::Midi(m) = &mut self.input {
                if let Some(why) = m.broken.take() {
                    break Ended::Lost(why);
                }
            }
            let live = match &self.input {
                Input::Midi(m) if m.can_probe() && !quiet => self.deps.surfaces.liveness(),
                _ => None,
            };
            let check = live.map(|l| next_check(heard, probed, l));
            tokio::select! {
                () = sleep_until_opt(held_until) => {
                    held_until = None;
                    let Input::Midi(m) = &mut self.input else {
                        continue;
                    };
                    events.clear();
                    m.decoder.flush(&mut events);
                    m.seen(&events);
                    m.drop_echoes(&mut events, self.deps.surfaces.echo_window());
                    self.handle(&events, true).await;
                }
                () = sleep_until_opt(check) => {
                    let (Some(l), Input::Midi(m)) = (live, &mut self.input) else {
                        continue;
                    };
                    let now = Instant::now();
                    if self.deps.surfaces.learning(&self.dev.id) {
                        // A learn watches the device: no probe (its answer
                        // would be learned as a control that moved), and
                        // the silence is not counted; liveness starts over
                        // when the learn ends.
                        heard = now;
                        probed = now;
                        continue;
                    }
                    if now.duration_since(heard) >= l {
                        if !answers {
                            // It never answered a value request: its
                            // silence says nothing; stop probing it.
                            tracing::debug!(
                                surface = %sanitize::log_safe(&self.spec.name, 64),
                                "the device does not answer value requests: no liveness check"
                            );
                            quiet = true;
                            continue;
                        }
                        break Ended::Lost(format!(
                            "nothing received for {} ms (the desk does not answer)",
                            l.as_millis()
                        ));
                    }
                    if now.duration_since(heard) >= l / 3 && now.duration_since(probed) >= l / 3 {
                        asked = m.probe().map(|id| (Some(id), now));
                        probed = now;
                    }
                }
                () = cancel.cancelled() => break Ended::Stopped,
                r = specs.changed() => {
                    if r.is_err() {
                        break Ended::Stopped;
                    }
                    let spec = specs.borrow_and_update().clone();
                    if let Some(end) = self.edit(spec, &sub).await {
                        break end;
                    }
                    // The edit asked the desk for its values again (its
                    // controls may be new): liveness starts over, as at
                    // open.
                    let now = Instant::now();
                    heard = now;
                    probed = now;
                    asked = Some((None, now));
                    answers = false;
                    quiet = false;
                }
                r = reports.next() => match r {
                    Err(why) => break Ended::Lost(why),
                    Ok(report) => {
                        heard = Instant::now();
                        // A learn is watching this device: hands off (a
                        // learn on a held MIDI port gets the bytes).
                        if self.deps.surfaces.learning(&self.dev.id) {
                            self.deps.surfaces.tap(&self.dev.id, &report);
                            continue;
                        }
                        events.clear();
                        match &mut self.input {
                            Input::Hid(decoder, last) => {
                                last.keep(&report);
                                decoder.feed(&report, &mut events);
                            }
                            Input::Midi(m) => {
                                m.decoder.feed(&report, &mut events);
                                m.seen(&events);
                                // An answer to our value request (the
                                // control asked, soon after): the device
                                // answers, so its silence counts from now.
                                // Matched on the controls the bytes
                                // addressed, also with an unchanged value
                                // (which decodes to no event), except one
                                // whose followed move's echo is expected.
                                let window = self.deps.surfaces.echo_window();
                                let addressed = m.decoder.take_heard();
                                if let Some((id, at)) = &asked {
                                    // Answers come at once: half a second at most, so a hand
                                    // move after a probe is not taken for one.
                                    let within = (self.deps.surfaces.liveness().unwrap_or(LIVENESS) / 3)
                                        .min(Duration::from_millis(500));
                                    if at.elapsed() <= within
                                        && addressed.iter().any(|c| {
                                            id.as_ref().is_none_or(|i| c == i)
                                                && m.echo_path(c, window).is_none()
                                        })
                                    {
                                        answers = true;
                                        asked = None;
                                    }
                                }
                                m.drop_echoes(&mut events, window);
                                held_until = m.decoder.has_held().then(|| {
                                    Instant::now()
                                        + Duration::from_millis(tessera_surface::midi::NRPN_HOLD_MS)
                                });
                            }
                        }
                        if !self.reported {
                            self.connect().await;
                        }
                        self.handle(&events, true).await;
                    }
                },
                Some(f) = follows.recv() => self.followed(&f).await,
                e = sub.recv_async() => match e {
                    Ok(Received::Event(Event::SurfacesChanged { .. })) => {
                        self.reassert_connection(&sub).await;
                    }
                    Ok(Received::Event(ev)) => {
                        let moves = observe_event(&mut self.mapper, &mut self.mutes, &self.cfg, &ev);
                        let muted = matches!(
                            ev,
                            Event::ChannelMuteChanged { .. }
                                | Event::OutputMuteChanged { .. }
                                | Event::HostVolumeChanged { .. }
                                | Event::PortMuteChanged { .. }
                                | Event::CrosspointMuteChanged { .. }
                        );
                        self.feedback(&moves, muted).await;
                    }
                    Ok(Received::Lagged(_)) => {
                        let moves = observe_status(
                            &self.deps.engine,
                            &self.cfg,
                            &mut self.mapper,
                            &mut self.mutes,
                            &sub,
                        )
                        .await;
                        self.feedback(&moves, true).await;
                        self.reassert_connection(&sub).await;
                    }
                    // The engine is gone: the daemon is stopping.
                    Err(_) => break Ended::Stopped,
                },
            }
        };
        drop(reports);
        self.finish(&ended).await;
        (ended, self.reported)
    }

    /// Feedback to a MIDI desk: the faders the mapper asks to move (then
    /// told to the mapper as arrived: a desk need not echo), and (`mutes`:
    /// a mute changed) the state of every mute button.
    async fn feedback(&mut self, moves: &[SurfaceAction], mutes: bool) {
        let Input::Midi(m) = &mut self.input else {
            return;
        };
        let mut arrived = Vec::new();
        for a in moves {
            if let SurfaceAction::MoveFader { control, value } = a {
                arrived.extend(m.move_fader(control, *value));
            }
        }
        if mutes {
            for b in self.cfg.bindings.iter().filter(|b| b.action.is_mute()) {
                let levels = b.levels();
                let all = levels
                    .iter()
                    .all(|l| self.mutes.get(l).copied().unwrap_or(false));
                let known = levels.iter().any(|l| self.mutes.contains_key(l));
                if known {
                    m.show_mute(&b.control, all);
                }
            }
        }
        // The desk took these positions (the write was queued, or it shows
        // them already): the live value of each, also for a control that
        // has not reported (its own echo of the move is no change, so it
        // would stay unknown). To the mapper they are arrivals, not
        // reports: no hand moved them.
        record_values(&self.deps.surfaces, self.name(), &arrived, true);
        self.handle(&arrived, false).await;
    }

    /// The first report was decoded: report the connection (the state
    /// engine gives the faders their channels' gain now), under the epoch
    /// read at the start (a stale one is read again). A refusal (the
    /// surface was disabled meanwhile: the runtime stops the session) is
    /// left to [`Session::reassert_connection`].
    async fn connect(&mut self) {
        self.reported = true;
        let reported = report_connected(
            &self.deps.engine,
            self.name(),
            self.epoch,
            &self.spec.source,
        )
        .await;
        match reported {
            Ok(epoch) => self.set_epoch(epoch),
            Err(e) => tracing::info!(
                "surface `{}`: connection not taken by the state engine: {e}",
                sanitize::log_safe(self.name(), 64)
            ),
        }
        self.deps.surfaces.update(self.name(), |l| {
            l.connected = true;
            l.problem = None;
        });
        tracing::info!(
            "surface `{}`: connected to {} ({})",
            sanitize::log_safe(self.name(), 64),
            self.dev.id,
            self.dev.node.display()
        );
    }

    /// Decoded events: live values, then the mapper's requests. `reported`:
    /// the device sent them (otherwise they only update the live value of a
    /// control that has reported before).
    ///
    /// A control a followed move drove reports during its echo window: its
    /// `follow` requests do not go back along the move's path (see
    /// [`Echo`]).
    async fn handle(&mut self, events: &[SurfaceEvent], reported: bool) {
        if events.is_empty() {
            return;
        }
        record_values(&self.deps.surfaces, self.name(), events, reported);
        let window = self.deps.surfaces.echo_window();
        for e in events {
            let mut path = match (&self.input, reported) {
                (Input::Midi(m), true) => m
                    .echo_path(&e.control, window)
                    .map(<[ControlKey]>::to_vec)
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            let me = self.key(&e.control);
            if path.last() != Some(&me) {
                path.push(me);
            }
            let mut actions = Vec::new();
            self.mapper.handle(e, &mut actions);
            self.submit_from(actions, &path).await;
        }
    }

    /// The state engine dropped this surface's connection while the session
    /// holds the device and has reported it: its connection epoch is not
    /// the one reported under, or it shows the surface not connected (a
    /// disable and enable the runtime saw only as the same spec, ...).
    /// Report the connection again under the new epoch, read the current
    /// levels again (what was set while the connection was dropped, whose
    /// events may still be queued behind this `SurfacesChanged`), then
    /// apply the last position of every fader without a motor (it owns its
    /// levels again, at its real position: a level set meanwhile is not
    /// taken for the fader's own). Read on every `SurfacesChanged` and
    /// after a lag. Nothing for a surface disabled or moved to another
    /// source (the runtime stops or reopens the session).
    async fn reassert_connection(&mut self, sub: &Subscription) {
        if !self.reported {
            return;
        }
        let Some(st) = engine_view(&self.deps.engine, self.name()).await else {
            return;
        };
        if !st.spec.enabled || st.spec.source != self.spec.source {
            return;
        }
        let epoch = st.connection_epoch;
        if st.connected && self.epoch.is_none_or(|e| e == epoch) {
            // Ours (only this session reports this surface).
            self.set_epoch(Some(epoch));
            return;
        }
        let reported = report_connected(
            &self.deps.engine,
            self.name(),
            Some(epoch),
            &self.spec.source,
        )
        .await;
        match reported {
            Ok(e) => self.set_epoch(e),
            Err(e) => {
                tracing::debug!(
                    "surface `{}`: connection not taken again: {e}",
                    sanitize::log_safe(self.name(), 64)
                );
                return;
            }
        }
        tracing::info!(
            "surface `{}`: connection reported again under epoch {} (the state \
             engine had dropped it)",
            sanitize::log_safe(self.name(), 64),
            self.epoch.unwrap_or(epoch)
        );
        // The levels now (this drains the queued events the read covers):
        // a level set elsewhere while the connection was dropped is then
        // no longer what a fader last asked for, so its position below is
        // applied, not taken as already there.
        let moves = observe_status(
            &self.deps.engine,
            &self.cfg,
            &mut self.mapper,
            &mut self.mutes,
            sub,
        )
        .await;
        self.feedback(&moves, true).await;
        let mut positions = Vec::new();
        match &mut self.input {
            Input::Hid(decoder, last) => {
                let mut d = Decoder::new(&self.cfg);
                last.prime(&mut d, numbered(&self.cfg), &mut positions);
                *decoder = d;
                positions.retain(|e| {
                    self.cfg
                        .controls
                        .iter()
                        .any(|c| c.id == e.control && !c.motorized)
                });
            }
            Input::Midi(m) => positions = m.last_positions(&self.cfg),
        }
        let replayed = matches!(self.input, Input::Hid(..));
        self.handle(&positions, replayed).await;
    }

    /// Apply an edited spec on the open device (see the module
    /// documentation); `Some` when the session must end instead.
    async fn edit(&mut self, spec: SurfaceSpec, sub: &Subscription) -> Option<Ended> {
        if spec == self.spec {
            return None;
        }
        if spec.source != self.spec.source {
            return Some(Ended::Reopen);
        }
        let cfg = match prepare(&spec) {
            Ok((cfg, _)) => cfg,
            Err(why) => return Some(Ended::Invalid(why)),
        };
        let mut primed = Vec::new();
        match &mut self.input {
            Input::Hid(decoder, last) => {
                let mut d = Decoder::new(&cfg);
                if self.reported {
                    last.prime(&mut d, numbered(&cfg), &mut primed);
                }
                *decoder = d;
            }
            Input::Midi(m) => {
                m.respec(&cfg);
                if self.reported {
                    primed = m.last_positions(&cfg);
                }
            }
        }
        let mut mapper = Mapper::new(&cfg);
        let mut mutes = Mutes::new();
        let moves = observe_status(&self.deps.engine, &cfg, &mut mapper, &mut mutes, sub).await;
        let added = added_push_to_talk(&self.cfg, &cfg);
        self.mapper = mapper;
        self.mutes = mutes;
        self.cfg = cfg;
        self.deps.surfaces.update(&spec.name, |l| l.relist(&spec));
        self.spec = spec;
        tracing::info!(
            "surface `{}`: configuration changed; applied on the open device",
            sanitize::log_safe(self.name(), 64)
        );
        self.submit(added).await;
        self.feedback(&moves, true).await;
        // Faders without a motor apply their last position under the new
        // bindings (motorized ones were moved above; a desk is asked for its
        // values again). HID: replayed reports. MIDI: what the desk showed,
        // shown only for controls it has reported.
        let replayed = matches!(self.input, Input::Hid(..));
        self.handle(&primed, replayed).await;
        if let Input::Midi(m) = &mut self.input {
            m.request_values();
        }
        None
    }

    /// The session ended: when lost (unplugged, silent, its input or output
    /// failed, a full output queue too: see the module documentation), make
    /// the targets safe; always report the disconnection (if the connection
    /// was reported).
    async fn finish(&mut self, ended: &Ended) {
        if self.reported {
            if matches!(ended, Ended::Lost(_)) {
                let resets = self.mapper.reset();
                self.submit(resets).await;
            }
            // Under an epoch that changed meanwhile this changes nothing:
            // the state engine dropped that connection already.
            let _ = report_connection(&self.deps.engine, self.name(), false, self.epoch).await;
        }
        self.deps.surfaces.update(self.name(), |l| {
            l.connected = false;
            l.device = None;
            l.epoch = None;
        });
        let shown = sanitize::log_safe(self.name(), 64);
        match ended {
            Ended::Lost(why) => tracing::info!(
                "surface `{shown}`: disconnected from {} ({})",
                self.dev.id,
                sanitize::log_safe(why, 120)
            ),
            _ => tracing::info!("surface `{shown}`: released {}", self.dev.id),
        }
    }
}

/// When a session with liveness `l` next looks at it: a probe a third of
/// `l` after the last bytes heard (or the last probe), the end at `l`.
fn next_check(heard: Instant, probed: Instant, l: Duration) -> Instant {
    let third = l / 3;
    (heard.max(probed) + third).min(heard + l)
}

/// Sleep until `at`; never without one.
async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// Keep the last value of each control that reported one. `reported`: the
/// device sent `events`, or took them (a motor move whose write was
/// taken); otherwise (a position replayed to the mapper) a control that has
/// not reported keeps no value.
fn record_values(surfaces: &Surfaces, name: &str, events: &[SurfaceEvent], reported: bool) {
    surfaces.update(name, |l| {
        for e in events {
            if let (Some(c), Some(v)) = (
                l.controls.iter_mut().find(|c| c.id == e.control),
                compat::value_of(&e.kind),
            ) {
                if reported || c.value.is_some() {
                    c.value = Some(v);
                }
            }
        }
    });
}

fn chan(output: bool, device: &str, direction: Direction, channel: u16) -> Level {
    Level::Channel {
        output,
        device: device.to_owned(),
        direction,
        channel,
    }
}

/// Is the event this surface's own (its origin)?
fn own(origin: &Origin, cfg: &SurfaceConfig) -> bool {
    matches!(origin, Origin::Surface { name } if *name == cfg.name)
}

/// One mute observation, if a binding covers the level.
fn see_mute(mapper: &mut Mapper, mutes: &mut Mutes, level: Level, muted: bool) {
    if mapper.covers(&level) {
        mutes.insert(level.clone(), muted);
        mapper.observe_level_mute(level, muted);
    }
}

/// One gain observation, if a binding covers the level.
fn see_gain(
    mapper: &mut Mapper,
    level: Level,
    gain: tessera_control_types::Gain,
    own: bool,
    moves: &mut Vec<SurfaceAction>,
) {
    if mapper.covers(&level) {
        mapper.observe_level_gain(level, compat::db_of_gain(gain), own, moves);
    }
}

/// Mute and gain events of every value a binding covers (channels and their
/// output stage, the host volume, router ports and crosspoints) into the
/// mapper (and `mutes`); returns the faders the mapper asks to move.
fn observe_event(
    mapper: &mut Mapper,
    mutes: &mut Mutes,
    cfg: &SurfaceConfig,
    ev: &Event,
) -> Vec<SurfaceAction> {
    let mut moves = Vec::new();
    match ev {
        Event::ChannelMuteChanged { target, muted, .. }
        | Event::OutputMuteChanged { target, muted, .. } => {
            let output = matches!(ev, Event::OutputMuteChanged { .. });
            for c in &target.channels {
                let l = chan(output, &target.device.0, target.direction, c.get());
                see_mute(mapper, mutes, l, *muted);
            }
        }
        Event::ChannelGainChanged {
            target,
            gain,
            origin,
        }
        | Event::OutputGainChanged {
            target,
            gain,
            origin,
        } => {
            let output = matches!(ev, Event::OutputGainChanged { .. });
            for c in &target.channels {
                let l = chan(output, &target.device.0, target.direction, c.get());
                see_gain(mapper, l, *gain, own(origin, cfg), &mut moves);
            }
        }
        Event::HostVolumeChanged {
            gain,
            muted,
            origin,
        } => {
            see_mute(mapper, mutes, Level::Host, *muted);
            if let Some(g) = gain {
                see_gain(mapper, Level::Host, *g, own(origin, cfg), &mut moves);
            }
        }
        Event::PortMuteChanged { ports, muted, .. } => {
            for p in ports {
                see_mute(mapper, mutes, Level::of_port(p), *muted);
            }
        }
        Event::PortGainChanged {
            ports,
            gain,
            origin,
        } => {
            for p in ports {
                see_gain(
                    mapper,
                    Level::of_port(p),
                    *gain,
                    own(origin, cfg),
                    &mut moves,
                );
            }
        }
        Event::CrosspointMuteChanged {
            crosspoints, muted, ..
        } => {
            for x in crosspoints {
                see_mute(mapper, mutes, Level::Crosspoint(x.clone()), *muted);
            }
        }
        Event::CrosspointGainChanged {
            crosspoints,
            gain,
            origin,
        } => {
            for x in crosspoints {
                let l = Level::Crosspoint(x.clone());
                see_gain(mapper, l, *gain, own(origin, cfg), &mut moves);
            }
        }
        _ => {}
    }
    moves
}

/// The current mute and gain of every value a binding covers into the
/// mapper (and `mutes`): channels and their output stage from the status,
/// the host volume from the status, local router ports and crosspoints from
/// `Query::Router` (only read when a binding needs it); returns the faders
/// the mapper asks to move. Drains `sub` first: what was queued is covered
/// by the read, and applied after it an older event would revert a newer
/// value.
async fn observe_status(
    engine: &EngineHandle,
    cfg: &SurfaceConfig,
    mapper: &mut Mapper,
    mutes: &mut Mutes,
    sub: &Subscription,
) -> Vec<SurfaceAction> {
    drain(sub);
    let mut moves = Vec::new();
    let status = match engine.query(Query::Status).await {
        Ok(Reply::Status(s)) => s,
        Ok(_) => return moves,
        Err(e) => {
            tracing::debug!(
                "surface `{}`: status read failed: {e}",
                sanitize::log_safe(&cfg.name, 64)
            );
            return moves;
        }
    };
    for d in &status.devices {
        for (dir, chans) in [
            (Direction::Rx, &d.rx_channels),
            (Direction::Tx, &d.tx_channels),
        ] {
            for c in chans {
                let ch = c.channel.get();
                let dev = &d.key.0;
                see_mute(mapper, mutes, chan(false, dev, dir, ch), c.muted);
                see_gain(mapper, chan(false, dev, dir, ch), c.gain, false, &mut moves);
                if dir == Direction::Rx {
                    see_mute(mapper, mutes, chan(true, dev, dir, ch), c.output_muted);
                    let l = chan(true, dev, dir, ch);
                    see_gain(mapper, l, c.output_gain, false, &mut moves);
                }
            }
        }
    }
    if let Some(h) = &status.host_volume {
        see_mute(mapper, mutes, Level::Host, h.muted);
        if let Some(g) = h.gain {
            see_gain(mapper, Level::Host, g, false, &mut moves);
        }
    }
    let routed = mapper
        .covered()
        .iter()
        .any(|l| matches!(l, Level::Port(_) | Level::Crosspoint(_)));
    if routed {
        if let Ok(Reply::Router(r)) = engine.query(Query::Router).await {
            for p in r
                .ports
                .iter()
                .filter(|p| crate::router::is_local_level(&p.port))
            {
                let l = Level::Port(p.port.clone());
                see_mute(mapper, mutes, l.clone(), p.muted);
                see_gain(mapper, l, p.gain, false, &mut moves);
            }
            for x in &r.crosspoints {
                let l = Level::Crosspoint(x.crosspoint.key());
                see_mute(mapper, mutes, l.clone(), x.crosspoint.muted);
                see_gain(mapper, l, x.crosspoint.gain, false, &mut moves);
            }
        }
    }
    moves
}

/// Tell the state engine the surface connected or disconnected (it decides
/// fader ownership from this), under connection epoch `epoch` (`None`: no
/// epoch check). A refusal is logged and returned.
async fn report_connection(
    engine: &EngineHandle,
    name: &str,
    connected: bool,
    epoch: Option<u64>,
) -> Result<(), CommandError> {
    let cmd = Command::ReportSurfaceConnection {
        name: name.to_owned(),
        connected,
        epoch,
    };
    let r = engine.apply(Origin::Internal, cmd).await.map(|_| ());
    if let Err(e) = &r {
        tracing::debug!(
            "surface `{}`: connection report refused: {e}",
            sanitize::log_safe(name, 64)
        );
    }
    r
}

/// The state engine's view of surface `name`: whether its connection is
/// reported, under which epoch, and its spec (`None`: not readable, or no
/// such surface).
async fn engine_view(engine: &EngineHandle, name: &str) -> Option<SurfaceStatus> {
    saved(engine)
        .await
        .ok()?
        .into_iter()
        .find(|s| s.spec.name == name)
}

/// Report surface `name` connected under the epoch `epoch` read before
/// (`None`: unknown, no epoch check), for a session on `source`; a refusal
/// for a stale epoch re-reads the surface and, while it is still enabled
/// with that source (a session on an old source must never claim the new
/// epoch), reports again under the new epoch (a few times). Returns the
/// epoch the connection is reported under; the refusal when it is not
/// (`Invalid` on `connected`: the surface is disabled; `epoch`: it changed
/// to a spec this session does not run; or gone, or the engine cannot be
/// read).
async fn report_connected(
    engine: &EngineHandle,
    name: &str,
    epoch: Option<u64>,
    source: &SurfaceSource,
) -> Result<Option<u64>, CommandError> {
    let mut epoch = epoch;
    let mut last = None;
    for _ in 0..3 {
        match report_connection(engine, name, true, epoch).await {
            Ok(()) => return Ok(epoch),
            Err(CommandError::Invalid { field, reason }) if field == "epoch" => {
                last = Some(CommandError::Invalid { field, reason });
                match engine_view(engine, name).await {
                    Some(st) if st.spec.enabled && st.spec.source == *source => {
                        epoch = Some(st.connection_epoch);
                    }
                    _ => break,
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| CommandError::NotFound {
        what: format!("surface `{}`", sanitize::log_safe(name, 64)),
    }))
}

/// Apply each request as a command from the surface; a refused one goes
/// back to the mapper. A `follow` request goes to the followed surface's
/// session with `path` (the controls the movement came through).
async fn submit_all(
    engine: &EngineHandle,
    surfaces: &Surfaces,
    origin: &Origin,
    mapper: &mut Mapper,
    actions: Vec<SurfaceAction>,
    path: &[ControlKey],
) {
    for a in actions {
        // Control routing: to the followed surface's session.
        if let SurfaceAction::Follow { to, value } = &a {
            surfaces.follow(to, *value, path);
            continue;
        }
        let Some(cmd) = compat::command_of(&a) else {
            continue;
        };
        let what = cmd.kind();
        if let Err(e) = engine.apply(origin.clone(), cmd).await {
            tracing::debug!("surface command {what} refused: {e}");
            mapper.rejected(&a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_engine::{ONE_DEVICE, TestEngine};
    use std::path::Path;
    use tessera_control_types::{
        BindingAction as Ba, ButtonMode as Bm, Channel, ChannelSet, ControlKind, ControlValue,
        DeviceKey, Direction, SurfaceBinding, SurfaceControl,
    };

    const SC3: &str = "hid:3142:0c33:3";

    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/surfaces/sc3")
    }

    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap() {
            let e = e.unwrap();
            let dest = to.join(e.file_name());
            if e.file_type().unwrap().is_dir() {
                copy_tree(&e.path(), &dest);
            } else {
                std::fs::copy(e.path(), &dest).unwrap();
            }
        }
    }

    /// A private copy of the SC3 fixture, removed on drop.
    struct Root(PathBuf);

    impl Root {
        /// The fixture with `script` as its node (`None`: a FIFO the test
        /// writes reports to).
        fn new(tag: &str, script: Option<&str>) -> Self {
            let dir = std::env::temp_dir().join(format!("tessera-rt-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            copy_tree(&fixture(), &dir);
            let node = dir.join("dev/hidraw3");
            std::fs::remove_file(&node).unwrap();
            match script {
                Some(s) => std::fs::write(&node, s).unwrap(),
                None => nix::unistd::mkfifo(&node, nix::sys::stat::Mode::S_IRWXU).unwrap(),
            }
            Self(dir)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// One report of the emulated layout: four 12-bit faders, four buttons.
    fn report(faders: [u64; 4], buttons: u8) -> String {
        let v = faders[0] | faders[1] << 12 | faders[2] << 24 | faders[3] << 36;
        let mut s = String::from("01");
        for i in 0..6 {
            s.push_str(&format!(" {:02x}", (v >> (8 * i)) & 0xFF));
        }
        format!("{s} {buttons:02x}\n")
    }

    fn control(id: &str, kind: ControlKind, offset: u32, bits: Option<u8>) -> SurfaceControl {
        SurfaceControl {
            id: id.into(),
            label: None,
            kind,
            report_id: Some(1),
            offset: Some(offset),
            bits,
            max: None,
            inverted: false,
            active_low: false,
            motorized: false,
            code: None,
            midi: None,
        }
    }

    fn tx1() -> ChannelSet {
        ChannelSet {
            device: DeviceKey("main".into()),
            direction: Direction::Tx,
            channels: vec![Channel::new(1).unwrap()],
        }
    }

    /// Surface `desk`: button `m1` (bit 48) on TX channel 1 in `mode`.
    fn desk(mode: Bm) -> SurfaceSpec {
        let mut b = SurfaceBinding::new("m1", Ba::Mute, tx1());
        b.mode = mode;
        SurfaceSpec::new(
            "desk",
            SurfaceSource::Hidraw {
                vendor_id: 0x3142,
                product_id: 0x0c33,
                interface: Some(3),
            },
            vec![
                control("f1", ControlKind::Fader, 0, Some(12)),
                control("m1", ControlKind::Button, 48, None),
            ],
            vec![b],
        )
    }

    struct Running {
        surfaces: Arc<Surfaces>,
        specs: watch::Sender<SurfaceSpec>,
        cancel: CancellationToken,
        task: JoinHandle<()>,
    }

    /// Save `spec` in the engine and run its surface task on `root`.
    async fn start(e: &TestEngine, root: &Root, spec: SurfaceSpec, learning: bool) -> Running {
        e.handle
            .apply(
                Origin::Internal,
                Command::SetSurface {
                    surface: spec.clone(),
                },
            )
            .await
            .unwrap();
        let surfaces = Arc::new(Surfaces::new(Some(root.0.clone())));
        lock(&surfaces.live).insert(spec.name.clone(), Live::of(&spec, None));
        if learning {
            lock(&surfaces.learning).insert(SC3.into());
        }
        let (specs, rx) = watch::channel(spec);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(surface_task(
            Deps {
                surfaces: Arc::clone(&surfaces),
                engine: e.handle.clone(),
                bus: e.bus.clone(),
            },
            rx,
            cancel.clone(),
        ));
        Running {
            surfaces,
            specs,
            cancel,
            task,
        }
    }

    impl Running {
        fn live(&self) -> Live {
            lock(&self.surfaces.live).get("desk").cloned().unwrap()
        }

        async fn stop(self) {
            self.cancel.cancel();
            tokio::time::timeout(Duration::from_secs(5), self.task)
                .await
                .unwrap()
                .unwrap();
        }
    }

    async fn wait_until<F: std::future::Future<Output = bool>>(
        what: &str,
        mut f: impl FnMut() -> F,
    ) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !f().await {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn muted(e: &EngineHandle) -> bool {
        match e.query(Query::Status).await {
            Ok(Reply::Status(s)) => s.devices[0].tx_channels[0].muted,
            other => panic!("{other:?}"),
        }
    }

    async fn engine_connected(e: &EngineHandle) -> bool {
        saved(e)
            .await
            .unwrap()
            .iter()
            .any(|s| s.spec.name == "desk" && s.connected)
    }

    /// Unnumbered two-byte reports: a counter, then `data`, 20 ms apart.
    fn counted(start: u8, data: &[u8]) -> String {
        let mut s = String::new();
        for (i, d) in data.iter().enumerate() {
            let c = start.wrapping_add(u8::try_from(i % 256).unwrap());
            s.push_str(&format!("{c:02x} {d:02x}\n# sleep 20\n"));
        }
        s
    }

    fn learn_script(tag: &str, script: &str) -> (Root, Reports) {
        let root = Root::new(tag, Some(script));
        let r = reader::open(&root.0.join("dev/hidraw3"), true).unwrap();
        (root, r)
    }

    #[tokio::test]
    async fn learning_a_button_ignores_a_counter_and_ends_at_release() {
        let mut data = vec![0u8; 60];
        data.extend([1, 1, 0]);
        data.extend([0u8; 100]);
        let script = counted(0, &data) + "# sleep 10000\n";
        let (_root, mut r) = learn_script("learn-button", &script);
        let t = Instant::now();
        let found = learn_from(
            &mut r,
            tessera_surface::learn::Learner::new(false, false),
            Duration::from_secs(10),
        )
        .await
        .expect("a button");
        assert_eq!(found.kind, ControlKind::Button, "{found:?}");
        assert_eq!((found.offset, found.bits), (8, 1), "{found:?}");
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    }

    #[tokio::test]
    async fn learning_a_fader_settles_on_the_result_and_honours_moving_up() {
        // Idle, then down from 0x80 in steps of 8 while the counter keeps
        // counting: the raw reports never stop changing.
        let mut data = vec![0x80u8; 60];
        data.extend((2..=16).rev().map(|v| v * 8));
        data.extend([0x10u8; 150]);
        let script = counted(0, &data) + "# sleep 10000\n";
        let (_root, mut r) = learn_script("learn-fader", &script);
        let t = Instant::now();
        let found = learn_from(
            &mut r,
            tessera_surface::learn::Learner::new(false, true),
            Duration::from_secs(10),
        )
        .await
        .expect("a fader");
        assert_eq!(found.kind, ControlKind::Fader, "{found:?}");
        assert_eq!(found.offset, 8, "{found:?}");
        assert!(found.inverted, "moved down when asked up: {found:?}");
        assert!(
            t.elapsed() < Duration::from_secs(4),
            "settled on the result, not on raw reports: {:?}",
            t.elapsed()
        );
        // Without the hint, nothing is inverted.
        let (_root, mut r) = learn_script("learn-fader-nohint", &script);
        let found = learn_from(
            &mut r,
            tessera_surface::learn::Learner::new(false, false),
            Duration::from_secs(10),
        )
        .await
        .expect("a fader");
        assert!(!found.inverted, "{found:?}");
    }

    #[tokio::test]
    async fn learning_the_fixture_finds_a_control_of_report_one() {
        let s = Surfaces::new(Some(fixture()));
        let scan = s.scan().await;
        let node = scan.hid_by_id(SC3).unwrap().clone();
        let mut reports = reader::open(&node.node, true).unwrap();
        let found = learn_from(
            &mut reports,
            tessera_surface::learn::Learner::new(true, true),
            Duration::from_secs(8),
        )
        .await
        .expect("the script moves a fader and presses a button");
        assert_eq!(found.report_id, Some(1));
        match found.kind {
            ControlKind::Fader => {
                assert!(found.offset + u32::from(found.bits) <= 12, "{found:?}");
            }
            ControlKind::Button => assert_eq!(found.offset, 48),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn learn_arguments_are_checked() {
        let s = Arc::new(Surfaces::new(Some(fixture())));
        lock(&s.learning).insert(SC3.into());
        let e = s.learn(SC3, 100, true).await.unwrap_err();
        assert!(
            matches!(e, CommandError::Invalid { ref field, .. } if field == "device"),
            "{e:?}"
        );
        lock(&s.learning).clear();
        for t in [0, MAX_LEARN_MS + 1] {
            let e = s.learn(SC3, t, true).await.unwrap_err();
            assert!(matches!(e, CommandError::Invalid { ref field, .. } if field == "timeout_ms"));
        }
        let e = s.learn("card:SC3", 100, true).await.unwrap_err();
        assert!(matches!(e, CommandError::NotFound { .. }), "{e:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_surface_connects_at_its_first_report_and_releases_its_device() {
        let e = TestEngine::start("surface-rt", ONE_DEVICE);
        // The shipped script: fader 1 travels for about 2.6 s, then button 1.
        let root = Root::new(
            "connect",
            Some(&std::fs::read_to_string(fixture().join("dev/hidraw3")).unwrap()),
        );
        let s = start(&e, &root, desk(Bm::Toggle), false).await;
        wait_until("connected with live values", || {
            let l = s.live();
            async move {
                let f = l
                    .controls
                    .iter()
                    .find(|c| c.id == "f1")
                    .and_then(|c| c.value);
                l.connected && matches!(f, Some(ControlValue::Fader { max: 4095, .. }))
            }
        })
        .await;
        let l = s.live();
        assert_eq!(l.device.as_deref(), Some(SC3));
        assert_eq!(l.problem, None);
        assert!(engine_connected(&e.handle).await);
        let live = Arc::clone(&s.surfaces);
        s.stop().await;
        assert!(!lock(&live.live)["desk"].connected, "released on stop");
        assert!(!engine_connected(&e.handle).await, "disconnection reported");
        e.stop().await;
    }

    /// Push-to-talk held all the time.
    fn held() -> String {
        format!("{}# sleep 50\n", report([0, 0, 0, 0], 1))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_edit_keeps_the_session_and_the_ownership() {
        let e = TestEngine::start("surface-edit", ONE_DEVICE);
        let root = Root::new("edit", Some(&held()));
        let s = start(&e, &root, desk(Bm::PushToTalk), false).await;
        wait_until("push-to-talk held: live", || {
            let h = e.handle.clone();
            let c = s.live().connected;
            async move { c && !muted(&h).await }
        })
        .await;
        let sub = e.bus.subscribe(64);
        let mut edited = desk(Bm::PushToTalk);
        edited.controls[1].label = Some("Mic".into());
        edited
            .controls
            .push(control("m2", ControlKind::Button, 49, None));
        s.specs.send_replace(edited);
        wait_until("the edit applied", || {
            let n = s.live().controls.len();
            async move { n == 3 }
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        while let Ok(r) = sub.try_recv() {
            if let Received::Event(
                ev @ (Event::SurfacesChanged { .. } | Event::ChannelMuteChanged { .. }),
            ) = r
            {
                panic!("an edit must not reconnect or mute: {ev:?}");
            }
        }
        assert!(s.live().connected);
        assert!(!muted(&e.handle).await);
        assert!(engine_connected(&e.handle).await);
        let f1 = s
            .live()
            .controls
            .iter()
            .find(|c| c.id == "f1")
            .and_then(|c| c.value);
        assert!(f1.is_some(), "values kept across the edit");
        s.stop().await;
        e.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disabling_mutes_nothing_and_reports_the_disconnection() {
        let e = TestEngine::start("surface-disable", ONE_DEVICE);
        let root = Root::new("disable", Some(&held()));
        let s = start(&e, &root, desk(Bm::PushToTalk), false).await;
        wait_until("push-to-talk held: live", || {
            let h = e.handle.clone();
            let c = s.live().connected;
            async move { c && !muted(&h).await }
        })
        .await;
        assert!(engine_connected(&e.handle).await);
        s.stop().await;
        assert!(!muted(&e.handle).await, "not muted on the way out");
        assert!(!engine_connected(&e.handle).await);
        e.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unplugging_applies_the_push_to_talk_mute() {
        use std::io::Write;
        let e = TestEngine::start("surface-unplug", ONE_DEVICE);
        let root = Root::new("unplug", None);
        let s = start(&e, &root, desk(Bm::PushToTalk), false).await;
        wait_until("the node open", || {
            let d = s.live().device.is_some();
            async move { d }
        })
        .await;
        wait_until("push-to-talk muted from the start", || {
            let h = e.handle.clone();
            async move { muted(&h).await }
        })
        .await;
        assert!(!s.live().connected, "no report yet");
        let fifo = root.0.join("dev/hidraw3");
        let mut w = tokio::task::spawn_blocking(move || {
            std::fs::OpenOptions::new().write(true).open(fifo).unwrap()
        })
        .await
        .unwrap();
        w.write_all(&[1, 0, 0, 0, 0, 0, 0, 1]).unwrap();
        wait_until("held: live and connected", || {
            let h = e.handle.clone();
            async move { engine_connected(&h).await && !muted(&h).await }
        })
        .await;
        drop(w);
        wait_until("unplugged: muted and disconnected", || {
            let h = e.handle.clone();
            async move { !engine_connected(&h).await && muted(&h).await }
        })
        .await;
        s.stop().await;
        e.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_learn_on_the_device_pauses_the_surface() {
        let e = TestEngine::start("surface-pause", ONE_DEVICE);
        let rest = report([0, 0, 0, 0], 0);
        let press = report([0, 0, 0, 0], 1);
        let script = format!("{rest}# sleep 100\n{press}# sleep 100\n{rest}# sleep 200\n");
        let root = Root::new("pause", Some(&script));
        let s = start(&e, &root, desk(Bm::Toggle), true).await;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!muted(&e.handle).await, "presses during a learn do nothing");
        assert!(!s.live().connected);
        lock(&s.surfaces.learning).clear();
        wait_until("a press after the learn mutes", || {
            let h = e.handle.clone();
            async move { muted(&h).await }
        })
        .await;
        s.stop().await;
        e.stop().await;
    }

    // MIDI surfaces, on a copy of the emulated CQ.

    const CQ: &str = "midi:CQ:0";

    /// A private copy of the CQ fixture with `script` as its MIDI port.
    fn cq_root(tag: &str, script: &str) -> Root {
        let dir = std::env::temp_dir().join(format!("tessera-rt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        copy_tree(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/surfaces/cq"),
            &dir,
        );
        std::fs::write(dir.join("dev/snd/midiC1D0"), script).unwrap();
        Root(dir)
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x} ")).collect()
    }

    fn nrpn_addr(number: u16) -> tessera_control_types::MidiAddress {
        tessera_control_types::MidiAddress::new(tessera_control_types::MidiKind::Nrpn, 1, number)
    }

    /// Surface `desk` on the CQ: input 1's mute (NRPN 00/00) toggles TX 1,
    /// input 1's motorized fader (NRPN 40/00) sets the gain of TX 2.
    fn cq_desk() -> SurfaceSpec {
        let mut mute = control("ip1-mute", ControlKind::Button, 0, None);
        mute.report_id = None;
        mute.offset = None;
        mute.midi = Some(nrpn_addr(0));
        let mut fader = control("ip1", ControlKind::Fader, 0, None);
        fader.report_id = None;
        fader.offset = None;
        fader.max = Some(16_320);
        fader.motorized = true;
        fader.midi = Some(nrpn_addr(0x40 << 7));
        let mut tx2 = tx1();
        tx2.channels = vec![Channel::new(2).unwrap()];
        let mut gain = SurfaceBinding::new("ip1", Ba::Gain, tx2);
        gain.max_mdb = 10_000;
        SurfaceSpec::new(
            "desk",
            SurfaceSource::Midi {
                card: "CQ".into(),
                device: 0,
            },
            vec![mute, fader],
            vec![SurfaceBinding::new("ip1-mute", Ba::Mute, tx1()), gain],
        )
    }

    fn written(s: &Surfaces) -> String {
        lock(&s.lab_out)
            .get(CQ)
            .map(|v| hex(&lock(v)))
            .unwrap_or_default()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_desk_mute_applies_and_the_desk_follows_without_echo() {
        let e = TestEngine::start("surface-midi", ONE_DEVICE);
        let mute_on = hex(&tessera_surface::midi::encode_control(&nrpn_addr(0), 1));
        let mute_off = hex(&tessera_surface::midi::encode_control(&nrpn_addr(0), 0));
        let root = cq_root("midi", &format!("# sleep 300\n{mute_on}\n# sleep 100000\n"));
        let s = start(&e, &root, cq_desk(), false).await;
        // On open: the desk is asked for both values.
        wait_until("the gets written", || {
            let w = written(&s.surfaces);
            async move {
                w.contains("b0 63 00 b0 62 00 b0 60 7f") && w.contains("b0 63 40 b0 62 00 b0 60 7f")
            }
        })
        .await;
        wait_until("the desk's mute applied", || {
            let h = e.handle.clone();
            async move { muted(&h).await }
        })
        .await;
        assert!(s.live().connected);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !written(&s.surfaces).contains(&mute_on),
            "the desk's own state is not echoed: {}",
            written(&s.surfaces)
        );
        // The portal unmutes: the desk is told.
        e.handle
            .apply(
                Origin::Internal,
                Command::SetChannelMute {
                    target: tx1(),
                    muted: false,
                },
            )
            .await
            .unwrap();
        wait_until("the desk unmuted", || {
            let w = written(&s.surfaces);
            let m = mute_off.clone();
            async move { w.contains(&m) }
        })
        .await;
        // The gain set elsewhere moves the motorized fader (it was sent to
        // the engine's gain on open already).
        let moves = |s: &Surfaces| written(s).matches("b0 63 40 b0 62 00 b0 06").count();
        let before = moves(&s.surfaces);
        assert_eq!(before, 1, "{}", written(&s.surfaces));
        let mut tx2 = tx1();
        tx2.channels = vec![Channel::new(2).unwrap()];
        e.handle
            .apply(
                Origin::Internal,
                Command::SetChannelGain {
                    target: tx2,
                    gain: tessera_control_types::Gain::MilliDb(-20_000),
                },
            )
            .await
            .unwrap();
        wait_until("the fader moved", || {
            let n = moves(&s.surfaces);
            async move { n == before + 1 }
        })
        .await;
        s.stop().await;
        e.stop().await;
    }

    /// The gain of TX channel 2 (the CQ desk's fader target).
    async fn tx2_gain(e: &EngineHandle) -> tessera_control_types::Gain {
        match e.query(Query::Status).await {
            Ok(Reply::Status(st)) => st.devices[0].tx_channels[1].gain,
            other => panic!("{other:?}"),
        }
    }

    /// The bytes that put the CQ desk's input 1 fader at `raw`.
    fn fader_at(raw: u32) -> String {
        hex(&tessera_surface::midi::encode_control(
            &nrpn_addr(0x40 << 7),
            raw,
        ))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_desk_that_has_not_reported_shows_no_values_and_sets_nothing() {
        let e = TestEngine::start("surface-quiet", ONE_DEVICE);
        let sub = e.bus.subscribe(256);
        let root = cq_root("quiet", "# sleep 100000\n");
        let s = start(&e, &root, cq_desk(), false).await;
        // The port is open: the desk was asked for its values and the
        // motorized fader sent to the gain (tessera's own move).
        wait_until("the gets and the start-up move written", || {
            let w = written(&s.surfaces);
            async move {
                w.contains("b0 63 40 b0 62 00 b0 60 7f") && w.contains("b0 63 40 b0 62 00 b0 06")
            }
        })
        .await;
        // The gain set elsewhere (to off: raw 0) moves the fader again.
        let mut tx2 = tx1();
        tx2.channels = vec![Channel::new(2).unwrap()];
        e.handle
            .apply(
                Origin::Internal,
                Command::SetChannelGain {
                    target: tx2,
                    gain: tessera_control_types::Gain::Off,
                },
            )
            .await
            .unwrap();
        wait_until("the fader sent to off", || {
            let w = written(&s.surfaces);
            async move { w.contains(&fader_at(0)) }
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let l = s.live();
        assert!(!l.connected, "connected only at the first report");
        let value = |id: &str| l.controls.iter().find(|c| c.id == id).and_then(|c| c.value);
        assert_eq!(
            value("ip1-mute"),
            None,
            "no value before the desk reports one"
        );
        // The motor move was written: the fader's value is where it was
        // sent (the desk's echo of it is no change), though it set nothing.
        assert!(
            matches!(value("ip1"), Some(ControlValue::Fader { value: 0, .. })),
            "the motor's position: {:?}",
            l.controls
        );
        while let Ok(r) = sub.try_recv() {
            if let Received::Event(
                Event::ChannelGainChanged { origin, .. } | Event::ChannelMuteChanged { origin, .. },
            ) = r
            {
                assert!(
                    !matches!(origin, Origin::Surface { .. }),
                    "the surface sets nothing before the desk reports"
                );
            }
        }
        assert_eq!(tx2_gain(&e.handle).await, tessera_control_types::Gain::Off);
        assert!(!muted(&e.handle).await);
        s.stop().await;
        e.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_edit_moves_a_motorized_fader_to_the_gain_under_the_new_curve() {
        let e = TestEngine::start("surface-recurve-motor", ONE_DEVICE);
        let mute_off = hex(&tessera_surface::midi::encode_control(&nrpn_addr(0), 0));
        // The desk reports its mute once (it connects), then stays quiet.
        let root = cq_root(
            "recurve-motor",
            &format!("# sleep 200\n{mute_off}\n# sleep 100000\n"),
        );
        let s = start(&e, &root, cq_desk(), false).await;
        wait_until("connected", || {
            let c = s.live().connected;
            async move { c }
        })
        .await;
        // Unity on the CQ's fader law is raw 12544; the dB taper put the
        // fader elsewhere.
        let unity = fader_at(12_544);
        assert!(!written(&s.surfaces).contains(&unity));
        let mut edited = cq_desk();
        edited.bindings[1].curve = tessera_surface::presets::allen_heath_cq(1).1;
        s.specs.send_replace(edited);
        wait_until("the fader moved to unity on the new curve", || {
            let w = written(&s.surfaces);
            let u = unity.clone();
            async move { w.contains(&u) }
        })
        .await;
        // The gain owns a motorized fader: it is unchanged.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            tx2_gain(&e.handle).await,
            tessera_control_types::Gain::default()
        );
        s.stop().await;
        e.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_edit_applies_a_fader_without_a_motor_under_the_new_curve() {
        let e = TestEngine::start("surface-recurve", ONE_DEVICE);
        // The desk reports its fader at raw 7936 (-10 dB on the CQ's law)
        // once, then stays quiet (and does not answer the gets).
        let root = cq_root(
            "recurve",
            &format!("# sleep 200\n{}\n# sleep 100000\n", fader_at(7_936)),
        );
        let mut spec = cq_desk();
        spec.controls[1].motorized = false;
        let s = start(&e, &root, spec.clone(), false).await;
        // dB taper, -60..+10 dB: raw 7936 of 16320 is about -26 dB.
        wait_until("the fader applied under the taper", || {
            let h = e.handle.clone();
            async move { tx2_gain(&h).await != tessera_control_types::Gain::default() }
        })
        .await;
        let first = tx2_gain(&e.handle).await.milli_db().unwrap();
        assert!((-27_000..=-25_000).contains(&first), "{first}");
        let reported = s
            .live()
            .controls
            .iter()
            .find(|c| c.id == "ip1")
            .and_then(|c| c.value);
        assert!(
            matches!(reported, Some(ControlValue::Fader { value: 7_936, .. })),
            "{reported:?}"
        );
        // The desk's fader law: the same position is now -10 dB, at once.
        spec.bindings[1].curve = tessera_surface::presets::allen_heath_cq(1).1;
        s.specs.send_replace(spec);
        wait_until("the last position applied under the new curve", || {
            let h = e.handle.clone();
            async move { tx2_gain(&h).await == tessera_control_types::Gain::MilliDb(-10_000) }
        })
        .await;
        s.stop().await;
        e.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_learn_on_a_held_midi_port_is_fed_by_the_session() {
        let e = TestEngine::start("surface-midi-learn", ONE_DEVICE);
        let mut script = String::from("# sleep 1100\n");
        for v in (0..16_320u32).step_by(1020) {
            script.push_str(&hex(&tessera_surface::midi::encode_control(
                &nrpn_addr(0x40 << 7),
                v,
            )));
            script.push_str("\n# sleep 20\n");
        }
        script.push_str("# sleep 100000\n");
        let root = cq_root("midi-learn", &script);
        let s = start(&e, &root, cq_desk(), false).await;
        wait_until("the port held", || {
            let held = lock(&s.surfaces.holding).contains(CQ);
            async move { held }
        })
        .await;
        let found = s
            .surfaces
            .learn(CQ, 5_000, true)
            .await
            .unwrap()
            .expect("the fader");
        assert_eq!(found.kind, ControlKind::Fader, "{found:?}");
        assert_eq!(found.midi, Some(nrpn_addr(0x40 << 7)));
        assert_eq!(found.max, Some(15_300), "the largest value seen");
        // The learn moved nothing.
        match e.handle.query(Query::Status).await {
            Ok(Reply::Status(st)) => assert_eq!(
                st.devices[0].tx_channels[1].gain,
                tessera_control_types::Gain::default()
            ),
            other => panic!("{other:?}"),
        }
        s.stop().await;
        e.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fader_bound_to_the_output_sets_the_output_level() {
        let e = TestEngine::start("surface-output", ONE_DEVICE);
        let root = Root::new(
            "output",
            Some(&std::fs::read_to_string(fixture().join("dev/hidraw3")).unwrap()),
        );
        let mut spec = desk(Bm::Toggle);
        let mut rx1 = tx1();
        rx1.direction = Direction::Rx;
        spec.bindings = vec![SurfaceBinding::new("f1", Ba::OutputGain, rx1)];
        let s = start(&e, &root, spec, false).await;
        let rx = |h: EngineHandle| async move {
            match h.query(Query::Status).await {
                Ok(Reply::Status(st)) => st.devices[0].rx_channels[0].clone(),
                other => panic!("{other:?}"),
            }
        };
        wait_until("the fader moved the output level", || {
            let h = e.handle.clone();
            async move { rx(h).await.output_gain != tessera_control_types::Gain::default() }
        })
        .await;
        let c = rx(e.handle.clone()).await;
        assert_eq!(
            c.gain,
            tessera_control_types::Gain::default(),
            "input level untouched"
        );
        s.stop().await;
        e.stop().await;
    }

    // ---- tcp_midi and control routing -------------------------------------

    use std::io::{Read as _, Write as _};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A desk that offers raw MIDI over TCP: accepts one connection at a
    /// time, records what it receives, sends what the test gives it, and
    /// drops the connection on request.
    struct FakeDesk {
        port: u16,
        got: Arc<Mutex<Vec<u8>>>,
        send: std::sync::mpsc::Sender<Vec<u8>>,
        accepts: Arc<AtomicUsize>,
        hang_up: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
    }

    impl FakeDesk {
        fn start() -> Self {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.set_nonblocking(true).unwrap();
            let port = l.local_addr().unwrap().port();
            let got: Arc<Mutex<Vec<u8>>> = Arc::default();
            let (send, out) = std::sync::mpsc::channel::<Vec<u8>>();
            let accepts = Arc::new(AtomicUsize::new(0));
            let hang_up = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let (g, a, h, st) = (
                Arc::clone(&got),
                Arc::clone(&accepts),
                Arc::clone(&hang_up),
                Arc::clone(&stop),
            );
            std::thread::spawn(move || {
                while !st.load(Ordering::Relaxed) {
                    let Ok((mut s, _)) = l.accept() else {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    };
                    a.fetch_add(1, Ordering::Relaxed);
                    s.set_nonblocking(false).unwrap();
                    s.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
                    let mut buf = [0u8; 512];
                    loop {
                        if st.load(Ordering::Relaxed) || h.swap(false, Ordering::Relaxed) {
                            break;
                        }
                        match s.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => lock(&g).extend_from_slice(&buf[..n]),
                            Err(_) => {}
                        }
                        while let Ok(b) = out.try_recv() {
                            let _ = s.write_all(&b);
                        }
                    }
                }
            });
            Self {
                port,
                got,
                send,
                accepts,
                hang_up,
                stop,
            }
        }

        fn got(&self) -> String {
            hex(&lock(&self.got))
        }

        fn clear(&self) {
            lock(&self.got).clear();
        }
    }

    impl Drop for FakeDesk {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    /// A tcp_midi surface `name` on `port` with fader `f` (NRPN 40/00, 0..=16320).
    fn tcp_desk(name: &str, port: u16, motorized: bool) -> SurfaceSpec {
        let mut f = control("f", ControlKind::Fader, 0, None);
        f.report_id = None;
        f.offset = None;
        f.max = Some(16_320);
        f.motorized = motorized;
        f.midi = Some(nrpn_addr(0x40 << 7));
        SurfaceSpec::new(
            name,
            SurfaceSource::TcpMidi {
                host: "127.0.0.1".into(),
                port,
            },
            vec![f],
            Vec::new(),
        )
    }

    fn fader_on(port_bytes: u32) -> String {
        fader_at(port_bytes)
    }

    /// Save `spec` and run its task with the shared `surfaces`.
    async fn run_on(
        e: &TestEngine,
        surfaces: &Arc<Surfaces>,
        spec: SurfaceSpec,
    ) -> (
        watch::Sender<SurfaceSpec>,
        CancellationToken,
        JoinHandle<()>,
    ) {
        e.handle
            .apply(
                Origin::Internal,
                Command::SetSurface {
                    surface: spec.clone(),
                },
            )
            .await
            .unwrap();
        lock(&surfaces.live).insert(spec.name.clone(), Live::of(&spec, None));
        let (tx, rx) = watch::channel(spec);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(surface_task(
            Deps {
                surfaces: Arc::clone(surfaces),
                engine: e.handle.clone(),
                bus: e.bus.clone(),
            },
            rx,
            cancel.clone(),
        ));
        (tx, cancel, task)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tcp_midi_desk_is_connected_out_sets_gains_and_is_reconnected() {
        let e = TestEngine::start("surface-tcp", ONE_DEVICE);
        let desk = FakeDesk::start();
        let mut spec = tcp_desk("desk", desk.port, false);
        let mut tx2 = tx1();
        tx2.channels = vec![Channel::new(2).unwrap()];
        spec.bindings = vec![SurfaceBinding::new("f", Ba::Gain, tx2)];
        let surfaces = Arc::new(Surfaces::new(None));
        let (_specs, cancel, task) = run_on(&e, &surfaces, spec).await;
        // Connected out; the desk is asked for the fader's value.
        wait_until("the get on the connection", || {
            let w = desk.got();
            async move { w.contains("b0 63 40 b0 62 00 b0 60 7f") }
        })
        .await;
        let device = lock(&surfaces.live)
            .get("desk")
            .and_then(|l| l.device.clone());
        assert_eq!(
            device.as_deref(),
            Some(format!("tcp:127.0.0.1:{}", desk.port).as_str())
        );
        // The desk's fader at the top: 0 dB, and the surface connected.
        desk.send
            .send(tessera_surface::midi::encode_control(&nrpn_addr(0x40 << 7), 16_320).to_vec())
            .unwrap();
        wait_until("the fader's gain applied", || {
            let h = e.handle.clone();
            async move { tx2_gain(&h).await == tessera_control_types::Gain::MilliDb(0) }
        })
        .await;
        wait_until("the connection reported", || {
            let h = e.handle.clone();
            async move { engine_connected(&h).await }
        })
        .await;
        // The desk hangs up: tessera connects again (back-off from 1 s).
        desk.hang_up.store(true, Ordering::Relaxed);
        wait_until("the second connection", || {
            let n = desk.accepts.load(Ordering::Relaxed);
            async move { n >= 2 }
        })
        .await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        e.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_desk_that_falls_silent_is_probed_then_lost_and_reconnected() {
        let e = TestEngine::start("surface-tcp-silent", ONE_DEVICE);
        let desk = FakeDesk::start();
        let mut spec = tcp_desk("desk", desk.port, false);
        let mut tx2 = tx1();
        tx2.channels = vec![Channel::new(2).unwrap()];
        spec.bindings = vec![SurfaceBinding::new("f", Ba::Gain, tx2)];
        let surfaces = Arc::new(Surfaces::new(None));
        surfaces.set_liveness(Duration::from_millis(900));
        let (_specs, cancel, task) = run_on(&e, &surfaces, spec).await;
        wait_until("the get on the connection", || {
            let w = desk.got();
            async move { w.contains("b0 63 40 b0 62 00 b0 60 7f") }
        })
        .await;
        // The desk answers once, then says nothing more (powered off with
        // the connection still open on our side).
        desk.send
            .send(tessera_surface::midi::encode_control(&nrpn_addr(0x40 << 7), 16_320).to_vec())
            .unwrap();
        wait_until("the connection reported", || {
            let h = e.handle.clone();
            async move { engine_connected(&h).await }
        })
        .await;
        desk.clear();
        // Asked again (a probe), then lost: the fader is released and the
        // disconnection reported, without the peer closing anything.
        wait_until("a liveness probe", || {
            let w = desk.got();
            async move { w.contains("b0 63 40 b0 62 00 b0 60 7f") }
        })
        .await;
        wait_until("the disconnection reported", || {
            let h = e.handle.clone();
            async move { !engine_connected(&h).await }
        })
        .await;
        let problem = lock(&surfaces.live)
            .get("desk")
            .and_then(|l| l.problem.clone())
            .unwrap_or_default();
        assert!(
            problem.contains("nothing received") || problem.contains(WAITING),
            "{problem}"
        );
        wait_until("the connection opened again", || {
            let n = desk.accepts.load(Ordering::Relaxed);
            async move { n >= 2 }
        })
        .await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        e.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_desk_that_never_answers_a_get_is_not_lost_when_idle() {
        // The Allen & Heath CQ over USB: it talks when a control moves and
        // does not answer a get, so an idle desk is silent and alive.
        let e = TestEngine::start("surface-tcp-idle", ONE_DEVICE);
        let desk = FakeDesk::start();
        let mut spec = tcp_desk("desk", desk.port, false);
        let mut tx2 = tx1();
        tx2.channels = vec![Channel::new(2).unwrap()];
        spec.bindings = vec![SurfaceBinding::new("f", Ba::Gain, tx2)];
        let surfaces = Arc::new(Surfaces::new(None));
        surfaces.set_liveness(Duration::from_millis(900));
        let (_specs, cancel, task) = run_on(&e, &surfaces, spec).await;
        wait_until("the get on the connection", || {
            let w = desk.got();
            async move { w.contains("b0 63 40 b0 62 00 b0 60 7f") }
        })
        .await;
        // No answer within the liveness (900 ms): probing stops. A hand
        // move after that, then nothing: well past the liveness the desk
        // is still connected.
        tokio::time::sleep(Duration::from_millis(1_300)).await;
        desk.send
            .send(tessera_surface::midi::encode_control(&nrpn_addr(0x40 << 7), 8_000).to_vec())
            .unwrap();
        wait_until("the connection reported", || {
            let h = e.handle.clone();
            async move { engine_connected(&h).await }
        })
        .await;
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        assert!(
            engine_connected(&e.handle).await,
            "an idle desk stays connected"
        );
        assert_eq!(desk.accepts.load(Ordering::Relaxed), 1, "never reconnected");
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        e.stop().await;
    }

    #[test]
    fn a_desk_value_is_kept_only_when_its_write_was_taken() {
        let cfg = compat::config_of(&tcp_desk("desk", 1, true));
        // An output that failed: nothing is taken as shown, the session ends.
        let mut m = MidiInput::new(&cfg, reader::failed_output());
        assert_eq!(m.move_fader("f", 100), None);
        assert!(!m.desk.contains_key("f"));
        assert!(m.broken.is_some(), "the session ends as lost");
        // A working output keeps it.
        let mut m = MidiInput::new(&cfg, Output::Lab(Arc::default()));
        assert!(m.move_fader("f", 100).is_some());
        assert_eq!(m.desk.get("f"), Some(&100));
    }

    #[test]
    fn liveness_checks_probe_at_a_third_and_end_at_the_whole() {
        let t0 = Instant::now();
        let l = Duration::from_millis(900);
        assert_eq!(next_check(t0, t0, l), t0 + l / 3);
        // Probed at a third: next probe at two thirds.
        let p = t0 + l / 3;
        assert_eq!(next_check(t0, p, l), p + l / 3);
        // Probed late: the end comes first.
        let p = t0 + Duration::from_millis(800);
        assert_eq!(next_check(t0, p, l), t0 + l);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_followed_desk_moves_by_its_law_and_its_echo_does_not_loop() {
        let e = TestEngine::start("surface-follow", ONE_DEVICE);
        let (a, b) = (FakeDesk::start(), FakeDesk::start());
        let surfaces = Arc::new(Surfaces::new(None));
        surfaces.set_echo_window(Duration::from_millis(600));
        // desk2 first (desk follows it), then desk, then desk2 follows desk
        // back: a pair.
        let (b_specs, b_cancel, b_task) =
            run_on(&e, &surfaces, tcp_desk("desk2", b.port, true)).await;
        let mut desk = tcp_desk("desk", a.port, false);
        let mut follow = SurfaceBinding::new("f", Ba::Follow, ChannelSet::default());
        follow.follow = Some(tessera_control_types::ControlRef::new("desk2", "f"));
        desk.bindings = vec![follow.clone()];
        let (_a_specs, a_cancel, a_task) = run_on(&e, &surfaces, desk).await;
        let mut desk2 = tcp_desk("desk2", b.port, true);
        follow.follow = Some(tessera_control_types::ControlRef::new("desk", "f"));
        // desk2's own law: -40..+10 dB.
        follow.min_mdb = -40_000;
        follow.max_mdb = 10_000;
        desk2.bindings = vec![follow];
        e.handle
            .apply(
                Origin::Internal,
                Command::SetSurface {
                    surface: desk2.clone(),
                },
            )
            .await
            .unwrap();
        b_specs.send_replace(desk2);
        for d in [&a, &b] {
            wait_until("connected", || {
                let w = d.got();
                async move { w.contains("b0 60 7f") }
            })
            .await;
        }
        b.clear();
        a.clear();
        // desk's fader at half travel: -30 dB by its (default) law, which is
        // position 3264 of 16320 on desk2's -40..+10 dB law.
        a.send
            .send(tessera_surface::midi::encode_control(&nrpn_addr(0x40 << 7), 8_160).to_vec())
            .unwrap();
        let want = fader_on(3_264);
        wait_until("desk2's fader driven", || {
            let w = b.got();
            let want = want.clone();
            async move { w.contains(&want) }
        })
        .await;
        // desk2's motor reports where it went: an echo, not followed back.
        b.send
            .send(tessera_surface::midi::encode_control(&nrpn_addr(0x40 << 7), 3_264).to_vec())
            .unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !a.got().contains("b0 63 40 b0 62 00 b0 06"),
            "the echo went back to desk: {}",
            a.got()
        );
        // Past the echo window: moved by hand on desk2 (to its top, +10
        // dB), desk follows by its own law (top 0 dB: its fader goes to the
        // top).
        tokio::time::sleep(Duration::from_millis(400)).await;
        b.send
            .send(tessera_surface::midi::encode_control(&nrpn_addr(0x40 << 7), 16_320).to_vec())
            .unwrap();
        let top = fader_on(16_320);
        wait_until("desk's fader driven back", || {
            let w = a.got();
            let top = top.clone();
            async move { w.contains(&top) }
        })
        .await;
        for (c, t) in [(a_cancel, a_task), (b_cancel, b_task)] {
            c.cancel();
            tokio::time::timeout(Duration::from_secs(5), t)
                .await
                .unwrap()
                .unwrap();
        }
        e.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_motorized_fader_on_a_router_port_follows_its_level() {
        let e = TestEngine::start("surface-port", ONE_DEVICE);
        let root = cq_root("port", "# sleep 100000\n");
        let app1 = tessera_control_types::RouterPort::AppOut {
            device: DeviceKey("main".into()),
            channel: Channel::new(1).unwrap(),
        };
        let mut spec = cq_desk();
        let mut b = SurfaceBinding::new("ip1", Ba::PortGain, ChannelSet::default());
        b.ports = vec![app1.clone()];
        spec.bindings = vec![b];
        let s = start(&e, &root, spec, false).await;
        let moves = |s: &Surfaces| written(s).matches("b0 63 40 b0 62 00 b0 06").count();
        // At start the motor goes to the port's level (unity: the top).
        wait_until("the start-up move", || {
            let n = moves(&s.surfaces);
            async move { n == 1 }
        })
        .await;
        e.handle
            .apply(
                Origin::Internal,
                Command::SetPortGain {
                    ports: vec![app1],
                    gain: tessera_control_types::Gain::MilliDb(-20_000),
                },
            )
            .await
            .unwrap();
        wait_until("the fader moved to the new level", || {
            let n = moves(&s.surfaces);
            async move { n == 2 }
        })
        .await;
        s.stop().await;
        e.stop().await;
    }

    // ---- follow semantics, echoes, connection reports, learn, stalls ------

    /// One device and a host volume.
    const WITH_HOST: &str = "[network]\ninterfaces=['audio0']\n[host_volume]\ncard='Headphones'\ncontrol='Digital'\nmin_db=-60\nmax_db=0\n[[device]]\nid='main'\nrx_channels=2\ntx_channels=2\n";

    async fn host_gain(e: &EngineHandle) -> Option<tessera_control_types::Gain> {
        match e.query(Query::Status).await {
            Ok(Reply::Status(st)) => st.host_volume.and_then(|h| h.gain),
            other => panic!("{other:?}"),
        }
    }

    fn follows(control: &str, surface: &str, followed: &str) -> SurfaceBinding {
        let mut b = SurfaceBinding::new(control, Ba::Follow, ChannelSet::default());
        b.follow = Some(tessera_control_types::ControlRef::new(surface, followed));
        b
    }

    fn tx2() -> ChannelSet {
        let mut t = tx1();
        t.channels = vec![Channel::new(2).unwrap()];
        t
    }

    fn send_fader(d: &FakeDesk, raw: u32) {
        d.send
            .send(tessera_surface::midi::encode_control(&nrpn_addr(0x40 << 7), raw).to_vec())
            .unwrap();
    }

    async fn stop_all(tasks: Vec<(CancellationToken, JoinHandle<()>)>) {
        for (c, t) in tasks {
            c.cancel();
            tokio::time::timeout(Duration::from_secs(5), t)
                .await
                .unwrap()
                .unwrap();
        }
    }

    /// Fader writes on a fake desk's connection (the gets excluded).
    fn fader_writes(d: &FakeDesk) -> usize {
        d.got().matches("b0 63 40 b0 62 00 b0 06").count()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_follower_applies_the_followed_controls_bindings_by_its_law() {
        let e = TestEngine::start("surface-follow-host", WITH_HOST);
        let (x, cq) = (FakeDesk::start(), FakeDesk::start());
        let surfaces = Arc::new(Surfaces::new(None));
        // The CQ's main fader (motorized) sets the host volume on the CQ's
        // own fader law; X's fader follows it.
        let mut cq_spec = tcp_desk("cq", cq.port, true);
        let mut host = SurfaceBinding::new("f", Ba::HostVolume, ChannelSet::default());
        host.curve = tessera_surface::presets::allen_heath_cq(1).1;
        cq_spec.bindings = vec![host];
        let law = compat::config_of(&cq_spec).bindings[0].clone();
        let (_c, c_cancel, c_task) = run_on(&e, &surfaces, cq_spec).await;
        let mut x_spec = tcp_desk("x", x.port, false);
        x_spec.bindings = vec![follows("f", "cq", "f")];
        let (_x, x_cancel, x_task) = run_on(&e, &surfaces, x_spec).await;
        for d in [&x, &cq] {
            wait_until("connected", || {
                let w = d.got();
                async move { w.contains("b0 60 7f") }
            })
            .await;
        }
        assert_eq!(host_gain(&e.handle).await, None);
        // X at half travel: -30 dB by its (default) law. The CQ's fader goes
        // to -30 dB on the CQ's law, and the host volume follows as if the
        // CQ's fader had moved there.
        send_fader(&x, 8_160);
        let pos = tessera_surface::fader_position(&law, -30.0, 16_320);
        let want = fader_at(pos);
        wait_until("the CQ's fader driven by its law", || {
            let w = cq.got();
            let want = want.clone();
            async move { w.contains(&want) }
        })
        .await;
        wait_until("the host volume set", || {
            let h = e.handle.clone();
            async move {
                host_gain(&h)
                    .await
                    .and_then(|g| g.milli_db())
                    .is_some_and(|m| (m + 30_000).abs() <= 300)
            }
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(fader_writes(&x), 0, "nothing back to X: {}", x.got());
        assert_eq!(fader_writes(&cq), 1, "one move of the CQ: {}", cq.got());
        stop_all(vec![(c_cancel, c_task), (x_cancel, x_task)]).await;
        e.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_chain_of_follows_reaches_its_end() {
        let e = TestEngine::start("surface-follow-chain", ONE_DEVICE);
        let (a, b, c) = (FakeDesk::start(), FakeDesk::start(), FakeDesk::start());
        let surfaces = Arc::new(Surfaces::new(None));
        // A follows B, B follows C, C's fader sets TX 2.
        let mut c_spec = tcp_desk("c", c.port, true);
        c_spec.bindings = vec![SurfaceBinding::new("f", Ba::Gain, tx2())];
        let mut b_spec = tcp_desk("b", b.port, true);
        b_spec.bindings = vec![follows("f", "c", "f")];
        let mut a_spec = tcp_desk("a", a.port, false);
        a_spec.bindings = vec![follows("f", "b", "f")];
        let (mut tasks, mut senders) = (Vec::new(), Vec::new());
        for s in [c_spec, b_spec, a_spec] {
            let (tx, cancel, task) = run_on(&e, &surfaces, s).await;
            senders.push(tx);
            tasks.push((cancel, task));
        }
        for d in [&a, &b, &c] {
            wait_until("connected", || {
                let w = d.got();
                async move { w.contains("b0 60 7f") }
            })
            .await;
        }
        let c_moves = fader_writes(&c);
        send_fader(&a, 8_160);
        let half = fader_at(8_160);
        for d in [&b, &c] {
            wait_until("driven along the chain", || {
                let w = d.got();
                let half = half.clone();
                async move { w.contains(&half) }
            })
            .await;
        }
        wait_until("C's binding applied", || {
            let h = e.handle.clone();
            async move { tx2_gain(&h).await == tessera_control_types::Gain::MilliDb(-30_000) }
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(fader_writes(&a), 0, "nothing back to A: {}", a.got());
        assert_eq!(fader_writes(&c), c_moves + 1, "{}", c.got());
        stop_all(tasks).await;
        e.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pair_never_ping_pongs_on_an_inexact_late_echo() {
        let e = TestEngine::start("surface-follow-pair", ONE_DEVICE);
        let (a, b) = (FakeDesk::start(), FakeDesk::start());
        let surfaces = Arc::new(Surfaces::new(None));
        // Longer than the 1.5 s this window once was.
        let window = Duration::from_millis(2_500);
        surfaces.set_echo_window(window);
        // A and B follow each other; B's fader also sets TX 2. B is saved
        // first, then A following it, then B following A back.
        let mut b_spec = tcp_desk("b", b.port, true);
        b_spec.bindings = vec![SurfaceBinding::new("f", Ba::Gain, tx2())];
        let (b_specs, b_cancel, b_task) = run_on(&e, &surfaces, b_spec.clone()).await;
        let mut a_spec = tcp_desk("a", a.port, true);
        a_spec.bindings = vec![follows("f", "b", "f")];
        let (_a_specs, a_cancel, a_task) = run_on(&e, &surfaces, a_spec).await;
        b_spec.bindings.insert(0, follows("f", "a", "f"));
        e.handle
            .apply(
                Origin::Internal,
                Command::SetSurface {
                    surface: b_spec.clone(),
                },
            )
            .await
            .unwrap();
        b_specs.send_replace(b_spec);
        let tasks = vec![(a_cancel, a_task), (b_cancel, b_task)];
        for d in [&a, &b] {
            wait_until("connected", || {
                let w = d.got();
                async move { w.contains("b0 60 7f") }
            })
            .await;
        }
        // B's motor went to TX 2's level at start, and A followed it: past
        // that echo window.
        tokio::time::sleep(window + Duration::from_millis(200)).await;
        a.clear();
        let b_moves = fader_writes(&b);
        // A by hand: B is driven, B's binding applies, nothing comes back.
        send_fader(&a, 8_160);
        wait_until("TX 2 set through B", || {
            let h = e.handle.clone();
            async move { tx2_gain(&h).await == tessera_control_types::Gain::MilliDb(-30_000) }
        })
        .await;
        let driven = Instant::now();
        assert_eq!(fader_writes(&b), b_moves + 1, "{}", b.got());
        // B's motor reports where it landed, 60 steps off (0.2 dB) and late
        // (after 1.8 s): an echo, dropped.
        tokio::time::sleep_until(driven + Duration::from_millis(1_800)).await;
        send_fader(&b, 8_220);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(fader_writes(&a), 0, "the echo went back to A: {}", a.got());
        assert_eq!(
            tx2_gain(&e.handle).await,
            tessera_control_types::Gain::MilliDb(-30_000),
            "an echo is not a movement"
        );
        // Still in the echo window, B lands well off (+2.2 dB): B's own
        // binding takes it, it is not followed back to A.
        send_fader(&b, 8_760);
        wait_until("B's binding applied", || {
            let h = e.handle.clone();
            async move { tx2_gain(&h).await != tessera_control_types::Gain::MilliDb(-30_000) }
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(fader_writes(&a), 0, "followed back to A: {}", a.got());
        // After the window, B moved by hand is followed by A again.
        tokio::time::sleep_until(driven + window + Duration::from_millis(100)).await;
        send_fader(&b, 16_320);
        let top = fader_at(16_320);
        wait_until("A follows B again", || {
            let w = a.got();
            let top = top.clone();
            async move { w.contains(&top) }
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(fader_writes(&a), 1, "{}", a.got());
        stop_all(tasks).await;
        e.stop().await;
    }

    #[test]
    fn a_fader_echo_is_within_half_a_db_or_a_hundredth_of_travel() {
        let law = default_law("f");
        // -60..0 dB over 16320: 0.5 dB is 136 steps, 1 % of travel 163.
        assert!(is_fader_echo(&law, 16_320, 8_160, 8_160));
        assert!(is_fader_echo(&law, 16_320, 8_160, 8_323));
        assert!(!is_fader_echo(&law, 16_320, 8_160, 8_400));
        // On a steep law (the CQ's, near its bottom) the travel decides.
        let mut cq = default_law("f");
        cq.curve = compat::config_of(&{
            let mut s = cq_desk();
            s.bindings[1].curve = tessera_surface::presets::allen_heath_cq(1).1;
            s
        })
        .bindings[1]
            .curve
            .clone();
        cq.apply_curve_range();
        assert!(is_fader_echo(&cq, 16_320, 400, 550));
        assert!(!is_fader_echo(&cq, 16_320, 400, 900));
        // Off is off.
        assert!(is_fader_echo(&law, 16_320, 0, 0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_connection_the_engine_dropped_is_reported_again() {
        let e = TestEngine::start("surface-reassert", ONE_DEVICE);
        let root = Root::new("reassert", Some(&held()));
        let spec = desk(Bm::PushToTalk);
        let s = start(&e, &root, spec.clone(), false).await;
        wait_until("connected", || {
            let h = e.handle.clone();
            async move { engine_connected(&h).await }
        })
        .await;
        let before = engine_view(&e.handle, "desk").await.unwrap();
        assert_eq!(s.live().connected, before.connected);
        // Disabled and enabled again faster than the runtime looks: the
        // session keeps running with the same spec, the engine dropped the
        // connection and issued a new epoch.
        let mut off = spec.clone();
        off.enabled = false;
        for s in [off, spec] {
            e.handle
                .apply(Origin::Internal, Command::SetSurface { surface: s })
                .await
                .unwrap();
        }
        wait_until("the connection reported again", || {
            let h = e.handle.clone();
            async move { engine_connected(&h).await }
        })
        .await;
        let after = engine_view(&e.handle, "desk").await.unwrap();
        assert_ne!(after.connection_epoch, before.connection_epoch);
        assert!(s.live().connected);
        s.stop().await;
        assert!(!engine_connected(&e.handle).await);
        e.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_first_report_under_a_stale_epoch_is_made_again_under_the_new_one() {
        use std::io::Write;
        let e = TestEngine::start("surface-stale-epoch", ONE_DEVICE);
        let root = Root::new("stale-epoch", None);
        let spec = desk(Bm::Toggle);
        let s = start(&e, &root, spec.clone(), false).await;
        wait_until("the node open", || {
            let d = s.live().device.is_some();
            async move { d }
        })
        .await;
        // The session read the epoch at its start (just after the node
        // opened); the surface is disabled and enabled again before the
        // device reports.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut off = spec.clone();
        off.enabled = false;
        for s in [off, spec] {
            e.handle
                .apply(Origin::Internal, Command::SetSurface { surface: s })
                .await
                .unwrap();
        }
        let fifo = root.0.join("dev/hidraw3");
        let mut w = tokio::task::spawn_blocking(move || {
            std::fs::OpenOptions::new().write(true).open(fifo).unwrap()
        })
        .await
        .unwrap();
        w.write_all(&[1, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        wait_until("connected under the new epoch", || {
            let h = e.handle.clone();
            async move { engine_connected(&h).await }
        })
        .await;
        drop(w);
        s.stop().await;
        e.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_learn_pauses_the_liveness_probe() {
        let e = TestEngine::start("surface-learn-live", ONE_DEVICE);
        let desk = FakeDesk::start();
        let mut spec = tcp_desk("desk", desk.port, false);
        spec.bindings = vec![SurfaceBinding::new("f", Ba::Gain, tx2())];
        let surfaces = Arc::new(Surfaces::new(None));
        surfaces.set_liveness(Duration::from_millis(900));
        let (_specs, cancel, task) = run_on(&e, &surfaces, spec).await;
        wait_until("the get on the connection", || {
            let w = desk.got();
            async move { w.contains("b0 63 40 b0 62 00 b0 60 7f") }
        })
        .await;
        send_fader(&desk, 16_320);
        wait_until("connected", || {
            let h = e.handle.clone();
            async move { engine_connected(&h).await }
        })
        .await;
        // A learn watches the device for longer than the liveness: no
        // probe is sent (its answer would be learned), the session stays.
        let id = format!("tcp:127.0.0.1:{}", desk.port);
        lock(&surfaces.learning).insert(id.clone());
        desk.clear();
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert_eq!(desk.got(), "", "probed during a learn");
        assert!(engine_connected(&e.handle).await, "lost during a learn");
        assert_eq!(desk.accepts.load(Ordering::Relaxed), 1);
        // The learn ends: probing starts over.
        lock(&surfaces.learning).remove(&id);
        wait_until("a probe after the learn", || {
            let w = desk.got();
            async move { w.contains("b0 63 40 b0 62 00 b0 60 7f") }
        })
        .await;
        stop_all(vec![(cancel, task)]).await;
        e.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_full_output_queue_is_lost_and_keeps_the_push_to_talk_safety() {
        let e = TestEngine::start("surface-stall", ONE_DEVICE);
        // The desk reports its push-to-talk button held (an NRPN state:
        // on is live), then never takes another byte.
        let on = hex(&tessera_surface::midi::encode_control(&nrpn_addr(0), 1));
        let root = cq_root("stall", &format!("# sleep 200\n{on}\n# sleep 100000\n"));
        let mut spec = cq_desk();
        spec.bindings[0].mode = Bm::PushToTalk;
        e.handle
            .apply(
                Origin::Internal,
                Command::SetSurface {
                    surface: spec.clone(),
                },
            )
            .await
            .unwrap();
        let surfaces = Arc::new(Surfaces::new(Some(root.0.clone())));
        lock(&surfaces.live).insert(spec.name.clone(), Live::of(&spec, None));
        let deps = Deps {
            surfaces: Arc::clone(&surfaces),
            engine: e.handle.clone(),
            bus: e.bus.clone(),
        };
        let cfg = compat::config_of(&spec);
        let (out, _stuck) = reader::stalled_output(16);
        let node = root.0.join("dev/snd/midiC1D0");
        let reports = reader::open(&node, true).unwrap();
        let session = Session::new(
            &deps,
            spec.clone(),
            cfg.clone(),
            DevRef {
                id: CQ.into(),
                node,
            },
            Input::Midi(Box::new(MidiInput::new(&cfg, out))),
        );
        let (_tx, mut specs) = watch::channel(spec);
        let cancel = CancellationToken::new();
        let driver = async {
            wait_until("push-to-talk held: live and connected", || {
                let h = e.handle.clone();
                async move { engine_connected(&h).await && !muted(&h).await }
            })
            .await;
            // Gains set elsewhere move the motorized fader until the queue
            // is full.
            for i in 1..=40 {
                let _ = e
                    .handle
                    .apply(
                        Origin::Internal,
                        Command::SetChannelGain {
                            target: tx2(),
                            gain: tessera_control_types::Gain::MilliDb(-1_000 * i),
                        },
                    )
                    .await;
            }
        };
        let ((ended, reported), ()) =
            tokio::join!(session.run(reports, &mut specs, &cancel), driver);
        assert!(
            matches!(&ended, Ended::Lost(why) if why.contains("queue full")),
            "lost on the full queue"
        );
        assert!(reported);
        // Nothing sees the button any more: its target is muted.
        assert!(muted(&e.handle).await, "push-to-talk safety mute");
        assert!(!engine_connected(&e.handle).await);
        e.stop().await;
    }

    // ---- council round 3: epochs, levels set while dropped, answers -------

    /// The gain of TX channel 1.
    async fn tx1_gain(e: &EngineHandle) -> tessera_control_types::Gain {
        match e.query(Query::Status).await {
            Ok(Reply::Status(st)) => st.devices[0].tx_channels[0].gain,
            other => panic!("{other:?}"),
        }
    }

    async fn epoch_of(e: &EngineHandle) -> u64 {
        engine_view(e, "desk").await.unwrap().connection_epoch
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_level_set_while_the_connection_was_dropped_is_overruled_by_the_fader() {
        let e = TestEngine::start("surface-reassert-level", ONE_DEVICE);
        // The fader (no motor) at half travel, reported once.
        let root = Root::new(
            "reassert-level",
            Some(&format!("{}# sleep 100000\n", report([2048, 0, 0, 0], 0))),
        );
        let mut spec = desk(Bm::Toggle);
        spec.bindings
            .push(SurfaceBinding::new("f1", Ba::Gain, tx1()));
        let s = start(&e, &root, spec.clone(), false).await;
        wait_until("connected, the fader's level applied", || {
            let h = e.handle.clone();
            async move {
                engine_connected(&h).await
                    && tx1_gain(&h).await != tessera_control_types::Gain::default()
            }
        })
        .await;
        let fader = tx1_gain(&e.handle).await;
        // Disabled, set to 0 dB from elsewhere (nothing owns it now) and
        // enabled again, all queued before the session looks: it handles
        // the first `SurfacesChanged` with the gain event still queued.
        let mut off = spec.clone();
        off.enabled = false;
        let (a, b, c) = tokio::join!(
            e.handle
                .apply(Origin::Internal, Command::SetSurface { surface: off }),
            e.handle.apply(
                Origin::Internal,
                Command::SetChannelGain {
                    target: tx1(),
                    gain: tessera_control_types::Gain::MilliDb(0),
                },
            ),
            e.handle.apply(
                Origin::Internal,
                Command::SetSurface {
                    surface: spec.clone()
                }
            ),
        );
        a.unwrap();
        b.unwrap();
        c.unwrap();
        wait_until("the connection reported again", || {
            let h = e.handle.clone();
            async move { engine_connected(&h).await }
        })
        .await;
        // The fader owns the level again, at its real position.
        wait_until("the fader's level again", || {
            let h = e.handle.clone();
            async move { tx1_gain(&h).await == fader }
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(tx1_gain(&e.handle).await, fader, "not left at 0 dB");
        s.stop().await;
        e.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_session_on_an_old_source_never_claims_the_new_epoch() {
        let e = TestEngine::start("surface-old-source", ONE_DEVICE);
        let spec = desk(Bm::Toggle);
        e.handle
            .apply(
                Origin::Internal,
                Command::SetSurface {
                    surface: spec.clone(),
                },
            )
            .await
            .unwrap();
        let old = epoch_of(&e.handle).await;
        // Moved to another device: a new epoch.
        let mut moved = spec.clone();
        moved.source = SurfaceSource::Hidraw {
            vendor_id: 0x3142,
            product_id: 0x0c34,
            interface: Some(3),
        };
        e.handle
            .apply(
                Origin::Internal,
                Command::SetSurface {
                    surface: moved.clone(),
                },
            )
            .await
            .unwrap();
        // The session still on the old device is refused, not retried
        // under the new epoch.
        let r = report_connected(&e.handle, "desk", Some(old), &spec.source).await;
        assert!(
            matches!(&r, Err(CommandError::Invalid { field, .. }) if field == "epoch"),
            "{r:?}"
        );
        assert!(!engine_connected(&e.handle).await);
        // A session on the new device reports under the new epoch.
        let now = epoch_of(&e.handle).await;
        let r = report_connected(&e.handle, "desk", Some(old), &moved.source).await;
        assert_eq!(r.unwrap(), Some(now));
        assert!(engine_connected(&e.handle).await);
        // Disabled: refused, with or without an epoch.
        let mut off = moved.clone();
        off.enabled = false;
        e.handle
            .apply(Origin::Internal, Command::SetSurface { surface: off })
            .await
            .unwrap();
        for epoch in [None, Some(epoch_of(&e.handle).await)] {
            let r = report_connected(&e.handle, "desk", epoch, &moved.source).await;
            assert!(
                matches!(&r, Err(CommandError::Invalid { field, .. }) if field == "connected"),
                "{r:?}"
            );
        }
        assert!(!engine_connected(&e.handle).await);
        e.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_aborted_session_reports_its_disconnection_under_its_epoch() {
        let e = TestEngine::start("surface-abort", ONE_DEVICE);
        let spec = desk(Bm::Toggle);
        e.handle
            .apply(
                Origin::Internal,
                Command::SetSurface {
                    surface: spec.clone(),
                },
            )
            .await
            .unwrap();
        let old = epoch_of(&e.handle).await;
        report_connection(&e.handle, "desk", true, Some(old))
            .await
            .unwrap();
        // A session that reported under `old` and will not stop.
        let surfaces = Arc::new(Surfaces::new(None));
        let mut live = Live::of(&spec, None);
        live.connected = true;
        live.epoch = Some(old);
        lock(&surfaces.live).insert("desk".into(), live);
        // The engine moved on, and the surface is connected under the new
        // epoch meanwhile.
        let mut off = spec.clone();
        off.enabled = false;
        for s in [off, spec.clone()] {
            e.handle
                .apply(Origin::Internal, Command::SetSurface { surface: s })
                .await
                .unwrap();
        }
        let now = epoch_of(&e.handle).await;
        assert_ne!(now, old);
        report_connection(&e.handle, "desk", true, Some(now))
            .await
            .unwrap();
        let deps = Deps {
            surfaces: Arc::clone(&surfaces),
            engine: e.handle.clone(),
            bus: e.bus.clone(),
        };
        let (specs, _rx) = watch::channel(spec.clone());
        let stuck = super::Running {
            spec,
            specs,
            cancel: CancellationToken::new(),
            join: tokio::spawn(std::future::pending::<()>()),
        };
        stop(&deps, "desk", stuck).await;
        assert!(
            engine_connected(&e.handle).await,
            "the disconnection under the old epoch drops nothing newer"
        );
        assert!(!lock(&surfaces.live)["desk"].connected);
        e.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_answer_with_an_unchanged_value_counts_for_liveness() {
        let e = TestEngine::start("surface-tcp-same-answer", ONE_DEVICE);
        let desk = FakeDesk::start();
        let mut spec = tcp_desk("desk", desk.port, false);
        spec.bindings = vec![SurfaceBinding::new("f", Ba::Gain, tx2())];
        let surfaces = Arc::new(Surfaces::new(None));
        // A probe after 1 s of silence, lost after 3 s.
        surfaces.set_liveness(Duration::from_millis(3_000));
        let (_specs, cancel, task) = run_on(&e, &surfaces, spec).await;
        wait_until("the get on the connection", || {
            let w = desk.got();
            async move { w.contains("b0 63 40 b0 62 00 b0 60 7f") }
        })
        .await;
        // Not an answer (past the get's half second, before the first
        // probe): a hand move, which connects.
        tokio::time::sleep(Duration::from_millis(700)).await;
        desk.clear();
        send_fader(&desk, 8_000);
        wait_until("connected", || {
            let h = e.handle.clone();
            async move { engine_connected(&h).await }
        })
        .await;
        // The probe is answered at once with the same value (no change to
        // decode): it answers, so its silence now counts.
        wait_until("a liveness probe", || {
            let w = desk.got();
            async move { w.contains("b0 63 40 b0 62 00 b0 60 7f") }
        })
        .await;
        send_fader(&desk, 8_000);
        wait_until("lost for its silence", || {
            let h = e.handle.clone();
            async move { !engine_connected(&h).await }
        })
        .await;
        stop_all(vec![(cancel, task)]).await;
        e.stop().await;
    }
}
