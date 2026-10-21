//! The Volume card: the amplifier's hardware mixer control, read and set
//! with `amixer` through the [`Runner`]. Not the EQ engine's own volume.
//! Shown only with an `[appliance.volume]` table.

use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use maud::{Markup, html};
use serde::Deserialize;

use crate::Shared;
use crate::appliance::camilla::{fill_class, parse_form_db, signed_db, snap_db};
use crate::appliance::services::{Runner, sanitize};
use crate::config::VolumeConfig;
use crate::pages::need_post;

/// Absolute path of `amixer`.
pub const AMIXER: &str = "/usr/bin/amixer";
/// Absolute path of `alsactl`.
pub const ALSACTL: &str = "/usr/sbin/alsactl";
/// Where the volume form sends the operator back.
pub const VOLUME_ANCHOR: &str = "/speaker#volume";

/// `amixer -c <card> sget <control>`.
#[must_use]
pub fn get_args(cfg: &VolumeConfig) -> Vec<String> {
    vec![
        "-c".into(),
        cfg.card.clone(),
        "sget".into(),
        cfg.control.clone(),
    ]
}

/// `amixer -c <card> -- sset <control> <db>dB` (`--` so a negative value
/// is not taken for an option).
#[must_use]
pub fn set_args(cfg: &VolumeConfig, db: f64) -> Vec<String> {
    vec![
        "-c".into(),
        cfg.card.clone(),
        "--".into(),
        "sset".into(),
        cfg.control.clone(),
        format!("{db:.1}dB"),
    ]
}

/// `alsactl store`: save the mixer state so it survives an unclean
/// shutdown (`alsa-restore.service` saves it at a clean one anyway).
#[must_use]
pub fn store_args() -> Vec<String> {
    vec!["store".into()]
}

/// The first `[... dB]` of `amixer sget` output (the first channel's
/// value, e.g. `[-12.00dB]`).
#[must_use]
pub fn parse_db(stdout: &str) -> Option<f64> {
    for (i, _) in stdout.match_indices("dB]") {
        let before = &stdout[..i];
        let Some(open) = before.rfind('[') else {
            continue;
        };
        if let Ok(v) = before[open + 1..].trim().parse::<f64>() {
            if v.is_finite() {
                return Some(v);
            }
        }
    }
    None
}

/// Read the control's current value, dB.
///
/// # Errors
/// What went wrong, for the card.
pub async fn read(r: &dyn Runner, cfg: &VolumeConfig) -> Result<f64, String> {
    match r.run(AMIXER, &get_args(cfg)).await {
        Ok(o) if o.success() => parse_db(&o.stdout).ok_or_else(|| {
            format!(
                "`{}` on card `{}` reports no dB value",
                cfg.control, cfg.card
            )
        }),
        Ok(o) => Err(failure("amixer sget", &o.stderr, o.code)),
        Err(e) => Err(format!("amixer: {e}")),
    }
}

/// Set the control to `db` (already snapped and clamped), then try to
/// save the mixer state; a failed save is logged at debug only.
///
/// # Errors
/// `amixer sset` failed.
pub async fn set(r: &dyn Runner, cfg: &VolumeConfig, db: f64) -> Result<(), String> {
    match r.run(AMIXER, &set_args(cfg, db)).await {
        Ok(o) if o.success() => {}
        Ok(o) => return Err(failure("amixer sset", &o.stderr, o.code)),
        Err(e) => return Err(format!("amixer: {e}")),
    }
    match r.run(ALSACTL, &store_args()).await {
        Ok(o) if o.success() => {}
        Ok(o) => tracing::debug!(
            stderr = %sanitize(&o.stderr),
            "alsactl store failed; the mixer state is saved at a clean shutdown"
        ),
        Err(e) => {
            tracing::debug!(%e, "alsactl store did not run; the mixer state is saved at a clean shutdown")
        }
    }
    Ok(())
}

fn failure(what: &str, stderr: &str, code: Option<i32>) -> String {
    let why = sanitize(stderr);
    if why.is_empty() {
        format!(
            "{what} failed (exit code {})",
            code.map_or("signal".to_owned(), |c| c.to_string())
        )
    } else {
        format!("{what}: {why}")
    }
}

/// The Volume card.
#[must_use]
pub fn card(cfg: &VolumeConfig, current: &Result<f64, String>, csrf: &str) -> Markup {
    let (min, max) = (cfg.min_db, cfg.max_db);
    let mid = (min + max) / 2.0;
    html! {
        section.card.c4 id="volume" {
            h2 { "Volume " span.n { "amp output · press Apply" } }
            @match current {
                Ok(db) => {
                    form.vol method="post" action="/speaker/volume" {
                        input type="hidden" name="csrf" value=(csrf);
                        div {
                            div.big { (signed_db(*db)) small { "dB" } }
                            div.sub { (format!("{} dB is full scale", signed_db(max))) }
                        }
                        div {
                            input type="range" name="db" min=(min) max=(max) step="0.5" value=(format!("{db:.1}"))
                                class=(fill_class(*db, min, max)) aria-label="Volume, dB";
                            div.ticks { span { (signed_db(min)) } span { (signed_db(mid)) } span { (signed_db(max)) } }
                        }
                        button.btn.primary type="submit" { "Apply" }
                    }
                }
                Err(e) => { p.muted { "The volume cannot be read: " (e) } }
            }
        }
    }
}

/// The volume form's fields.
#[derive(Debug, Deserialize)]
pub struct VolumeForm {
    #[serde(default)]
    csrf: String,
    /// The value, dB.
    #[serde(default)]
    db: String,
}

/// `POST /speaker/volume`.
pub async fn post(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<VolumeForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some((a, cfg)) = st
        .appliance
        .as_ref()
        .and_then(|a| a.config.volume.as_ref().map(|v| (a, v)))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(db) = parse_form_db(&f.db) else {
        return (StatusCode::BAD_REQUEST, "db must be a finite number").into_response();
    };
    let db = snap_db(db, cfg.min_db, cfg.max_db);
    let r = set(&*a.runner, cfg, db).await;
    tracing::info!(session = %s.id, db, ok = r.is_ok(), "appliance volume");
    match r {
        Ok(()) => st.set_flash(&token, true, format!("Volume {} dB.", signed_db(db))),
        Err(m) => st.set_flash(&token, false, m),
    }
    Redirect::to(VOLUME_ANCHOR).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appliance::services::{FakeRunner, Output, RunError, argv};

    fn cfg() -> VolumeConfig {
        VolumeConfig {
            card: "DigiAMP".into(),
            control: "Digital".into(),
            min_db: -60.0,
            max_db: 0.0,
        }
    }

    const SGET: &str = "Simple mixer control 'Digital',0\n  Capabilities: pvolume pswitch\n  Playback channels: Front Left - Front Right\n  Limits: Playback 0 - 207\n  Mono:\n  Front Left: Playback 183 [88%] [-12.00dB] [on]\n  Front Right: Playback 183 [88%] [-12.00dB] [on]\n";

    #[test]
    fn amixer_output_is_parsed() {
        assert_eq!(parse_db(SGET), Some(-12.0));
        assert_eq!(
            parse_db("  Mono: Playback 255 [100%] [0.00dB] [on]\n"),
            Some(0.0)
        );
        assert_eq!(parse_db("x [-3.50dB]"), Some(-3.5));
        assert_eq!(parse_db("Limits: Playback 0 - 207\n"), None);
        assert_eq!(parse_db(""), None);
        assert_eq!(parse_db("[xdB]"), None);
    }

    #[test]
    fn argument_vectors_are_exact() {
        let c = cfg();
        assert_eq!(get_args(&c), ["-c", "DigiAMP", "sget", "Digital"]);
        assert_eq!(
            set_args(&c, -12.0),
            ["-c", "DigiAMP", "--", "sset", "Digital", "-12.0dB"]
        );
        assert_eq!(
            set_args(&c, -0.5),
            ["-c", "DigiAMP", "--", "sset", "Digital", "-0.5dB"]
        );
        assert_eq!(set_args(&c, 0.0)[5], "0.0dB");
        assert_eq!(store_args(), ["store"]);
        assert_eq!(AMIXER, "/usr/bin/amixer");
        assert_eq!(ALSACTL, "/usr/sbin/alsactl");
    }

    #[tokio::test]
    async fn read_and_set_go_through_the_runner() {
        let r = FakeRunner::default();
        let c = cfg();
        r.script(
            &[AMIXER, "-c", "DigiAMP", "sget", "Digital"],
            Output::ok(SGET),
        );
        assert_eq!(read(&r, &c).await, Ok(-12.0));
        r.script(
            &[AMIXER, "-c", "DigiAMP", "--", "sset", "Digital", "-20.5dB"],
            Output::ok(SGET),
        );
        // alsactl store unscripted (fails): still Ok.
        assert_eq!(set(&r, &c, -20.5).await, Ok(()));
        assert_eq!(
            r.calls()[1..],
            [
                argv(&[AMIXER, "-c", "DigiAMP", "--", "sset", "Digital", "-20.5dB"]),
                argv(&[ALSACTL, "store"]),
            ]
        );
        let e = set(&r, &c, -1.0).await.unwrap_err();
        assert_eq!(e, "amixer sset: unscripted command");
        let r = FakeRunner::default();
        r.script_error(
            &[AMIXER, "-c", "DigiAMP", "sget", "Digital"],
            RunError::Timeout,
        );
        assert!(read(&r, &c).await.unwrap_err().starts_with("amixer: "));
        r.script(
            &[AMIXER, "-c", "DigiAMP", "sget", "Digital"],
            Output::ok("Simple mixer control 'Digital',0\n"),
        );
        assert!(read(&r, &c).await.unwrap_err().contains("no dB value"));
        r.script(
            &[AMIXER, "-c", "DigiAMP", "sget", "Digital"],
            Output::fail(1, "amixer: Unable to find simple control 'Digital',0\n"),
        );
        assert_eq!(
            read(&r, &c).await.unwrap_err(),
            "amixer sget: amixer: Unable to find simple control 'Digital',0"
        );
    }
}
