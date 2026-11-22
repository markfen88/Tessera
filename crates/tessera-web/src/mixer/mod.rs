//! The Mixer page (`/mixer`): mixing desks of channel strips anyone can
//! use. Each strip's fader and buttons post to the existing handlers
//! (channel gain and mute, the RX output stage, router port and crosspoint
//! levels, the host volume), so every change is one of the daemon's
//! commands.
//!
//! Where panels come from is [`panels`]: the panels the daemon saves
//! (`Query::Mixers`), or, while none is saved, the default panel derived
//! from the status ([`model::default_panel`]). The widgets render any
//! [`model::Panel`]; the editor ([`editor`], `/mixer/edit`) builds panels
//! (`docs/plans/2026-11-22-mixer-panel.md`).

pub mod editor;
pub mod model;
pub mod widgets;

use axum::extract::{Query as UrlQuery, State};
use axum::http::HeaderMap;
use axum::response::Response;
use maud::html;
use serde::Deserialize;
use tessera_control_types::{
    MixerSpec, Query, Reply, RequestBody, RouterReport, Status, SurfacesReport,
};

use crate::pages::{error_box, need, page, path_segment, status_of, view};
use crate::{AppState, Shared, call};
use model::{MainTarget, Panel, Sources};

/// The page refreshes this often (seconds): a desk's motorized faders and
/// other operators' changes show within it.
const REFRESH: u32 = 2;

/// What the Main strip controls: the daemon's host volume when the status
/// says the daemon owns it (`Status::host_volume`); else the appliance
/// amplifier's volume set by the portal when `[appliance.volume]` is
/// configured; otherwise the output stage of the RX channels.
pub async fn main_target(st: &AppState, status: Option<&Status>) -> MainTarget {
    if let Some(hv) = status.and_then(|s| s.host_volume.clone()) {
        return MainTarget::HostVolume(hv);
    }
    match st
        .appliance
        .as_ref()
        .and_then(|a| a.config.volume.as_ref().map(|v| (a, v)))
    {
        Some((a, v)) => MainTarget::AmpVolume {
            level: crate::appliance::volume::read(&*a.runner, v).await,
            min: v.min_db,
            max: v.max_db,
        },
        None => MainTarget::OutputStage,
    }
}

/// The panels to show: every saved panel (in the daemon's id order), or
/// the default panel when none is saved.
#[must_use]
pub fn panels(saved: &[MixerSpec], src: &Sources<'_>) -> Vec<Panel> {
    if saved.is_empty() {
        return vec![model::default_panel(src.status, src.surfaces, src.main)];
    }
    saved.iter().map(|m| model::from_spec(m, src)).collect()
}

/// The surfaces, for the strips' links; none when the daemon cannot say.
pub(crate) async fn surfaces(st: &AppState) -> Option<SurfacesReport> {
    match call(st, RequestBody::Query(Query::Surfaces)).await {
        Ok(Reply::Surfaces(r)) => Some(r),
        _ => None,
    }
}

/// The saved panels; none when the daemon has none or cannot say (an
/// older daemon), so the default panel shows.
pub(crate) async fn saved(st: &AppState) -> Vec<MixerSpec> {
    match call(st, RequestBody::Query(Query::Mixers)).await {
        Ok(Reply::Mixers(m)) => m,
        _ => Vec::new(),
    }
}

/// The router, for strips on router ports and crosspoints.
pub(crate) async fn router(st: &AppState) -> Option<RouterReport> {
    crate::ports::router_of(st).await.ok()
}

/// Letters, digits, `-` and `_`, 1 to 64: a panel or strip id.
#[must_use]
pub fn ident_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Where a form posted from the Mixer page returns without the script:
/// `/mixer`, `/mixer?panel=<id>`, either with `#<strip>` (letters, digits,
/// `-`, `_`). Anything else is not ours to follow: `None`, and the handler
/// keeps its own page.
#[must_use]
pub fn back_to(back: &str) -> Option<String> {
    let rest = back.strip_prefix("/mixer")?;
    let rest = match rest.strip_prefix("?panel=") {
        Some(r) => {
            let (id, frag) = r.split_once('#').map_or((r, ""), |(a, b)| (a, b));
            if !ident_ok(id) {
                return None;
            }
            if frag.is_empty() && !r.contains('#') {
                ""
            } else {
                &r[id.len()..]
            }
        }
        None => rest,
    };
    let ok = rest.is_empty()
        || rest.strip_prefix('#').is_some_and(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        });
    ok.then(|| back.to_owned())
}

/// `?panel=<id>`: which saved panel to show.
#[derive(Debug, Deserialize, Default)]
pub struct PanelQuery {
    #[serde(default)]
    panel: String,
}

/// `GET /mixer`.
pub async fn show(
    State(st): State<Shared>,
    headers: HeaderMap,
    UrlQuery(q): UrlQuery<PanelQuery>,
) -> Response {
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let status = status_of(&st).await;
    let main = main_target(&st, status.as_ref().ok()).await;
    let body = match &status {
        Err(e) => error_box(e),
        Ok(stt) => {
            let surfaces = surfaces(&st).await;
            let saved = saved(&st).await;
            let router = router(&st).await;
            let src = Sources {
                status: stt,
                router: router.as_ref(),
                surfaces: surfaces.as_ref(),
                main: &main,
            };
            let all = panels(&saved, &src);
            let current = saved.iter().position(|m| m.id == q.panel).unwrap_or(0);
            let edit = saved.get(current).map_or_else(
                || "/mixer/edit".to_owned(),
                |m| format!("/mixer/edit?panel={}", path_segment(&m.id)),
            );
            html! {
                div.grid {
                    @if saved.len() > 1 {
                        nav.panel-tabs aria-label="Mixer panels" {
                            @for (i, m) in saved.iter().enumerate() {
                                a.btn.sm.cur[i == current] href=(format!("/mixer?panel={}", path_segment(&m.id)))
                                    aria-current=[(i == current).then_some("page")] { (m.title) }
                            }
                        }
                    }
                    @if let Some(p) = all.get(current) { (widgets::panel(p, &s.csrf)) }
                    section.card {
                        p.actions {
                            a.btn href=(edit) { (if saved.is_empty() { "Edit this panel" } else { "Edit panel" }) }
                            " "
                            a.btn.sm href="/mixer/edit?new=1" { "New panel" }
                        }
                        p.muted {
                            "RX is network \u{2192} this host; TX is this host \u{2192} network. "
                            "A channel fader is the channel's own (input) level; Main is "
                            @match &main {
                                MainTarget::HostVolume(_) => { "the speaker volume tesserad sets." }
                                MainTarget::AmpVolume { .. } => { "the amplifier's volume." }
                                MainTarget::OutputStage => { "the output level of host outputs 1–2 (the main stereo pair this host plays), after the local mix; other host outputs keep their own levels (Router page)." }
                            }
                            " The bottom of a channel fader is off. A fader that a connected control surface's fader "
                            "sets is locked and says which; its mute still works. A motorized fader follows this page."
                        }
                        p.muted {
                            @if saved.is_empty() { "Channels not in use are under More channels. " }
                            "Change channel labels on the "
                            a href="/devices" { "Device" } " page, routes on the "
                            a href="/router" { "Router" } " page and surface mappings on the "
                            a href="/surfaces" { "Surfaces" } " page."
                        }
                    }
                }
            }
        }
    };
    page(
        &st,
        &token,
        &s,
        &headers,
        view("Mixer", "/mixer").every(REFRESH),
        &body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_mixer_page_is_a_place_to_return_to() {
        for ok in [
            "/mixer",
            "/mixer#strip-main-rx-1",
            "/mixer?panel=main",
            "/mixer?panel=desk-2#strip-desk-2-s1",
        ] {
            assert_eq!(back_to(ok).as_deref(), Some(ok), "{ok}");
        }
        for bad in [
            "",
            "/devices",
            "/mixerx",
            "/mixer#",
            "/mixer#a b",
            "/mixer#<x>",
            "//evil",
            "/mixer?x=1",
            "/mixer?panel=",
            "/mixer?panel=a b",
            "/mixer?panel=a#",
            "https://x/mixer",
        ] {
            assert_eq!(back_to(bad), None, "{bad}");
        }
    }
}
