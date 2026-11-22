//! Levels and mutes beyond the channel controls: router ports
//! (`SetPortGain`, `SetPortMute`), router crosspoints
//! (`SetCrosspointGain`, `SetCrosspointMute`) and the host (speaker
//! amplifier) volume the daemon owns (`SetHostVolume`, `SetHostMute`).
//!
//! One handler per command; the Router page, the mixer strips and the
//! Speaker page all post here. Posted in place (`X-Tessera-Live`) they
//! answer JSON; otherwise a redirect back to the page that posted
//! ([`return_to`]) with a one-shot message. A refusal of the daemon (a
//! level a surface fader owns, a daemon without the runtime) is shown as
//! the daemon words it, explained.

use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use tessera_control_types::{Command, CrosspointKey, Gain, RequestBody, RouterPort};

use super::{answer, gain_of_slider, need_post, refusal, slider_text, slider_value};
use crate::{Shared, call, ports};

/// Where a level form may send the browser back without the script: the
/// Mixer page, the Router page (and its editors) or the Speaker page's
/// volume. Anything else is not ours to follow.
#[must_use]
pub fn return_to(back: &str) -> Option<String> {
    if let Some(m) = crate::mixer::back_to(back) {
        return Some(m);
    }
    if back == "/speaker#volume" {
        return Some(back.to_owned());
    }
    super::router::back_to(back)
}

fn field<'a>(form: &'a [(String, String)], name: &str) -> &'a str {
    form.iter()
        .find(|(k, _)| k == name)
        .map_or("", |(_, v)| v.as_str())
}

fn all<'a>(form: &'a [(String, String)], name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    form.iter()
        .filter(move |(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// The ports a form names (`port`, repeated); `None` if one is not a port.
fn ports_of(form: &[(String, String)]) -> Option<Vec<RouterPort>> {
    let v: Option<Vec<RouterPort>> = all(form, "port").map(ports::decode).collect();
    v.filter(|v| !v.is_empty())
}

/// The crosspoints a form names (`xp`, repeated).
fn crosspoints_of(form: &[(String, String)]) -> Option<Vec<CrosspointKey>> {
    let v: Option<Vec<CrosspointKey>> = all(form, "xp").map(ports::decode_xp).collect();
    v.filter(|v| !v.is_empty())
}

/// Send one command for a form; the message on success.
async fn run(
    st: &Shared,
    session: &str,
    cmd: Command,
    ok: String,
) -> Result<String, (StatusCode, String)> {
    let kind = cmd.kind();
    let r = call(st, RequestBody::Command(cmd)).await;
    tracing::info!(session = %session, command = kind, ok = r.is_ok(), "page command");
    r.map(|_| ok).map_err(|e| refusal(&e))
}

fn bad(m: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, m.to_owned())
}

fn gain_words(n: usize, what: &str) -> String {
    if n == 1 {
        what.to_owned()
    } else {
        format!("{n} {what}s")
    }
}

/// `POST /levels/port/gain`: `port` (repeated), `db` (the bottom of the
/// slider is off) → `SetPortGain`.
pub async fn port_gain(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, field(&form, "csrf")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let to = return_to(field(&form, "back")).unwrap_or_else(|| "/router".into());
    let gain = gain_of_slider(field(&form, "db"));
    let r = match (ports_of(&form), gain) {
        (None, _) => Err(bad("Name the router port(s).")),
        (_, None) => Err(bad(
            "A level is from \u{2212}96 to +12 dB, or the bottom of the slider for off.",
        )),
        (Some(p), Some(gain)) => {
            let what = match p.as_slice() {
                [one] => ports::place(one),
                more => gain_words(more.len(), "port"),
            };
            run(
                &st,
                &s.id,
                Command::SetPortGain { ports: p, gain },
                format!("{what}: level {}.", slider_text(gain)),
            )
            .await
        }
    };
    let db = gain.map_or(serde_json::Value::Null, |g| slider_value(g).into());
    answer(
        &st,
        &token,
        &headers,
        &to,
        r,
        serde_json::json!({ "db": db }),
    )
}

/// `POST /levels/port/mute`: `port` (repeated), `muted` → `SetPortMute`.
pub async fn port_mute(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, field(&form, "csrf")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let to = return_to(field(&form, "back")).unwrap_or_else(|| "/router".into());
    let muted = field(&form, "muted") == "true";
    let r = match ports_of(&form) {
        None => Err(bad("Name the router port(s).")),
        Some(p) => {
            let what = match p.as_slice() {
                [one] => ports::place(one),
                more => gain_words(more.len(), "port"),
            };
            run(
                &st,
                &s.id,
                Command::SetPortMute { ports: p, muted },
                format!("{what}: {}.", if muted { "muted" } else { "unmuted" }),
            )
            .await
        }
    };
    answer(&st, &token, &headers, &to, r, serde_json::json!({}))
}

/// `POST /levels/crosspoint/gain`: `xp` (repeated), `db` →
/// `SetCrosspointGain`.
pub async fn crosspoint_gain(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, field(&form, "csrf")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let to = return_to(field(&form, "back")).unwrap_or_else(|| "/router".into());
    let gain = gain_of_slider(field(&form, "db"));
    let r = match (crosspoints_of(&form), gain) {
        (None, _) => Err(bad("Name the crosspoint(s).")),
        (_, None) => Err(bad(
            "A gain is from \u{2212}96 to +12 dB, or the bottom of the slider for off.",
        )),
        (Some(x), Some(gain)) => {
            let what = match x.as_slice() {
                [one] => ports::xp_words(None, one),
                more => gain_words(more.len(), "crosspoint"),
            };
            run(
                &st,
                &s.id,
                Command::SetCrosspointGain {
                    crosspoints: x,
                    gain,
                },
                format!("{what}: gain {}.", slider_text(gain)),
            )
            .await
        }
    };
    let db = gain.map_or(serde_json::Value::Null, |g| slider_value(g).into());
    answer(
        &st,
        &token,
        &headers,
        &to,
        r,
        serde_json::json!({ "db": db }),
    )
}

/// `POST /levels/crosspoint/mute`: `xp` (repeated), `muted` →
/// `SetCrosspointMute`.
pub async fn crosspoint_mute(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, field(&form, "csrf")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let to = return_to(field(&form, "back")).unwrap_or_else(|| "/router".into());
    let muted = field(&form, "muted") == "true";
    let r = match crosspoints_of(&form) {
        None => Err(bad("Name the crosspoint(s).")),
        Some(x) => {
            let what = match x.as_slice() {
                [one] => ports::xp_words(None, one),
                more => gain_words(more.len(), "crosspoint"),
            };
            run(
                &st,
                &s.id,
                Command::SetCrosspointMute {
                    crosspoints: x,
                    muted,
                },
                format!("{what}: {}.", if muted { "muted" } else { "unmuted" }),
            )
            .await
        }
    };
    answer(&st, &token, &headers, &to, r, serde_json::json!({}))
}

/// The host volume a form asks for, milli-dB snapped to 0.5 dB.
fn host_gain(db: &str) -> Option<Gain> {
    let v: f64 = db.trim().replace('\u{2212}', "-").parse().ok()?;
    if !v.is_finite() {
        return None;
    }
    let snapped = (v * 2.0).round() / 2.0;
    #[allow(clippy::cast_possible_truncation)]
    let g = Gain::MilliDb((snapped * 1000.0).clamp(-1e9, 1e9) as i32);
    g.is_valid().then_some(g)
}

/// `POST /host/volume`: `db` → `SetHostVolume` (the daemon checks the
/// configured range and fader priority). `commit` is accepted and ignored:
/// every value the daemon takes is saved by it.
pub async fn host_volume(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, field(&form, "csrf")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let to = return_to(field(&form, "back")).unwrap_or_else(|| "/mixer".into());
    let gain = host_gain(field(&form, "db"));
    let r = match gain {
        None => Err(bad("The volume is a number of dB.")),
        Some(gain) => {
            run(
                &st,
                &s.id,
                Command::SetHostVolume { gain },
                format!("Volume {}.", slider_text(gain)),
            )
            .await
        }
    };
    let db = gain
        .and_then(Gain::milli_db)
        .map_or(serde_json::Value::Null, |m| (f64::from(m) / 1000.0).into());
    answer(
        &st,
        &token,
        &headers,
        &to,
        r,
        serde_json::json!({ "db": db }),
    )
}

/// `POST /host/mute`: `muted` → `SetHostMute`.
pub async fn host_mute(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, field(&form, "csrf")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let to = return_to(field(&form, "back")).unwrap_or_else(|| "/mixer".into());
    let muted = field(&form, "muted") == "true";
    let r = run(
        &st,
        &s.id,
        Command::SetHostMute { muted },
        if muted {
            "The speaker output is muted.".into()
        } else {
            "The speaker output is unmuted.".into()
        },
    )
    .await;
    answer(&st, &token, &headers, &to, r, serde_json::json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_volumes_snap_to_half_decibels() {
        assert_eq!(host_gain("-12.26"), Some(Gain::MilliDb(-12_500)));
        assert_eq!(host_gain("\u{2212}6"), Some(Gain::MilliDb(-6_000)));
        assert_eq!(host_gain("x"), None);
        assert_eq!(host_gain("NaN"), None);
        assert_eq!(host_gain("-200"), None);
    }

    #[test]
    fn only_the_portal_pages_are_places_to_return_to() {
        assert_eq!(
            return_to("/speaker#volume").as_deref(),
            Some("/speaker#volume")
        );
        assert_eq!(return_to("/mixer").as_deref(), Some("/mixer"));
        assert_eq!(return_to("/router#p-x").as_deref(), Some("/router#p-x"));
        assert_eq!(return_to("//evil"), None);
        assert_eq!(return_to("/speaker"), None);
    }
}
