//! A simple mixer control of a sound card, read and set through the ALSA
//! mixer API (alsa-lib's `snd_mixer_*`; no `amixer` process): the daemon's
//! host (speaker amplifier) volume (`[host_volume]`, plan
//! `docs/plans/2026-11-22-host-router.md` §4).
//!
//! Control plane only: every call may block briefly on the card's control
//! device and allocates. [`HostMixer::wait`] polls the mixer's descriptors
//! together with a caller's wake descriptor, so one thread can both follow
//! external changes (another program moving the control) and apply new
//! values at once.
//!
//! Units: milli-dB on this side; ALSA's "dB" calls take hundredths of a dB
//! ([`MilliBel`]).

use std::os::fd::{AsRawFd, BorrowedFd};

use alsa::Round;
use alsa::mixer::{MilliBel, Mixer, Selem, SelemChannelId, SelemId};
use alsa::poll::{Descriptors, Flags, pollfd};

/// What the control reads now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixerReading {
    /// The first channel's level, milli-dB; `None` at the bottom of the
    /// control (off).
    pub mdb: Option<i32>,
    /// The playback switch is off (always `false` for a control without
    /// one).
    pub muted: bool,
}

/// What [`HostMixer::wait`] saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Woken {
    /// The control changed (or the card reported something): read again.
    pub changed: bool,
    /// The caller's wake descriptor is readable.
    pub woken: bool,
    /// The card is gone (an error or hang-up on its control device).
    pub lost: bool,
}

/// One open simple mixer control.
#[derive(Debug)]
pub struct HostMixer {
    mixer: Mixer,
    id: SelemId,
    card: String,
    control: String,
}

/// The ALSA control device of a card named by id (`Headphones`) or index
/// (`0`).
#[must_use]
pub fn device_name(card: &str) -> String {
    format!("hw:{card}")
}

impl HostMixer {
    /// Open `control` (a simple mixer control with a playback volume) of
    /// `card`.
    ///
    /// # Errors
    /// The card cannot be opened, or it has no such control, or the control
    /// has no playback volume; in words.
    pub fn open(card: &str, control: &str) -> Result<Self, String> {
        let mixer = Mixer::new(&device_name(card), true)
            .map_err(|e| format!("sound card `{card}` cannot be opened: {e}"))?;
        let id = SelemId::new(control, 0);
        let s = mixer
            .find_selem(&id)
            .ok_or_else(|| format!("sound card `{card}` has no mixer control `{control}`"))?;
        if !s.has_playback_volume() {
            return Err(format!(
                "mixer control `{control}` of `{card}` has no playback volume"
            ));
        }
        Ok(Self {
            mixer,
            id,
            card: card.to_owned(),
            control: control.to_owned(),
        })
    }

    /// The card it was opened on.
    #[must_use]
    pub fn card(&self) -> &str {
        &self.card
    }

    /// The control it was opened on.
    #[must_use]
    pub fn control(&self) -> &str {
        &self.control
    }

    fn selem(&self) -> Result<Selem<'_>, String> {
        self.mixer.find_selem(&self.id).ok_or_else(|| {
            format!(
                "mixer control `{}` of `{}` went away",
                self.control, self.card
            )
        })
    }

    /// The control's dB range, milli-dB (its bottom may mean off).
    ///
    /// # Errors
    /// The control went away.
    pub fn range_mdb(&self) -> Result<(i32, i32), String> {
        let (lo, hi) = self.selem()?.get_playback_db_range();
        Ok((mb_to_mdb(lo), mb_to_mdb(hi)))
    }

    /// Read the control now.
    ///
    /// # Errors
    /// The control went away or the card refused the read.
    pub fn read(&self) -> Result<MixerReading, String> {
        let s = self.selem()?;
        let ch = SelemChannelId::mono();
        let (min, _) = s.get_playback_volume_range();
        let raw = s
            .get_playback_volume(ch)
            .map_err(|e| format!("reading `{}`: {e}", self.control))?;
        let mdb = if raw <= min {
            None
        } else {
            Some(mb_to_mdb(s.get_playback_vol_db(ch).map_err(|e| {
                format!("reading `{}` in dB: {e}", self.control)
            })?))
        };
        let muted = s.has_playback_switch()
            && s.get_playback_switch(ch)
                .map_err(|e| format!("reading the switch of `{}`: {e}", self.control))?
                == 0;
        Ok(MixerReading { mdb, muted })
    }

    /// Set the level (`None`: the bottom of the control) and the mute: the
    /// playback switch, or the bottom of the control when it has none.
    ///
    /// # Errors
    /// The control went away or the card refused the value.
    pub fn set(&self, mdb: Option<i32>, muted: bool) -> Result<(), String> {
        let s = self.selem()?;
        let (min, _) = s.get_playback_volume_range();
        let switch = s.has_playback_switch();
        let err = |e: alsa::Error| format!("setting `{}`: {e}", self.control);
        match mdb {
            Some(m) if !(muted && !switch) => s
                .set_playback_db_all(MilliBel(i64::from(m) / 10), Round::Floor)
                .map_err(err)?,
            _ => s.set_playback_volume_all(min).map_err(err)?,
        }
        if switch {
            s.set_playback_switch_all(i32::from(!muted)).map_err(err)?;
        }
        Ok(())
    }

    /// Set only the mute: the playback switch, or (without one) the bottom
    /// of the control when muting; unmuting a control without a switch
    /// leaves it where it is.
    ///
    /// # Errors
    /// The control went away or the card refused the value.
    pub fn set_muted(&self, muted: bool) -> Result<(), String> {
        let s = self.selem()?;
        let err = |e: alsa::Error| format!("setting `{}`: {e}", self.control);
        if s.has_playback_switch() {
            s.set_playback_switch_all(i32::from(!muted)).map_err(err)
        } else if muted {
            let (min, _) = s.get_playback_volume_range();
            s.set_playback_volume_all(min).map_err(err)
        } else {
            Ok(())
        }
    }

    /// Wait up to `timeout_ms` (`-1`: for ever) for the control to change or
    /// `wake` to become readable; then let alsa-lib take the card's events.
    ///
    /// # Errors
    /// The poll itself failed.
    pub fn wait(&self, wake: BorrowedFd<'_>, timeout_ms: i32) -> Result<Woken, String> {
        let mut fds = self
            .mixer
            .get()
            .map_err(|e| format!("mixer descriptors: {e}"))?;
        let n = fds.len();
        fds.push(pollfd {
            fd: wake.as_raw_fd(),
            events: Flags::IN.bits(),
            revents: 0,
        });
        alsa::poll::poll(&mut fds, timeout_ms).map_err(|e| format!("poll: {e}"))?;
        let mut w = Woken {
            woken: fds
                .get(n)
                .is_some_and(|p| Flags::from_bits_truncate(p.revents).contains(Flags::IN)),
            ..Woken::default()
        };
        let card = fds.get(..n).unwrap_or_default();
        let any = card.iter().any(|p| p.revents != 0);
        if any {
            let r = self.mixer.revents(card).unwrap_or(Flags::ERR);
            if r.intersects(Flags::ERR | Flags::HUP | Flags::NVAL) {
                w.lost = true;
            } else {
                w.changed = true;
            }
            if self.mixer.handle_events().is_err() {
                w.lost = true;
            }
        }
        Ok(w)
    }
}

fn mb_to_mdb(v: MilliBel) -> i32 {
    i32::try_from(v.0.saturating_mul(10)).unwrap_or(if v.0 < 0 { i32::MIN } else { i32::MAX })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_card_is_an_error_in_words() {
        let e = HostMixer::open("tessera-no-such-card", "Digital").unwrap_err();
        assert!(e.contains("tessera-no-such-card"), "{e}");
        assert_eq!(device_name("Headphones"), "hw:Headphones");
        assert_eq!(mb_to_mdb(MilliBel(-6_000)), -60_000);
    }
}
