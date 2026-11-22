//! The host (speaker amplifier) volume (provisional 0.x;
//! `docs/plans/2026-11-22-host-router.md` §5).
//!
//! `[host_volume]` of the configuration names an ALSA mixer control and the
//! range offered; `SetHostVolume` / `SetHostMute` save the value in
//! `state.toml` (`[host_volume]`), and the daemon sets the control and sets
//! it again at start. What the daemon last read from the control, and why
//! it cannot, are runtime observations
//! ([`StateEngine::report_host_volume`]), never saved.
//!
//! Fader priority covers the volume like any level: a non-motorized fader
//! bound to `host_volume` owns it while its surface is connected. Mute is
//! never owned.

use tessera_control_types::{
    Command, CommandError, Event, Gain, HostVolumeReading, HostVolumeStatus, Origin,
};

use super::{StateEngine, invalid};
use crate::persist::StateFile;

impl StateEngine {
    /// `Unsupported` unless `[host_volume]` is configured and the runtime
    /// sets it.
    pub(super) fn need_host_volume(&self, what: &str) -> Result<(), CommandError> {
        if self.config.config.host_volume.is_none() {
            return Err(CommandError::Unsupported {
                what: format!("{what}: no host volume is configured (`[host_volume]`)"),
            });
        }
        if !self.env.runtime.host_volume {
            return Err(CommandError::Unsupported {
                what: format!("{what}: this tesserad cannot set the host volume yet"),
            });
        }
        Ok(())
    }

    /// The host volume as the daemon should apply it: the configured
    /// control and range, the saved volume and mute; `None` without
    /// `[host_volume]`.
    #[must_use]
    pub fn host_volume_status(&self) -> Option<HostVolumeStatus> {
        let h = self.config.config.host_volume.as_ref()?;
        let mut s = HostVolumeStatus::new(&h.card, &h.control, h.min_mdb(), h.max_mdb());
        s.gain = self.intent.host_volume.gain;
        s.muted = self.intent.host_volume.muted;
        s.reading = self.host_volume_reading;
        s.gain_owner = self.host_volume_owner().map(str::to_owned);
        s.problem.clone_from(&self.host_volume_problem);
        if s.problem.is_none() && !self.env.runtime.host_volume {
            s.problem = Some("this tesserad cannot set the host volume yet".into());
        }
        Some(s)
    }

    /// Record what the daemon read from the host volume's control
    /// (`reading`) and why it cannot read or set it (`problem`; `None`: it
    /// works). A runtime observation: not saved, no event (the status shows
    /// it).
    pub fn report_host_volume(
        &mut self,
        reading: Option<HostVolumeReading>,
        problem: Option<String>,
    ) {
        self.host_volume_reading = reading;
        self.host_volume_problem = problem;
    }

    /// `mutate` for the host volume commands; `None` for any other command.
    pub(super) fn mutate_host_volume_intent(
        &self,
        intent: &mut StateFile,
        cmd: &Command,
    ) -> Option<Result<bool, CommandError>> {
        Some(match cmd {
            Command::SetHostVolume { gain } => {
                self.need_host_volume("the host volume").and_then(|()| {
                    let h = self
                        .config
                        .config
                        .host_volume
                        .as_ref()
                        .ok_or_else(|| invalid("gain", "no host volume is configured"))?;
                    if let Gain::MilliDb(m) = gain {
                        if *m < h.min_mdb() || *m > h.max_mdb() {
                            return Err(invalid(
                                "gain",
                                format!(
                                    "the host volume must be off or {}..={} milli-dB",
                                    h.min_mdb(),
                                    h.max_mdb()
                                ),
                            ));
                        }
                    }
                    let changed = intent.host_volume.gain != Some(*gain);
                    intent.host_volume.gain = Some(*gain);
                    Ok(changed)
                })
            }
            Command::SetHostMute { muted } => self.need_host_volume("the host mute").map(|()| {
                let changed = intent.host_volume.muted != *muted;
                intent.host_volume.muted = *muted;
                changed
            }),
            _ => return None,
        })
    }
}

/// `HostVolumeChanged` when the saved volume or mute changed.
pub(super) fn host_volume_events(
    before: &StateFile,
    after: &StateFile,
    origin: &Origin,
) -> Vec<Event> {
    if before.host_volume == after.host_volume {
        return Vec::new();
    }
    vec![Event::HostVolumeChanged {
        gain: after.host_volume.gain,
        muted: after.host_volume.muted,
        origin: origin.clone(),
    }]
}
