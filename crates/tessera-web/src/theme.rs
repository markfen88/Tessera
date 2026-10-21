//! Appearance without JavaScript: the colour theme is a cookie rendered into
//! `<html data-theme=…>` before the stylesheet applies, and the stylesheet is
//! served at a content-hashed path so browsers may cache it for a year.

use std::sync::LazyLock;

use axum::Form;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use crate::auth::same;
use crate::{Shared, cookie_value, session_of};

/// Name of the theme cookie.
pub const COOKIE: &str = "tessera_theme";

/// How long a chosen theme is remembered (one year).
const MAX_AGE_SECS: u32 = 365 * 24 * 3600;

/// The stylesheet, embedded at build time.
pub const CSS: &str = include_str!("../assets/style.css");

/// The colour theme of the pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    /// Follow the browser's `prefers-color-scheme` (no cookie).
    #[default]
    Auto,
    /// Light colours.
    Light,
    /// Dark colours.
    Dark,
}

impl Theme {
    /// Every theme, in the order of the segmented control.
    pub const ALL: [Self; 3] = [Self::Auto, Self::Light, Self::Dark];

    /// The value of the `data-theme` attribute and of the form field.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// The label on the segmented control.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    /// `auto`, `light` or `dark`; anything else is `None`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }

    /// The theme the request's cookie asks for (`Auto` without one).
    #[must_use]
    pub fn of(headers: &HeaderMap) -> Self {
        cookie_value(headers, COOKIE)
            .as_deref()
            .and_then(Self::parse)
            .unwrap_or_default()
    }
}

/// The `Set-Cookie` value that remembers `theme` (`Auto` forgets it).
#[must_use]
pub fn cookie(theme: Theme, secure: bool) -> String {
    let (value, age) = match theme {
        Theme::Auto => ("", 0),
        other => (other.name(), MAX_AGE_SECS),
    };
    format!(
        "{COOKIE}={value}; Max-Age={age}; Path=/; HttpOnly; SameSite=Strict{}",
        if secure { "; Secure" } else { "" }
    )
}

/// The path to send the browser back to: the `Referer`'s path (with its
/// query) when it names this host and a page that answers `GET`
/// ([`crate::pages::is_get_page`]), otherwise the overview. A page that only
/// answers `POST` (the confirmation page) cannot be returned to; a path
/// starting with `//` or `/\` would be read as another host by a browser.
#[must_use]
pub fn back_to(headers: &HeaderMap) -> String {
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let referer = headers.get(header::REFERER).and_then(|h| h.to_str().ok());
    let path = host.zip(referer).and_then(|(host, r)| {
        let (_, rest) = r.split_once("://")?;
        let (r_host, path) = rest.split_once('/').unwrap_or((rest, ""));
        (r_host == host).then(|| format!("/{}", path.split('#').next().unwrap_or("")))
    });
    path.filter(|p| {
        p.starts_with('/')
            && !p.starts_with("//")
            && !p.starts_with("/\\")
            && crate::pages::is_get_page(p.split('?').next().unwrap_or(p))
    })
    .unwrap_or_else(|| "/".to_owned())
}

/// The theme form: the session's CSRF token and the chosen theme.
#[derive(Debug, Deserialize)]
pub struct ThemeForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    theme: String,
}

/// `POST /theme`: remember the theme and go back to the page it was chosen on.
pub async fn set_theme(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<ThemeForm>,
) -> Response {
    let Some((_, s)) = session_of(&st, &headers) else {
        return Redirect::to("/login").into_response();
    };
    if !same(&f.csrf, &s.csrf) {
        return (
            StatusCode::FORBIDDEN,
            "stale or missing form token; reload the page",
        )
            .into_response();
    }
    let Some(theme) = Theme::parse(&f.theme) else {
        return (StatusCode::BAD_REQUEST, "unknown theme").into_response();
    };
    (
        [(header::SET_COOKIE, cookie(theme, st.cookie_secure))],
        Redirect::to(&back_to(&headers)),
    )
        .into_response()
}

// ---- the stylesheet ----------------------------------------------------------

/// FNV-1a over the bytes: a build-time content hash for cache busting (not a
/// security property).
const fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        h ^= bytes[i] as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
        i += 1;
    }
    h
}

const CSS_HASH: u64 = fnv1a(CSS.as_bytes());

static CSS_HASH_HEX: LazyLock<String> = LazyLock::new(|| format!("{CSS_HASH:016x}"));
static CSS_PATH: LazyLock<String> =
    LazyLock::new(|| format!("/assets/{}/style.css", *CSS_HASH_HEX));

/// The content hash of the embedded stylesheet, 16 hex digits.
#[must_use]
pub fn css_hash() -> &'static str {
    &CSS_HASH_HEX
}

/// Where the pages link the stylesheet: `/assets/<hash>/style.css`.
#[must_use]
pub fn css_path() -> &'static str {
    &CSS_PATH
}

const CSS_TYPE: (header::HeaderName, &str) = (header::CONTENT_TYPE, "text/css; charset=utf-8");

/// `GET /style.css` and `GET /static/style.css`: the stylesheet, not cached.
pub async fn stylesheet() -> Response {
    ([CSS_TYPE], CSS).into_response()
}

/// `GET /assets/{hash}/style.css`: the stylesheet, cacheable for a year
/// because its path changes with its content.
pub async fn stylesheet_immutable(Path(hash): Path<String>) -> Response {
    if hash != css_hash() {
        return StatusCode::NOT_FOUND.into_response();
    }
    (
        [
            CSS_TYPE,
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        CSS,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn the_cookie_selects_the_theme_and_auto_forgets_it() {
        assert_eq!(Theme::of(&headers(&[])), Theme::Auto);
        assert_eq!(
            Theme::of(&headers(&[("cookie", "a=b; tessera_theme=dark")])),
            Theme::Dark
        );
        assert_eq!(
            Theme::of(&headers(&[("cookie", "tessera_theme=purple")])),
            Theme::Auto
        );
        assert_eq!(
            cookie(Theme::Light, true),
            "tessera_theme=light; Max-Age=31536000; Path=/; HttpOnly; SameSite=Strict; Secure"
        );
        assert_eq!(
            cookie(Theme::Auto, false),
            "tessera_theme=; Max-Age=0; Path=/; HttpOnly; SameSite=Strict"
        );
    }

    #[test]
    fn the_referer_is_followed_only_on_this_host() {
        let h = headers(&[
            ("host", "box:8090"),
            ("referer", "http://box:8090/clock?x=1#f"),
        ]);
        assert_eq!(back_to(&h), "/clock?x=1");
        let h = headers(&[("host", "box:8090"), ("referer", "http://box:8090")]);
        assert_eq!(back_to(&h), "/");
        let h = headers(&[("host", "box:8090"), ("referer", "http://box:8090/login")]);
        assert_eq!(back_to(&h), "/login");
        for bad in [
            "http://evil.example/clock",
            "http://box:8090//evil.example/",
            "http://box:8090/\\evil.example",
            "http://box:8090/confirm",
            "http://box:8090/devices/main/channels",
            "http://box:8090/api/v1/status",
            "http://box:8090/nope",
        ] {
            let h = headers(&[("host", "box:8090"), ("referer", bad)]);
            assert_eq!(back_to(&h), "/", "{bad}");
        }
        assert_eq!(back_to(&headers(&[("host", "box:8090")])), "/");
    }

    #[test]
    fn the_stylesheet_path_carries_its_content_hash() {
        assert_eq!(css_hash().len(), 16);
        assert_eq!(css_path(), format!("/assets/{}/style.css", css_hash()));
        assert_ne!(fnv1a(b"a"), fnv1a(b"b"));
    }
}
