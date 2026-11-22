//! Mixer panels (provisional 0.x; `docs/plans/2026-11-22-mixer-panel.md`).
//!
//! A panel is saved intent (`[[mixer]]` in `state.toml`, in id order). It
//! changes no audio, needs no runtime support and creates no binding: a
//! strip's links to hardware controls are derived from the surfaces'
//! bindings (`StripControl::links_to`). A strip whose device, channel, port
//! or crosspoint later goes away is kept as saved (the portal shows it as
//! not available); a strip that is saved unchanged is not checked against
//! them again, so the rest of the panel stays editable.

use std::collections::BTreeSet;

use tessera_control_types::{
    BindingAction, Command, CommandError, CrosspointOrigin, Event, FaderRange, GAIN_CEILING_MDB,
    GAIN_FLOOR_MDB, MixerSpec, MixerStrip, Origin, StripControl,
};

use super::{StateEngine, not_found};
use crate::persist::StateFile;

/// Longest panel or strip id, bytes.
const MAX_ID_BYTES: usize = 64;
/// Longest panel title, bytes.
const MAX_TITLE_BYTES: usize = 63;
/// Longest strip label or group heading, bytes.
const MAX_LABEL_BYTES: usize = 31;

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn bad(reason: impl Into<String>) -> CommandError {
    CommandError::InvalidMixer {
        reason: reason.into(),
    }
}

fn text_ok(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(char::is_control)
}

impl StateEngine {
    /// The saved mixer panels, in id order.
    #[must_use]
    pub fn mixers(&self) -> &[MixerSpec] {
        &self.intent.mixer
    }

    /// The whole check of a panel (`InvalidMixer`, or `Unsupported` for a
    /// host strip without a host volume).
    fn check_mixer(&self, m: &MixerSpec, intent: &StateFile) -> Result<(), CommandError> {
        if !valid_id(&m.id) {
            return Err(bad(format!(
                "the id `{}` must be 1..={MAX_ID_BYTES} letters, digits, `-` or `_`",
                m.id
            )));
        }
        if m.title.trim().is_empty() || !text_ok(&m.title, MAX_TITLE_BYTES) {
            return Err(bad(format!(
                "the title must be 1..={MAX_TITLE_BYTES} bytes without control characters"
            )));
        }
        let saved = intent.mixer.iter().find(|o| o.id == m.id);
        let mut ids = BTreeSet::new();
        for s in &m.strips {
            if !valid_id(&s.id) {
                return Err(bad(format!(
                    "strip `{}`: the id must be 1..={MAX_ID_BYTES} letters, digits, `-` or `_`",
                    s.id
                )));
            }
            if !ids.insert(s.id.as_str()) {
                return Err(bad(format!("strip `{}` is listed twice", s.id)));
            }
            if !text_ok(&s.label, MAX_LABEL_BYTES) {
                return Err(bad(format!(
                    "strip `{}`: the label must be 0..={MAX_LABEL_BYTES} bytes without control characters",
                    s.id
                )));
            }
            if s.group
                .as_ref()
                .is_some_and(|g| g.trim().is_empty() || !text_ok(g, MAX_LABEL_BYTES))
            {
                return Err(bad(format!(
                    "strip `{}`: a group heading must be 1..={MAX_LABEL_BYTES} bytes without control characters",
                    s.id
                )));
            }
            let unchanged = saved.is_some_and(|o| o.strips.iter().any(|x| x.control == s.control));
            self.check_strip(s, unchanged, intent)
                .map_err(|e| match e {
                    CommandError::InvalidMixer { reason } => {
                        bad(format!("strip `{}`: {reason}", s.id))
                    }
                    other => other,
                })?;
        }
        Ok(())
    }

    /// One strip control: the action fits the control kind and its target
    /// fields; the target exists unless `unchanged`.
    fn check_strip(
        &self,
        s: &MixerStrip,
        unchanged: bool,
        intent: &StateFile,
    ) -> Result<(), CommandError> {
        let c = &s.control;
        let action = c.action();
        let (fader, range) = match c {
            StripControl::Fader { range, .. } => (true, *range),
            StripControl::Button { .. } => (false, None),
            _ => return Err(bad("this control kind is not known to this tesserad")),
        };
        if action == BindingAction::Follow {
            return Err(bad("`follow` is a surface binding, not a strip action"));
        }
        if fader && !action.is_level() {
            return Err(bad(format!(
                "a fader takes a level action (gain, output_gain, host_volume, port_gain, crosspoint_gain), not {action:?}"
            )));
        }
        if !fader && !action.is_mute() {
            return Err(bad(format!(
                "a button takes a mute action (mute, output_mute, host_mute, port_mute, crosspoint_mute), not {action:?}"
            )));
        }
        let host = matches!(action, BindingAction::HostVolume | BindingAction::HostMute);
        let port = matches!(action, BindingAction::PortGain | BindingAction::PortMute);
        let xp = matches!(
            action,
            BindingAction::CrosspointGain | BindingAction::CrosspointMute
        );
        let channels = action.uses_channels();
        if c.target().is_some() != channels {
            return Err(bad(if channels {
                "a channel action needs `target`"
            } else {
                "only channel actions take `target`"
            }));
        }
        if c.ports().is_empty() == port {
            return Err(bad(if port {
                "a port action needs `ports`"
            } else {
                "only port actions take `ports`"
            }));
        }
        if c.crosspoints().is_empty() == xp {
            return Err(bad(if xp {
                "a crosspoint action needs `crosspoints`"
            } else {
                "only crosspoint actions take `crosspoints`"
            }));
        }
        if let Some(FaderRange {
            min_mdb, max_mdb, ..
        }) = range
        {
            // A host strip's range must fit `[host_volume]`'s, checked only
            // for a strip that is new or changed: narrowing `[host_volume]`
            // must not block editing the other strips of a saved panel.
            let (lo, hi) = match (host && !unchanged, self.config.config.host_volume.as_ref()) {
                (true, Some(h)) => (h.min_mdb(), h.max_mdb()),
                _ => (GAIN_FLOOR_MDB, GAIN_CEILING_MDB),
            };
            if !(lo <= min_mdb && min_mdb < max_mdb && max_mdb <= hi) {
                return Err(bad(format!(
                    "the range must satisfy {lo} <= min_mdb < max_mdb <= {hi}"
                )));
            }
        }
        // Below: what the strip refers to. A strip saved unchanged is not
        // checked again, so a host strip whose `[host_volume]` was removed
        // (or a target that went away) does not block editing the panel.
        if unchanged {
            return Ok(());
        }
        if host && self.config.config.host_volume.is_none() {
            return Err(bad(
                "a host volume strip needs `[host_volume]` in the configuration",
            ));
        }
        if let Some(t) = c.target() {
            if matches!(
                action,
                BindingAction::OutputGain | BindingAction::OutputMute
            ) {
                self.check_output_set(t)
            } else {
                self.check_channel_set(t)
            }
            .map_err(|e| match e {
                CommandError::Invalid { reason, .. } => bad(reason),
                other => other,
            })?;
        }
        if port {
            let bridges = self.bridges_of(intent);
            for p in c.ports() {
                if let Some(m) = self.port_problem(p, &bridges) {
                    return Err(bad(format!("{}: {m}", crate::router::describe(p))));
                }
            }
        }
        if xp {
            let grid = self.grid_of(intent);
            for k in c.crosspoints() {
                match grid.iter().find(|e| e.x.key() == *k) {
                    None => {
                        return Err(bad(format!(
                            "{} is not patched",
                            crate::router::describe_key(k)
                        )));
                    }
                    Some(e) if matches!(e.origin, CrosspointOrigin::Bridge { .. }) => {
                        return Err(bad(format!(
                            "{} is patched by a card bridge: use its ports' levels",
                            crate::router::describe_key(k)
                        )));
                    }
                    Some(_) => {}
                }
            }
        }
        Ok(())
    }

    /// `mutate` for the mixer commands; `None` for any other command.
    pub(super) fn mutate_mixer_intent(
        &self,
        intent: &mut StateFile,
        cmd: &Command,
    ) -> Option<Result<bool, CommandError>> {
        Some(match cmd {
            Command::SetMixer { mixer } => self.check_mixer(mixer, intent).map(|()| {
                match intent.mixer.iter_mut().find(|m| m.id == mixer.id) {
                    Some(m) if m == mixer => false,
                    Some(m) => {
                        m.clone_from(mixer);
                        true
                    }
                    None => {
                        intent.mixer.push(mixer.clone());
                        intent.mixer.sort_by(|a, b| a.id.cmp(&b.id));
                        true
                    }
                }
            }),
            Command::RemoveMixer { id } => {
                let before = intent.mixer.len();
                intent.mixer.retain(|m| m.id != *id);
                if intent.mixer.len() == before {
                    Err(not_found(format!("mixer panel `{id}`")))
                } else {
                    Ok(true)
                }
            }
            _ => return None,
        })
    }

    /// Invariants of the saved panels: ids unique and in order.
    pub(super) fn check_mixer_invariants(&self) -> Result<(), String> {
        if self.intent.mixer.windows(2).any(|w| w[0].id >= w[1].id) {
            return Err("mixer panels not in strict id order".into());
        }
        Ok(())
    }
}

/// `MixerChanged` for every panel added, replaced or removed.
pub(super) fn mixer_events(before: &StateFile, after: &StateFile, origin: &Origin) -> Vec<Event> {
    let ids: BTreeSet<&String> = before
        .mixer
        .iter()
        .chain(&after.mixer)
        .map(|m| &m.id)
        .collect();
    ids.into_iter()
        .filter(|id| {
            let of = |s: &StateFile| s.mixer.iter().find(|m| m.id == **id).cloned();
            of(before) != of(after)
        })
        .map(|id| Event::MixerChanged {
            id: id.clone(),
            origin: origin.clone(),
        })
        .collect()
}
