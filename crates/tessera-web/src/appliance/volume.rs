//! The Volume pane of the master card: the amplifier's hardware mixer
//! control, read and set with `amixer` through the [`Runner`]. Not the EQ
//! engine's own volume. Shown only with an `[appliance.volume]` table.
//!
//! The page's script posts the slider in place while it is dragged
//! (`commit=0`: `amixer` only, throttled) and once on release (`amixer`
//! and the saved value); without the script the Apply button posts a
//! committing form. Both go through the same handler and checks.
//!
//! The portal owns the setting. A commit writes the value, with the card and
//! control it belongs to, atomically into the portal's state directory, and
//! at start the portal sets it again as soon as the card is there
//! ([`restore`]). Nothing else keeps an amplifier's mixer across a reboot:
//! `alsactl store` needs root, and the distribution's restore service does
//! not run for every card. The card is named, never numbered: `amixer`
//! finds it by its id at each call, so a card that moves to another index
//! after a reboot is still the same card.

use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use maud::{Markup, html};
use serde::{Deserialize, Serialize};

use crate::Shared;
use crate::appliance::Appliance;
use crate::appliance::camilla::{fill_class, parse_form_db, signed_db, snap_db};
use crate::appliance::files::{read_regular, write_atomic};
use crate::appliance::services::{Runner, sanitize};
use crate::config::VolumeConfig;
use crate::pages::{is_live, live_answer, need_post};

/// Absolute path of `amixer`.
pub const AMIXER: &str = "/usr/bin/amixer";
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

/// `amixer -c <card> -- sset <control> unmute`: the control's playback switch on.
#[must_use]
pub fn unmute_args(cfg: &VolumeConfig) -> Vec<String> {
    vec![
        "-c".into(),
        cfg.card.clone(),
        "--".into(),
        "sset".into(),
        cfg.control.clone(),
        "unmute".into(),
    ]
}

/// Whether a playback channel of the control has its switch off: a `[off]` on a
/// `Playback` line (`Front Left: Playback 183 [88%] [-12.00dB] [off]`). A control without a
/// switch prints neither `[on]` nor `[off]` and is never muted.
#[must_use]
pub fn parse_muted(stdout: &str) -> bool {
    stdout
        .lines()
        .any(|line| line.contains("Playback") && line.contains("[off]"))
}

/// What the control says: its level, and whether its hardware switch is off. A card
/// can come up muted (an `alsa-restore` that never ran), and the dB value alone does not
/// unmute it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    /// The first channel's value, dB.
    pub db: f64,
    /// The playback switch is off: nothing is heard at any value.
    pub muted: bool,
}

/// Read the control's current value, dB, and its switch.
///
/// # Errors
/// What went wrong, for the card.
pub async fn read(r: &dyn Runner, cfg: &VolumeConfig) -> Result<Level, String> {
    match r.run(AMIXER, &get_args(cfg)).await {
        Ok(o) if o.success() => parse_db(&o.stdout)
            .map(|db| Level {
                db,
                muted: parse_muted(&o.stdout),
            })
            .ok_or_else(|| {
                format!(
                    "`{}` on card `{}` reports no dB value",
                    cfg.control, cfg.card
                )
            }),
        Ok(o) => Err(failure("amixer sget", &o.stderr, o.code, cfg)),
        Err(e) => Err(format!("amixer: {e}")),
    }
}

/// Set the control to `db` (already snapped and clamped). With `commit`,
/// then save the value for the next start; a preview (the slider still
/// moving) is not saved, the release's commit does it once. Answers whether the
/// card's switch is off, so the value is never set silently on a muted card.
///
/// # Errors
/// `amixer sset` failed, or the value is set but could not be saved (the
/// message says which).
pub async fn set(
    r: &dyn Runner,
    cfg: &VolumeConfig,
    db: f64,
    commit: bool,
) -> Result<bool, String> {
    let muted = amixer_set(r, cfg, db).await?;
    if !commit {
        return Ok(muted);
    }
    let saved = cfg.clone();
    tokio::task::spawn_blocking(move || save(&saved, db))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| {
            format!(
                "The volume is {} dB now, but it could not be saved for the next start: {e}. The portal's unit needs its state directory (StateDirectory=tessera-web, docs/web-portal.md, Volume).",
                signed_db(db)
            )
        })?;
    Ok(muted)
}

/// `amixer sset`; `true` when the control it prints back has its switch off.
async fn amixer_set(r: &dyn Runner, cfg: &VolumeConfig, db: f64) -> Result<bool, String> {
    match r.run(AMIXER, &set_args(cfg, db)).await {
        Ok(o) if o.success() => Ok(parse_muted(&o.stdout)),
        Ok(o) => Err(failure("amixer sset", &o.stderr, o.code, cfg)),
        Err(e) => Err(format!("amixer: {e}")),
    }
}

/// Switch the control's playback switch on.
///
/// # Errors
/// `amixer sset` failed.
pub async fn unmute(r: &dyn Runner, cfg: &VolumeConfig) -> Result<(), String> {
    match r.run(AMIXER, &unmute_args(cfg)).await {
        Ok(o) if o.success() => Ok(()),
        Ok(o) => Err(failure("amixer sset", &o.stderr, o.code, cfg)),
        Err(e) => Err(format!("amixer: {e}")),
    }
}

/// What the page says after a value was set: the value, and when the card's switch is
/// off, that nothing is heard yet and what to do.
fn set_text(db: f64, muted: bool) -> String {
    if muted {
        format!(
            "Volume {} dB, but the output is muted: switch it on with Unmute.",
            signed_db(db)
        )
    } else {
        format!("Volume {} dB.", signed_db(db))
    }
}

/// What the state file holds: the committed value and the control it was
/// set on, so a value is never applied to another card or control.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Saved {
    card: String,
    control: String,
    db: f64,
}

/// Save the committed `db` for the next start: written next to the state
/// file, synced and renamed over it.
///
/// # Errors
/// The I/O error, with the file it concerns.
pub fn save(cfg: &VolumeConfig, db: f64) -> Result<(), String> {
    let saved = Saved {
        card: cfg.card.clone(),
        control: cfg.control.clone(),
        db,
    };
    let json = serde_json::to_vec(&saved).map_err(|e| e.to_string())?;
    write_atomic(&cfg.state_file, &json)
}

/// The saved value, if the state file holds one for this card and control.
/// No file, or one saved for another card or control: `None`.
///
/// # Errors
/// The file exists but cannot be read or understood.
pub fn load(cfg: &VolumeConfig) -> Result<Option<f64>, String> {
    let bytes = match read_regular(&cfg.state_file) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", cfg.state_file.display())),
    };
    let saved: Saved =
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", cfg.state_file.display()))?;
    Ok(
        (saved.card == cfg.card && saved.control == cfg.control && saved.db.is_finite())
            .then_some(saved.db),
    )
}

/// How long the restore at start waits for the card, which may enumerate
/// after the portal starts (USB audio, a slow HAT).
#[derive(Debug, Clone, Copy)]
pub struct Wait {
    /// Between two looks at the card.
    pub every: Duration,
    /// How many looks, the first at once.
    pub tries: u32,
}

/// Every 5 seconds for 10 minutes.
pub const START_WAIT: Wait = Wait {
    every: Duration::from_secs(5),
    tries: 120,
};

/// How [`restore`] ended.
#[derive(Debug, PartialEq)]
pub enum Restored {
    /// Nothing is saved for this card and control.
    Nothing,
    /// The saved value was set (dB).
    Set(f64),
    /// The operator moved the slider first, which wins.
    Touched,
    /// The card or control never answered; the last reason.
    GaveUp(String),
    /// The state file or `amixer sset` failed.
    Failed(String),
}

/// Set the saved volume again, once the card answers. `touched` is set by
/// the first volume request of the operator: from then on the restore
/// stands down, so it never undoes a value chosen after the start.
pub async fn restore(
    r: &dyn Runner,
    cfg: &VolumeConfig,
    touched: &AtomicBool,
    wait: Wait,
) -> Restored {
    let loaded = {
        let cfg = cfg.clone();
        tokio::task::spawn_blocking(move || load(&cfg))
            .await
            .map_err(|e| e.to_string())
            .and_then(|loaded| loaded)
    };
    let db = match loaded {
        Ok(Some(db)) => snap_db(db, cfg.min_db, cfg.max_db),
        Ok(None) => return Restored::Nothing,
        Err(e) => {
            tracing::warn!(%e, "the saved appliance volume cannot be read");
            return Restored::Failed(e);
        }
    };
    let tries = wait.tries.max(1);
    let mut last = String::new();
    for attempt in 0..tries {
        if touched.load(Ordering::SeqCst) {
            return Restored::Touched;
        }
        match read(r, cfg).await {
            Ok(_) => {
                // The card answers; a value chosen while it was asked wins.
                if touched.load(Ordering::SeqCst) {
                    return Restored::Touched;
                }
                return match amixer_set(r, cfg, db).await {
                    Ok(muted) => {
                        tracing::info!(card = %cfg.card, control = %cfg.control, db, "appliance volume restored");
                        if muted {
                            tracing::warn!(card = %cfg.card, control = %cfg.control, "the card's playback switch is off: nothing is heard until it is switched on (Unmute on the Speaker page)");
                        }
                        Restored::Set(db)
                    }
                    Err(e) => {
                        tracing::warn!(%e, "the saved appliance volume cannot be set");
                        Restored::Failed(e)
                    }
                };
            }
            Err(e) => {
                if attempt == 0 {
                    tracing::info!(%e, "waiting for the sound card to set the saved volume");
                }
                last = e;
            }
        }
        if attempt + 1 < tries {
            tokio::time::sleep(wait.every).await;
        }
    }
    tracing::warn!(reason = %last, "the sound card did not appear: the saved volume was not set");
    Restored::GaveUp(last)
}

/// Start the restore on a task of its own, so the portal serves meanwhile.
pub fn spawn_restore(a: Arc<Appliance>) {
    let Some(cfg) = a.config.volume.clone() else {
        return;
    };
    tokio::spawn(async move {
        restore(&*a.runner, &cfg, &a.volume_touched, START_WAIT).await;
    });
}

/// Stderr words of `amixer` for a card it cannot find or open. A card the
/// unit may not open is reported the same way (`amixer` looks the name up by
/// opening every control device), so the message names both causes.
fn card_unavailable(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    [
        "invalid card number",
        "cannot find card",
        "no such device",
        "no such file or directory",
        "operation not permitted",
        "permission denied",
    ]
    .iter()
    .any(|words| stderr.contains(words))
}

fn failure(what: &str, stderr: &str, code: Option<i32>, cfg: &VolumeConfig) -> String {
    if card_unavailable(stderr) {
        return format!(
            "The sound card `{}` was not found. Check that it is connected (`aplay -l` lists the cards) and that `[appliance.volume] card` in /etc/tessera/web.toml names it; the portal's unit must also be allowed to open the card's control device (DeviceAllow=/dev/snd/controlC<n>, docs/web-portal.md, Volume).",
            cfg.card
        );
    }
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

/// A level at or below this is the mixer's mute step, not a gain. An amplifier whose lowest
/// step silences it reports ALSA's mute value (about −100000 dB), which is no number to show.
pub const MUTE_STEP_DB: f64 = -1000.0;

/// Is `db` the card's lowest step, the one that silences the amplifier?
#[must_use]
pub fn is_mute_step(db: f64) -> bool {
    db <= MUTE_STEP_DB
}

/// The Volume pane of the master card (`#volume`, where the form's
/// redirect lands). The script finds the form by `data-live-slider`; the
/// readout, the fill class and `aria-valuetext` are the server's values
/// until it moves them. A card whose playback switch is off says so under
/// the slider and offers the switch.
#[must_use]
pub fn pane(cfg: &VolumeConfig, current: &Result<Level, String>, csrf: &str) -> Markup {
    let (min, max) = (cfg.min_db, cfg.max_db);
    let mid = (min + max) / 2.0;
    html! {
        div.pane id="volume" {
            h2 { "Volume " span.n { "amp output" } }
            @match current {
                Ok(Level { db, muted }) => {
                    // The lowest step is shown as the slider's floor, not as its gain.
                    @let floor = is_mute_step(*db);
                    @let shown = if floor { min } else { *db };
                    form.vol.live method="post" action="/speaker/volume" data-live-slider="volume" {
                        input type="hidden" name="csrf" value=(csrf);
                        div.readout {
                            output.big for="volume-db" { @if floor { "\u{2212}\u{221e}" } @else { (signed_db(*db)) } small { "dB" } }
                            div.sub {
                                (format!("{} dB is full scale", signed_db(max)))
                                @if floor { " \u{b7} the card is at its lowest step (its own mute)" }
                            }
                        }
                        div.slider {
                            input id="volume-db" type="range" name="db" min=(min) max=(max) step="0.5" value=(format!("{shown:.1}"))
                                class=(fill_class(shown, min, max)) aria-label="Volume, dB"
                                aria-valuetext=(if floor { "lowest step".to_owned() } else { format!("{} dB", signed_db(*db)) });
                            div.ticks { span { (signed_db(min)) } span { (signed_db(mid)) } span { (signed_db(max)) } }
                        }
                        div.apply {
                            // Without the script the button commits the form;
                            // with it the slider applies itself and the note
                            // reports (the script owns `data-live-local` nodes).
                            button.btn.primary.nojs type="submit" { "Apply" }
                            span.note role="status" aria-live="polite" data-live-local {}
                        }
                    }
                    @if *muted {
                        p.flash.bad role="alert" { "The output is muted: the card's playback switch is off, so nothing is heard at any volume." }
                        form method="post" action="/speaker/volume/unmute" {
                            input type="hidden" name="csrf" value=(csrf);
                            button.btn.primary type="submit" { "Unmute" }
                        }
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
    /// `0` while the slider is still moving (set the control only); absent
    /// or `1` on release and from the plain form (set and store).
    #[serde(default)]
    commit: String,
}

/// The form's `commit`: `true` (the default, the plain form has no field),
/// `false` for a preview, or nothing for anything else.
#[must_use]
pub fn parse_commit(s: &str) -> Option<bool> {
    match s.trim() {
        "" | "1" | "yes" => Some(true),
        "0" | "no" => Some(false),
        _ => None,
    }
}

/// `POST /speaker/volume`. Posted in place (`X-Tessera-Live`) it answers
/// JSON with the value as applied (`db`); otherwise a redirect to the pane
/// with a one-shot message.
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
    let Some(commit) = parse_commit(&f.commit) else {
        return (StatusCode::BAD_REQUEST, "commit must be 0 or 1").into_response();
    };
    let db = snap_db(db, cfg.min_db, cfg.max_db);
    // From the first request on, the operator's value outranks the saved one
    // that is still waiting for its card at start.
    a.volume_touched.store(true, Ordering::SeqCst);
    let r = set(&*a.runner, cfg, db, commit).await;
    // Previews come every 150 ms while a slider moves: debug only.
    if commit {
        tracing::info!(session = %s.id, db, ok = r.is_ok(), "appliance volume");
    } else {
        tracing::debug!(session = %s.id, db, ok = r.is_ok(), "appliance volume preview");
    }
    let text = match &r {
        Ok(muted) => set_text(db, *muted),
        Err(m) => m.clone(),
    };
    if is_live(&headers) {
        // The value is applied either way (`ok`); `muted` tells the page the card's
        // switch is off, which the refresh after a commit then shows with its button.
        return live_answer(
            StatusCode::OK,
            r.is_ok(),
            &text,
            serde_json::json!({ "db": db, "commit": commit, "muted": r.as_ref().is_ok_and(|m| *m) }),
        );
    }
    st.set_flash(&token, r.is_ok(), text);
    Redirect::to(VOLUME_ANCHOR).into_response()
}

/// The unmute form's field.
#[derive(Debug, Deserialize)]
pub struct UnmuteForm {
    #[serde(default)]
    csrf: String,
}

/// `POST /speaker/volume/unmute`: switch the card's playback switch on, then back to
/// the pane with a one-shot message. The saved volume is not touched.
pub async fn post_unmute(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<UnmuteForm>,
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
    let r = unmute(&*a.runner, cfg).await;
    tracing::info!(session = %s.id, ok = r.is_ok(), "appliance volume unmute");
    let text = match &r {
        Ok(()) => "The output is unmuted.".to_owned(),
        Err(m) => m.clone(),
    };
    st.set_flash(&token, r.is_ok(), text);
    Redirect::to(VOLUME_ANCHOR).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU32;

    use crate::appliance::services::{BoxFuture, FakeRunner, Output, RunError, argv};

    /// A fresh directory for a state file, removed again at the end.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("tessera-web-volume-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn cfg(&self) -> VolumeConfig {
            VolumeConfig {
                state_file: self.0.join("volume.json"),
                ..cfg()
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The table with a state file that cannot exist.
    fn cfg() -> VolumeConfig {
        VolumeConfig {
            card: "DigiAMP".into(),
            control: "Digital".into(),
            min_db: -60.0,
            max_db: 0.0,
            state_file: "/nonexistent/tessera-web/volume.json".into(),
        }
    }

    const SGET: &str = "Simple mixer control 'Digital',0\n  Capabilities: pvolume pswitch\n  Playback channels: Front Left - Front Right\n  Limits: Playback 0 - 207\n  Mono:\n  Front Left: Playback 183 [88%] [-12.00dB] [on]\n  Front Right: Playback 183 [88%] [-12.00dB] [on]\n";
    const NO_CARD: &str = "amixer: Invalid card number 'DigiAMP'.\n";

    fn sget() -> [&'static str; 5] {
        [AMIXER, "-c", "DigiAMP", "sget", "Digital"]
    }

    fn sset(db: &str) -> [&str; 7] {
        [AMIXER, "-c", "DigiAMP", "--", "sset", "Digital", db]
    }

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
    fn the_cards_mute_step_is_shown_as_the_minimum() {
        // ALSA's mute value as amixer prints it.
        assert_eq!(
            parse_db("  Front Left: Playback 0 [0%] [-99999.99dB] [on]\n"),
            Some(-99999.99)
        );
        assert!(is_mute_step(-99999.99) && is_mute_step(-100000.0) && is_mute_step(MUTE_STEP_DB));
        assert!(!is_mute_step(-74.0) && !is_mute_step(-60.0) && !is_mute_step(0.0));
        let c = cfg();
        let level = |db| Ok(Level { db, muted: false });
        let html = pane(&c, &level(-99999.99), "token").into_string();
        assert!(
            !html.contains("99999") && !html.contains("100000"),
            "{html}"
        );
        assert!(html.contains("\u{2212}\u{221e}"), "{html}");
        assert!(html.contains("value=\"-60.0\""), "{html}");
        assert!(html.contains("lowest step"), "{html}");
        // An ordinary level is shown as it is.
        let html = pane(&c, &level(-12.0), "token").into_string();
        assert!(html.contains("\u{2212}12.0"), "{html}");
        assert!(html.contains("value=\"-12.0\""), "{html}");
        assert!(
            !html.contains("lowest step") && !html.contains('\u{221e}'),
            "{html}"
        );
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
        assert_eq!(AMIXER, "/usr/bin/amixer");
    }

    #[tokio::test]
    async fn read_and_set_go_through_the_runner() {
        let r = FakeRunner::default();
        let dir = Scratch::new("set");
        let c = dir.cfg();
        r.script(&sget(), Output::ok(SGET));
        assert_eq!(
            read(&r, &c).await,
            Ok(Level {
                db: -12.0,
                muted: false
            })
        );
        r.script(&sset("-20.5dB"), Output::ok(SGET));
        // A commit sets the control and saves the value; no `alsactl`.
        assert_eq!(set(&r, &c, -20.5, true).await, Ok(false));
        assert_eq!(r.calls()[1..], [argv(&sset("-20.5dB"))]);
        assert_eq!(load(&c), Ok(Some(-20.5)));
        // A preview sets the control and leaves the saved value alone.
        r.clear_calls();
        r.script(&sset("-30.0dB"), Output::ok(SGET));
        assert_eq!(set(&r, &c, -30.0, false).await, Ok(false));
        assert_eq!(r.calls(), [argv(&sset("-30.0dB"))]);
        assert_eq!(load(&c), Ok(Some(-20.5)));
        let e = set(&r, &c, -1.0, true).await.unwrap_err();
        assert_eq!(e, "amixer sset: unscripted command");
        assert_eq!(load(&c), Ok(Some(-20.5)), "a failed set saves nothing");
        let r = FakeRunner::default();
        r.script_error(&sget(), RunError::Timeout);
        assert!(read(&r, &c).await.unwrap_err().starts_with("amixer: "));
        r.script(&sget(), Output::ok("Simple mixer control 'Digital',0\n"));
        assert!(read(&r, &c).await.unwrap_err().contains("no dB value"));
        r.script(
            &sget(),
            Output::fail(1, "amixer: Unable to find simple control 'Digital',0\n"),
        );
        assert_eq!(
            read(&r, &c).await.unwrap_err(),
            "amixer sget: amixer: Unable to find simple control 'Digital',0"
        );
    }

    #[tokio::test]
    async fn a_missing_card_is_named_with_its_possible_causes() {
        let c = cfg();
        for stderr in [
            NO_CARD,
            "amixer: Control hw:3 open error: No such file or directory\n",
            "amixer: Control hw:3 open error: Operation not permitted\n",
        ] {
            let r = FakeRunner::default();
            r.script(&sget(), Output::fail(1, stderr));
            let e = read(&r, &c).await.unwrap_err();
            assert!(
                e.starts_with("The sound card `DigiAMP` was not found."),
                "{e}"
            );
            assert!(e.contains("aplay -l") && e.contains("web.toml"), "{e}");
            assert!(e.contains("DeviceAllow"), "{e}");
        }
        let r = FakeRunner::default();
        r.script(&sset("-3.0dB"), Output::fail(1, NO_CARD));
        let e = set(&r, &c, -3.0, true).await.unwrap_err();
        assert!(e.contains("`DigiAMP` was not found"), "{e}");
    }

    #[test]
    fn the_saved_value_belongs_to_its_card_and_control() {
        let dir = Scratch::new("saved");
        let c = dir.cfg();
        assert_eq!(load(&c), Ok(None), "no file yet");
        save(&c, -12.5).unwrap();
        assert_eq!(load(&c), Ok(Some(-12.5)));
        // Written whole: no temporary file is left in the directory.
        let names: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["volume.json"]);
        for other in [
            VolumeConfig {
                card: "Other".into(),
                ..c.clone()
            },
            VolumeConfig {
                control: "PCM".into(),
                ..c.clone()
            },
        ] {
            assert_eq!(load(&other), Ok(None));
        }
        save(&c, -3.0).unwrap();
        assert_eq!(load(&c), Ok(Some(-3.0)), "the next commit replaces it");
        for bad in [
            "",
            "{",
            "[]",
            r#"{"card":"DigiAMP","control":"Digital","db":null}"#,
        ] {
            std::fs::write(&c.state_file, bad).unwrap();
            assert!(load(&c).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn a_commit_that_cannot_be_saved_says_so_after_setting_the_volume() {
        let r = FakeRunner::default();
        r.script(&sset("-20.5dB"), Output::ok(SGET));
        let e = set(&r, &cfg(), -20.5, true).await.unwrap_err();
        assert!(
            e.starts_with("The volume is \u{2212}20.5 dB now, but it could not be saved"),
            "{e}"
        );
        assert!(e.contains("StateDirectory=tessera-web"), "{e}");
        assert_eq!(r.calls().len(), 1, "the control was set first");
        // A preview never touches the file, so it cannot fail on it.
        assert_eq!(set(&r, &cfg(), -20.5, false).await, Ok(false));
    }

    const MUTED: &str = "Simple mixer control 'Digital',0\n  Capabilities: pvolume pswitch\n  Playback channels: Front Left - Front Right\n  Limits: Playback 0 - 207\n  Front Left: Playback 12 [6%] [-74.00dB] [off]\n  Front Right: Playback 12 [6%] [-74.00dB] [off]\n";

    #[test]
    fn a_switch_that_is_off_is_read_from_the_playback_lines() {
        assert!(!parse_muted(SGET), "[on]");
        assert!(parse_muted(MUTED));
        // One channel off is muted: that channel is silent.
        assert!(parse_muted(
            "  Front Left: Playback 1 [1%] [-9.00dB] [on]\n  Front Right: Playback 1 [1%] [-9.00dB] [off]\n"
        ));
        assert!(parse_muted("  Mono: Playback 255 [100%] [0.00dB] [off]\n"));
        // A control without a switch prints neither word; nothing else counts.
        assert!(!parse_muted("  Mono: Playback 255 [100%] [0.00dB]\n"));
        assert!(!parse_muted(
            "  Capabilities: pvolume pswitch\n  Limits: Playback 0 - 207\n"
        ));
        assert!(
            !parse_muted("  Front Left: Capture 1 [1%] [off]\n"),
            "capture is not playback"
        );
        assert!(!parse_muted(""));
        assert_eq!(
            unmute_args(&cfg()),
            ["-c", "DigiAMP", "--", "sset", "Digital", "unmute"]
        );
    }

    #[tokio::test]
    async fn the_level_carries_the_switch_and_unmute_runs_the_one_command() {
        let c = cfg();
        let r = FakeRunner::default();
        r.script(&sget(), Output::ok(MUTED));
        assert_eq!(
            read(&r, &c).await,
            Ok(Level {
                db: -74.0,
                muted: true
            })
        );
        // A value set on a muted card is set and reported as muted, and not saved as muted.
        let dir = Scratch::new("muted");
        let saved = dir.cfg();
        r.script(&sset("-24.0dB"), Output::ok(MUTED));
        assert_eq!(set(&r, &saved, -24.0, true).await, Ok(true));
        assert_eq!(load(&saved), Ok(Some(-24.0)));
        assert_eq!(
            set_text(-24.0, true),
            "Volume \u{2212}24.0 dB, but the output is muted: switch it on with Unmute."
        );
        assert_eq!(set_text(-24.0, false), "Volume \u{2212}24.0 dB.");
        // Unmute is one `amixer sset <control> unmute` and leaves the saved value alone.
        r.clear_calls();
        let unmute_call = [AMIXER, "-c", "DigiAMP", "--", "sset", "Digital", "unmute"];
        r.script(&unmute_call, Output::ok(SGET));
        assert_eq!(unmute(&r, &c).await, Ok(()));
        assert_eq!(r.calls(), [argv(&unmute_call)]);
        assert_eq!(load(&saved), Ok(Some(-24.0)));
        r.script(&unmute_call, Output::fail(1, NO_CARD));
        let e = unmute(&r, &c).await.unwrap_err();
        assert!(
            e.starts_with("The sound card `DigiAMP` was not found."),
            "{e}"
        );
    }

    const QUICK: Wait = Wait {
        every: Duration::from_millis(1),
        tries: 4,
    };

    /// A card that answers `sget` only from the `after`+1th look on, and
    /// optionally moves the operator's slider at the second look.
    #[derive(Debug)]
    struct Late {
        after: u32,
        looks: AtomicU32,
        touch_at_second_look: Option<Arc<AtomicBool>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl Late {
        fn new(after: u32, touch: Option<Arc<AtomicBool>>) -> Self {
            Self {
                after,
                looks: AtomicU32::new(0),
                touch_at_second_look: touch,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn sets(&self) -> Vec<Vec<String>> {
            let calls = self.calls.lock().unwrap();
            calls
                .iter()
                .filter(|c| c.iter().any(|a| a == "sset"))
                .cloned()
                .collect()
        }
    }

    impl Runner for Late {
        fn run<'a>(
            &'a self,
            program: &'a str,
            args: &'a [String],
        ) -> BoxFuture<'a, Result<Output, RunError>> {
            Box::pin(async move {
                let mut line = vec![program.to_owned()];
                line.extend(args.iter().cloned());
                self.calls.lock().unwrap().push(line);
                if args.iter().any(|a| a == "sget") {
                    let look = self.looks.fetch_add(1, Ordering::SeqCst) + 1;
                    if look == 2 {
                        if let Some(flag) = &self.touch_at_second_look {
                            flag.store(true, Ordering::SeqCst);
                        }
                    }
                    if look <= self.after {
                        return Ok(Output::fail(1, NO_CARD));
                    }
                    return Ok(Output::ok(SGET));
                }
                Ok(Output::ok(""))
            })
        }
    }

    #[tokio::test]
    async fn start_sets_the_saved_volume_when_the_card_answers() {
        let dir = Scratch::new("restore");
        let c = dir.cfg();
        save(&c, -12.0).unwrap();
        let r = FakeRunner::default();
        r.script(&sget(), Output::ok(SGET));
        r.script(&sset("-12.0dB"), Output::ok(""));
        let touched = AtomicBool::new(false);
        assert_eq!(restore(&r, &c, &touched, QUICK).await, Restored::Set(-12.0));
        assert_eq!(r.calls(), [argv(&sget()), argv(&sset("-12.0dB"))]);
        // It does not rewrite the file: the saved value stays the operator's.
        assert_eq!(load(&c), Ok(Some(-12.0)));
        // A value outside a range changed since is brought into it.
        save(&c, -90.0).unwrap();
        r.script(&sset("-60.0dB"), Output::ok(""));
        assert_eq!(restore(&r, &c, &touched, QUICK).await, Restored::Set(-60.0));
    }

    #[tokio::test]
    async fn start_waits_for_a_card_that_appears_late() {
        let dir = Scratch::new("late");
        let c = dir.cfg();
        save(&c, -12.0).unwrap();
        let r = Late::new(2, None);
        let touched = AtomicBool::new(false);
        assert_eq!(restore(&r, &c, &touched, QUICK).await, Restored::Set(-12.0));
        assert_eq!(r.looks.load(Ordering::SeqCst), 3);
        assert_eq!(r.sets().len(), 1);
        // A card that never shows up ends the wait, with the reason.
        let r = Late::new(u32::MAX, None);
        let Restored::GaveUp(why) = restore(&r, &c, &touched, QUICK).await else {
            panic!("expected GaveUp");
        };
        assert!(why.contains("`DigiAMP` was not found"), "{why}");
        assert_eq!(r.looks.load(Ordering::SeqCst), 4, "one look per try");
        assert!(r.sets().is_empty());
    }

    #[tokio::test]
    async fn start_stands_down_when_the_operator_sets_a_volume_first() {
        let dir = Scratch::new("touched");
        let c = dir.cfg();
        save(&c, -12.0).unwrap();
        // Before the first look.
        let r = Late::new(0, None);
        let touched = AtomicBool::new(true);
        assert_eq!(restore(&r, &c, &touched, QUICK).await, Restored::Touched);
        assert!(r.calls.lock().unwrap().is_empty());
        // While the card is still being waited for.
        let touched = Arc::new(AtomicBool::new(false));
        let r = Late::new(1, Some(touched.clone()));
        assert_eq!(restore(&r, &c, &touched, QUICK).await, Restored::Touched);
        assert!(r.sets().is_empty(), "the operator's value is not undone");
    }

    #[tokio::test]
    async fn start_has_nothing_to_set_without_a_usable_saved_value() {
        let dir = Scratch::new("nothing");
        let c = dir.cfg();
        let r = FakeRunner::default();
        let touched = AtomicBool::new(false);
        assert_eq!(restore(&r, &c, &touched, QUICK).await, Restored::Nothing);
        let other = VolumeConfig {
            card: "Other".into(),
            ..c.clone()
        };
        save(&other, -5.0).unwrap();
        assert_eq!(restore(&r, &c, &touched, QUICK).await, Restored::Nothing);
        std::fs::write(&c.state_file, "not json").unwrap();
        assert!(matches!(
            restore(&r, &c, &touched, QUICK).await,
            Restored::Failed(_)
        ));
        assert!(r.calls().is_empty(), "the card is not asked");
    }

    #[test]
    fn commit_is_the_default_and_strict() {
        assert_eq!(parse_commit(""), Some(true));
        assert_eq!(parse_commit("1"), Some(true));
        assert_eq!(parse_commit(" yes "), Some(true));
        assert_eq!(parse_commit("0"), Some(false));
        assert_eq!(parse_commit("no"), Some(false));
        assert_eq!(parse_commit("true"), None);
        assert_eq!(parse_commit("2"), None);
    }

    #[tokio::test]
    async fn the_handler_checks_the_session_then_the_token_then_the_fields() {
        use crate::appliance::testing::{form_headers, state};
        use crate::config::ApplianceConfig;
        use axum::body::to_bytes;

        let dir = Scratch::new("handler");
        let c = dir.cfg();
        let runner = Arc::new(FakeRunner::default());
        let (st, token, s) = state(
            ApplianceConfig {
                enabled: true,
                volume: Some(c.clone()),
                ..ApplianceConfig::default()
            },
            runner.clone(),
        );
        let touched = || {
            st.appliance
                .as_ref()
                .unwrap()
                .volume_touched
                .load(Ordering::SeqCst)
        };
        let form = |csrf: &str, db: &str, commit: &str| VolumeForm {
            csrf: csrf.into(),
            db: db.into(),
            commit: commit.into(),
        };
        // No session: the login page; a wrong token: 403. Nothing is run.
        let r = post(
            State(st.clone()),
            form_headers(None, true),
            Form(form(&s.csrf, "-3", "0")),
        )
        .await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(r.headers()["location"], "/login");
        let r = post(
            State(st.clone()),
            form_headers(Some(&token), true),
            Form(form("wrong", "-3", "0")),
        )
        .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        // Bad fields: 400, nothing run.
        for (db, commit) in [("loud", ""), ("-3", "maybe"), ("inf", "")] {
            let r = post(
                State(st.clone()),
                form_headers(Some(&token), true),
                Form(form(&s.csrf, db, commit)),
            )
            .await;
            assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{db} {commit}");
        }
        assert!(runner.calls().is_empty());
        assert!(
            !touched(),
            "a refused request does not stand the restore down"
        );
        // A preview in place: snapped and clamped, `amixer` only, JSON back.
        runner.script(&sset("-3.5dB"), Output::ok(SGET));
        let r = post(
            State(st.clone()),
            form_headers(Some(&token), true),
            Form(form(&s.csrf, "-3.26", "0")),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let j: serde_json::Value =
            serde_json::from_slice(&to_bytes(r.into_body(), 1 << 16).await.unwrap()).unwrap();
        assert_eq!(j["ok"], true);
        assert_eq!(j["db"], -3.5);
        assert_eq!(j["commit"], false);
        assert_eq!(j["text"], "Volume \u{2212}3.5 dB.");
        assert_eq!(runner.calls(), [argv(&sset("-3.5dB"))]);
        assert!(st.take_flash(&token).is_none());
        assert!(
            touched(),
            "the first accepted request stands the restore down"
        );
        assert_eq!(load(&c), Ok(None), "a preview is not saved");
        // The commit (the plain form has no `commit` field) saves as well
        // and, without the live header, redirects with a message.
        runner.clear_calls();
        runner.script(&sset("0.0dB"), Output::ok(SGET));
        let r = post(
            State(st.clone()),
            form_headers(Some(&token), false),
            Form(form(&s.csrf, "7", "")),
        )
        .await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(r.headers()["location"], VOLUME_ANCHOR);
        assert_eq!(runner.calls(), [argv(&sset("0.0dB"))]);
        assert_eq!(load(&c), Ok(Some(0.0)));
        let f = st.take_flash(&token).unwrap();
        assert!(f.ok);
        assert_eq!(f.text, "Volume 0.0 dB.");
    }
}
