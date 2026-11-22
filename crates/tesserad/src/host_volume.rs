//! The host (speaker amplifier) volume: the saved `SetHostVolume` /
//! `SetHostMute` applied to the `[host_volume]` mixer control through the
//! ALSA mixer API (plan `docs/plans/2026-11-22-host-router.md` §4, §12
//! item 3). Control plane only; needs the `alsa-bridge` build (libasound).
//!
//! * A tokio task follows the state engine: at start, on `HostVolumeChanged`
//!   and on a reload it reads `Status::host_volume` and hands the wanted
//!   value to the mixer thread.
//! * A mute with no saved level first saves the current reading as the
//!   level (`SetHostVolume`, origin internal), so the unmute goes back to
//!   it, also after a restart. The mixer thread also remembers the level it
//!   read before such a mute and undoes the mute (level and switch) when
//!   the unmute comes with still no saved level, also when the card was
//!   opened again in between (the remembered level is kept: reading it
//!   again would read the bottom the mute left).
//! * One plain thread owns the mixer (alsa-lib's handle is not shareable).
//!   It opens the card's control (`hw:<card>`, the simple control by name),
//!   sets the saved value (`Gain::Off`: the control's bottom; a gain within
//!   the configured range; mute: the playback switch, or the bottom when the
//!   control has none; nothing saved: the control is left as it is), and
//!   then polls the mixer's descriptors with a wake socket: a change made by
//!   another program is read back and reported
//!   (`StateEngine::report_host_volume`, shown as `Status::host_volume.
//!   reading`), a new wanted value is applied at once. A card that is
//!   missing or goes away is reported as the problem and tried again every
//!   [`RETRY`]; when it is back the saved value is set again.

use tessera_control_types::{Event, Gain, HostVolumeReading, HostVolumeStatus, Query, Reply};
use tessera_state::EventBus;
use tokio_util::sync::CancellationToken;

use crate::engine_thread::EngineHandle;

/// This build can set the host volume (it links libasound).
pub const SUPPORTED: bool = cfg!(feature = "alsa-bridge");

/// How often a missing card is looked for again.
pub const RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// What the mixer thread should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    /// ALSA card id or index.
    pub card: String,
    /// Simple mixer control.
    pub control: String,
    /// Range offered, milli-dB.
    pub min_mdb: i32,
    /// Range offered, milli-dB.
    pub max_mdb: i32,
    /// Saved volume (`None`: never set).
    pub gain: Option<Gain>,
    /// Saved mute.
    pub muted: bool,
}

impl Wanted {
    /// From the engine's status.
    #[must_use]
    pub fn of(s: &HostVolumeStatus) -> Self {
        Self {
            card: s.card.clone(),
            control: s.control.clone(),
            min_mdb: s.min_mdb,
            max_mdb: s.max_mdb,
            gain: s.gain,
            muted: s.muted,
        }
    }

    /// The level to set (`Some(None)`: the bottom of the control) and the
    /// mute; `None`: nothing was ever saved, leave the control alone.
    #[must_use]
    pub fn target(&self) -> Option<(Option<Option<i32>>, bool)> {
        match self.gain {
            None if !self.muted => None,
            None => Some((None, true)),
            Some(g) => Some((
                Some(g.milli_db().map(|m| m.clamp(self.min_mdb, self.max_mdb))),
                self.muted,
            )),
        }
    }
}

/// What the mixer thread does to the control for a wanted value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing saved and nothing to undo: leave the control alone.
    Leave,
    /// Set the level (`None`: the bottom) and the mute.
    Set(Option<i32>, bool),
    /// Mute with no saved level: the switch, or the bottom without one
    /// (the thread first remembers the level it read, to restore it).
    Mute,
    /// Undo a mute this daemon applied with no saved level: the level
    /// remembered before it (`Some`) and the switch, or only the switch.
    Unmute(Option<i32>),
}

/// The action for `w`, given the value applied before on this open of the
/// control (`None`: nothing yet) and the level remembered before the last
/// mute.
#[must_use]
pub fn action(w: &Wanted, before: Option<&Wanted>, premute: Option<i32>) -> Action {
    match w.target() {
        Some((Some(level), muted)) => Action::Set(level, muted),
        Some((None, _)) => Action::Mute,
        // Never saved and unmuted: leave the control alone, unless this
        // undoes a mute we applied (else unmuting would leave a control
        // without a switch at its bottom, or the switch off, for good). A
        // remembered level is such a mute too: it survives the control
        // being opened again (`before` is then `None`).
        None if premute.is_some() || before.is_some_and(|b| b.muted) => Action::Unmute(premute),
        None => Action::Leave,
    }
}

/// The level to remember for a [`Action::Mute`]: the level read now, taken
/// only for a mute this daemon has not applied already, neither on this
/// open of the control (`before` muted) nor before it was opened again (a
/// remembered level is kept): either way the control is at its bottom now
/// and the bottom would be remembered.
#[must_use]
pub fn premute_for_mute(
    before: Option<&Wanted>,
    premute: Option<i32>,
    read: impl FnOnce() -> Option<i32>,
) -> Option<i32> {
    if premute.is_some() || before.is_some_and(|b| b.muted) {
        premute
    } else {
        read()
    }
}

/// The level remembered before our mute, once the wanted value goes from
/// `last` to `want`: forgotten when `[host_volume]` is removed or names
/// another card or control (that level belongs to a control no longer
/// configured; an unmute must not set it on another one), also while the
/// control is not open.
#[must_use]
pub fn premute_on_retarget(
    premute: Option<i32>,
    last: Option<&Wanted>,
    want: Option<&Wanted>,
) -> Option<i32> {
    match (last, want) {
        (_, None) => None,
        (Some(l), Some(w)) if l.card != w.card || l.control != w.control => None,
        _ => premute,
    }
}

/// The mixer calls the thread makes (the ALSA mixer; tests fake it).
pub trait Mixer {
    /// Set the level (`None`: the bottom) and the mute.
    ///
    /// # Errors
    /// The mixer's error.
    fn set(&self, mdb: Option<i32>, muted: bool) -> Result<(), String>;
    /// Set only the mute.
    ///
    /// # Errors
    /// The mixer's error.
    fn set_muted(&self, muted: bool) -> Result<(), String>;
    /// The level now, milli-dB (`None`: the bottom, or unreadable).
    fn level(&self) -> Option<i32>;
}

/// Apply `w` to the control (see [`action`]), keeping `premute`: set when
/// our mute remembers the level, forgotten only once a level was set or an
/// unmute applied (a write that failed keeps it: the control is opened
/// again and the unmute still has the level to go back to).
///
/// # Errors
/// The mixer's error.
pub fn apply(
    m: &impl Mixer,
    w: &Wanted,
    applied: Option<&Wanted>,
    premute: &mut Option<i32>,
) -> Result<(), String> {
    match action(w, applied, *premute) {
        Action::Leave => Ok(()),
        Action::Set(level, muted) => {
            m.set(level, muted)?;
            *premute = None;
            Ok(())
        }
        Action::Mute => {
            // Remember the level, unless this is already our mute (the
            // bottom would be remembered), also from before the control
            // was opened again.
            *premute = premute_for_mute(applied, *premute, || m.level());
            m.set_muted(true)
        }
        Action::Unmute(level) => {
            match level {
                Some(l) => m.set(Some(l), false)?,
                None => m.set_muted(false)?,
            }
            *premute = None;
            Ok(())
        }
    }
}

/// When a mute arrives with no saved level, the level to save first (a
/// `SetHostVolume` of the current reading, clamped to the range), so the
/// unmute has a level to go back to also after a restart. `None`: nothing
/// to save (a level is saved, not muted, or no reading above the bottom).
#[must_use]
pub fn level_to_keep(s: &HostVolumeStatus) -> Option<Gain> {
    if s.gain.is_some() || !s.muted {
        return None;
    }
    s.reading?
        .gain
        .milli_db()
        .map(|m| Gain::MilliDb(m.clamp(s.min_mdb, s.max_mdb)))
}

/// A mixer reading as the contract reports it (clamped to the gain range).
#[must_use]
pub fn reading(mdb: Option<i32>, muted: bool) -> HostVolumeReading {
    HostVolumeReading::new(
        mdb.map_or(Gain::Off, |m| Gain::MilliDb(m.clamp(-96_000, 12_000))),
        muted,
    )
}

/// The wanted value from the engine's status. A mute with no saved level
/// first saves the current reading as the level (see [`level_to_keep`]).
async fn wanted(engine: &EngineHandle) -> Option<Option<Wanted>> {
    let s = match engine.query(Query::Status).await {
        Ok(Reply::Status(s)) => s,
        _ => return None,
    };
    if let Some(g) = s.host_volume.as_ref().and_then(level_to_keep) {
        let cmd = tessera_control_types::Command::SetHostVolume { gain: g };
        match engine
            .apply(tessera_control_types::Origin::Internal, cmd)
            .await
        {
            Ok(_) => {
                return match engine.query(Query::Status).await {
                    Ok(Reply::Status(s)) => Some(s.host_volume.as_ref().map(Wanted::of)),
                    _ => None,
                };
            }
            Err(e) => tracing::debug!("host volume: keeping the level before the mute: {e}"),
        }
    }
    Some(s.host_volume.as_ref().map(Wanted::of))
}

/// Run until `shutdown` (supervised by the daemon).
pub async fn run(engine: EngineHandle, bus: EventBus, shutdown: CancellationToken) {
    #[cfg(feature = "alsa-bridge")]
    imp::run(engine, bus, shutdown).await;
    #[cfg(not(feature = "alsa-bridge"))]
    {
        // Nothing to run (the engine refuses the commands); returning early
        // would make the supervisor restart it.
        let _ = (engine, bus, wanted, relevant);
        shutdown.cancelled().await;
    }
}

fn relevant(e: &Event) -> bool {
    matches!(
        e,
        Event::HostVolumeChanged { .. }
            | Event::ConfigReloaded
            | Event::ConfigReloadedWithOrigin { .. }
    )
}

#[cfg(feature = "alsa-bridge")]
mod imp {
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;

    use tessera_backend_alsa::mixer::HostMixer;
    use tessera_state::Received;

    use super::{
        CancellationToken, EngineHandle, EventBus, Mixer, RETRY, Wanted, apply,
        premute_on_retarget, reading, relevant, wanted,
    };

    impl Mixer for HostMixer {
        fn set(&self, mdb: Option<i32>, muted: bool) -> Result<(), String> {
            HostMixer::set(self, mdb, muted)
        }
        fn set_muted(&self, muted: bool) -> Result<(), String> {
            HostMixer::set_muted(self, muted)
        }
        fn level(&self) -> Option<i32> {
            self.read().ok().and_then(|r| r.mdb)
        }
    }

    pub(super) async fn run(engine: EngineHandle, bus: EventBus, shutdown: CancellationToken) {
        let mut sub = bus.subscribe(256);
        let (tx, rx) = mpsc::channel::<Option<Wanted>>();
        let Ok((mut wake_w, wake_r)) = UnixStream::pair() else {
            tracing::error!("host volume: cannot create the wake socket");
            return;
        };
        let _ = wake_w.set_nonblocking(true);
        let e2 = engine.clone();
        let thread = std::thread::Builder::new()
            .name("tessera-volume".into())
            .spawn(move || body(&rx, wake_r, &e2));
        let Ok(thread) = thread else {
            tracing::error!("host volume: cannot start its thread");
            return;
        };
        let mut sent: Option<Option<Wanted>> = None;
        let mut stale = true;
        loop {
            if stale {
                while sub.try_recv().is_ok() {}
                match wanted(&engine).await {
                    Some(w) => {
                        if sent.as_ref() != Some(&w) {
                            if tx.send(w.clone()).is_err() {
                                break;
                            }
                            let _ = wake_w.write(&[1]);
                            sent = Some(w);
                        }
                    }
                    None => {
                        tokio::select! {
                            () = shutdown.cancelled() => break,
                            () = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
                        }
                        continue;
                    }
                }
            }
            tokio::select! {
                () = shutdown.cancelled() => break,
                r = sub.recv_async() => match r {
                    Ok(Received::Event(e)) => stale = relevant(&e),
                    Ok(Received::Lagged(_)) => stale = true,
                    Err(_) => break,
                },
            }
        }
        drop(tx);
        let _ = wake_w.write(&[0]);
        let _ = tokio::task::spawn_blocking(move || thread.join()).await;
    }

    /// The mixer thread (see the module documentation).
    fn body(rx: &mpsc::Receiver<Option<Wanted>>, mut wake: UnixStream, engine: &EngineHandle) {
        let mut want: Option<Wanted> = None;
        let mut open: Option<HostMixer> = None;
        let mut applied: Option<Wanted> = None;
        // The level read before a mute with no saved level.
        let mut premute: Option<i32> = None;
        // The wanted value seen last (its card and control own `premute`).
        let mut seen: Option<Wanted> = None;
        let mut reported: Option<(
            Option<tessera_control_types::HostVolumeReading>,
            Option<String>,
        )> = None;
        let _ = wake.set_nonblocking(true);
        let mut report = |r: Option<tessera_control_types::HostVolumeReading>,
                          p: Option<String>| {
            let now = (r, p);
            if reported.as_ref() != Some(&now) {
                if let Some(m) = &now.1 {
                    tracing::warn!("host volume: {m}");
                }
                if engine.report_host_volume(now.0, now.1.clone()) {
                    reported = Some(now);
                }
            }
        };
        loop {
            // The newest wanted value; a closed channel ends the thread.
            loop {
                match rx.try_recv() {
                    Ok(w) => want = w,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            }
            let mut buf = [0u8; 64];
            while matches!(wake.read(&mut buf), Ok(n) if n > 0) {}
            // Removed or moved to another control (open or not): the level
            // remembered belongs to the old one.
            premute = premute_on_retarget(premute, seen.as_ref(), want.as_ref());
            seen.clone_from(&want);
            let Some(w) = want.clone() else {
                open = None;
                applied = None;
                // Nothing configured: wait for a value.
                match rx.recv() {
                    Ok(w) => want = w,
                    Err(_) => return,
                }
                continue;
            };
            if open
                .as_ref()
                .is_some_and(|m| m.card() != w.card || m.control() != w.control)
            {
                open = None;
                applied = None;
            }
            if open.is_none() {
                match HostMixer::open(&w.card, &w.control) {
                    Ok(m) => {
                        tracing::info!(
                            "host volume: mixer control `{}` of `{}` opened",
                            w.control,
                            w.card
                        );
                        open = Some(m);
                        applied = None;
                    }
                    Err(e) => {
                        report(None, Some(e));
                        sleep_or_wake(&wake);
                        continue;
                    }
                }
            }
            let Some(m) = open.as_ref() else { continue };
            if applied.as_ref() != Some(&w) {
                if let Err(e) = apply(m, &w, applied.as_ref(), &mut premute) {
                    report(None, Some(e));
                    open = None;
                    sleep_or_wake(&wake);
                    continue;
                }
                applied = Some(w.clone());
            }
            match m.read() {
                Ok(r) => report(Some(reading(r.mdb, r.muted)), None),
                Err(e) => {
                    report(None, Some(e));
                    open = None;
                    continue;
                }
            }
            match m.wait(
                wake.as_fd(),
                i32::try_from(RETRY.as_millis()).unwrap_or(2_000),
            ) {
                Ok(woken) if woken.lost => {
                    report(None, Some(format!("sound card `{}` went away", w.card)));
                    open = None;
                }
                Ok(_) => {}
                Err(e) => {
                    report(None, Some(e));
                    open = None;
                    sleep_or_wake(&wake);
                }
            }
        }
    }

    /// Wait [`RETRY`] or until woken.
    fn sleep_or_wake(wake: &UnixStream) {
        let mut fds = [nix::poll::PollFd::new(
            wake.as_fd(),
            nix::poll::PollFlags::POLLIN,
        )];
        let ms = u16::try_from(RETRY.as_millis()).unwrap_or(2_000);
        let _ = nix::poll::poll(&mut fds, ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(gain: Option<Gain>, muted: bool) -> Wanted {
        Wanted {
            card: "Headphones".into(),
            control: "Digital".into(),
            min_mdb: -60_000,
            max_mdb: 0,
            gain,
            muted,
        }
    }

    #[test]
    fn nothing_saved_leaves_the_control_alone() {
        assert_eq!(w(None, false).target(), None);
        assert_eq!(w(None, true).target(), Some((None, true)));
    }

    #[test]
    fn a_saved_gain_is_kept_in_the_configured_range_and_off_is_the_bottom() {
        assert_eq!(
            w(Some(Gain::MilliDb(-20_000)), false).target(),
            Some((Some(Some(-20_000)), false))
        );
        assert_eq!(
            w(Some(Gain::MilliDb(6_000)), true).target(),
            Some((Some(Some(0)), true))
        );
        assert_eq!(
            w(Some(Gain::MilliDb(-90_000)), false).target(),
            Some((Some(Some(-60_000)), false))
        );
        assert_eq!(
            w(Some(Gain::Off), false).target(),
            Some((Some(None), false))
        );
        assert_eq!(reading(None, true), HostVolumeReading::new(Gain::Off, true));
        assert_eq!(
            reading(Some(-120_000), false),
            HostVolumeReading::new(Gain::MilliDb(-96_000), false)
        );
    }

    #[test]
    fn an_unmute_with_nothing_saved_undoes_our_mute() {
        let muted = w(None, true);
        let unmuted = w(None, false);
        assert_eq!(action(&muted, None, None), Action::Mute);
        // Unmute after our mute: back to the level read before it, or (none
        // remembered, e.g. a control with a switch at its bottom) the switch.
        assert_eq!(
            action(&unmuted, Some(&muted), Some(-20_000)),
            Action::Unmute(Some(-20_000))
        );
        assert_eq!(action(&unmuted, Some(&muted), None), Action::Unmute(None));
        // Nothing saved and never muted by us: left alone.
        assert_eq!(action(&unmuted, None, None), Action::Leave);
        assert_eq!(action(&unmuted, Some(&unmuted), None), Action::Leave);
        // A saved level is set as it is.
        assert_eq!(
            action(&w(Some(Gain::MilliDb(-10_000)), false), Some(&muted), None),
            Action::Set(Some(-10_000), false)
        );
    }

    #[test]
    fn a_mute_survives_the_card_opened_again() {
        let muted = w(None, true);
        let unmuted = w(None, false);
        let bottom = || Some(-60_000);
        // Our first mute remembers the level read.
        let premute = premute_for_mute(None, None, || Some(-20_000));
        assert_eq!(premute, Some(-20_000));
        // Still muted on this open: the bottom is not read.
        assert_eq!(
            premute_for_mute(Some(&muted), premute, bottom),
            Some(-20_000)
        );
        // The card went away and came back (nothing applied on this open):
        // the mute is applied again, the level remembered is kept...
        assert_eq!(action(&muted, None, premute), Action::Mute);
        let premute = premute_for_mute(None, premute, bottom);
        assert_eq!(premute, Some(-20_000));
        // ... and the unmute goes back to it, on this open or after
        // another one.
        assert_eq!(
            action(&unmuted, Some(&muted), premute),
            Action::Unmute(Some(-20_000))
        );
        assert_eq!(
            action(&unmuted, None, premute),
            Action::Unmute(Some(-20_000))
        );
        // Not our mute before: the level is read.
        assert_eq!(
            premute_for_mute(Some(&unmuted), None, || Some(-7_000)),
            Some(-7_000)
        );
    }

    /// A mixer control that records what is set; `fail` makes writes fail.
    #[derive(Debug, Default)]
    struct Fake {
        level: std::cell::Cell<Option<i32>>,
        muted: std::cell::Cell<bool>,
        fail: std::cell::Cell<bool>,
    }

    impl Mixer for Fake {
        fn set(&self, mdb: Option<i32>, muted: bool) -> Result<(), String> {
            if self.fail.get() {
                return Err("gone".into());
            }
            self.level.set(mdb);
            self.muted.set(muted);
            Ok(())
        }
        fn set_muted(&self, muted: bool) -> Result<(), String> {
            if self.fail.get() {
                return Err("gone".into());
            }
            self.muted.set(muted);
            Ok(())
        }
        fn level(&self) -> Option<i32> {
            self.level.get()
        }
    }

    #[test]
    fn a_failed_unmute_keeps_the_level_remembered() {
        let (muted, unmuted) = (w(None, true), w(None, false));
        let m = Fake::default();
        m.level.set(Some(-20_000));
        let mut premute = None;
        apply(&m, &muted, None, &mut premute).unwrap();
        assert_eq!(premute, Some(-20_000));
        assert!(m.muted.get());
        // The unmute fails (the card went away): the level is kept ...
        m.fail.set(true);
        assert!(apply(&m, &unmuted, Some(&muted), &mut premute).is_err());
        assert_eq!(premute, Some(-20_000));
        // ... and set when the control is opened again.
        m.fail.set(false);
        m.level.set(Some(-60_000));
        apply(&m, &unmuted, None, &mut premute).unwrap();
        assert_eq!(premute, None);
        assert_eq!((m.level.get(), m.muted.get()), (Some(-20_000), false));
        // A saved level that fails to be set keeps it too.
        let mut premute = Some(-7_000);
        m.fail.set(true);
        let saved = w(Some(Gain::MilliDb(-10_000)), false);
        assert!(apply(&m, &saved, None, &mut premute).is_err());
        assert_eq!(premute, Some(-7_000));
    }

    #[test]
    fn the_level_remembered_is_forgotten_for_another_control() {
        let here = w(None, true);
        let mut other = w(None, false);
        other.card = "Speaker".into();
        let mut other_control = w(None, false);
        other_control.control = "Master".into();
        // Same control: kept (also the first value seen).
        assert_eq!(
            premute_on_retarget(Some(-20_000), None, Some(&here)),
            Some(-20_000)
        );
        assert_eq!(
            premute_on_retarget(Some(-20_000), Some(&here), Some(&w(None, false))),
            Some(-20_000)
        );
        // `[host_volume]` removed, or another card or control (open or not).
        assert_eq!(premute_on_retarget(Some(-20_000), Some(&here), None), None);
        assert_eq!(
            premute_on_retarget(Some(-20_000), Some(&here), Some(&other)),
            None
        );
        assert_eq!(
            premute_on_retarget(Some(-20_000), Some(&here), Some(&other_control)),
            None
        );
        // So an unmute there leaves that control alone.
        assert_eq!(action(&other, None, None), Action::Leave);
    }

    #[test]
    fn a_mute_with_no_saved_level_keeps_the_reading_first() {
        let mut s = HostVolumeStatus::new("Headphones", "Digital", -60_000, 0);
        s.reading = Some(HostVolumeReading::new(Gain::MilliDb(-15_000), false));
        assert_eq!(level_to_keep(&s), None, "not muted");
        s.muted = true;
        assert_eq!(level_to_keep(&s), Some(Gain::MilliDb(-15_000)));
        // Kept inside the range the engine accepts.
        s.reading = Some(HostVolumeReading::new(Gain::MilliDb(3_000), true));
        assert_eq!(level_to_keep(&s), Some(Gain::MilliDb(0)));
        // At the bottom there is nothing to go back to.
        s.reading = Some(HostVolumeReading::new(Gain::Off, false));
        assert_eq!(level_to_keep(&s), None);
        s.reading = None;
        assert_eq!(level_to_keep(&s), None);
        // A saved level is already what the unmute goes back to.
        s.reading = Some(HostVolumeReading::new(Gain::MilliDb(-15_000), false));
        s.gain = Some(Gain::MilliDb(-30_000));
        assert_eq!(level_to_keep(&s), None);
    }

    #[test]
    fn only_volume_events_and_reloads_wake_it() {
        assert!(relevant(&Event::HostVolumeChanged {
            gain: Some(Gain::Off),
            muted: false,
            origin: tessera_control_types::Origin::Internal,
        }));
        assert!(relevant(&Event::ConfigReloaded));
        assert!(!relevant(&Event::RouterChanged {
            origin: tessera_control_types::Origin::Internal
        }));
    }
}
