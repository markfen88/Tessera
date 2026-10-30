//! The EQ engine's files: configurations (`configs_dir`) and filter
//! coefficients (`coeffs_dir`). Listing, upload, activation, deletion and
//! download. Every path is `dir.join(name)` with a `name` that passed
//! [`valid_name`] (one path component of safe characters, never starting
//! with `.`), so nothing outside the two directories can be named; only
//! regular files count (a symbolic link is neither listed, served, edited,
//! activated nor removed). Files are written atomically (a temporary file
//! in the same directory, then a rename) with mode 0664 and the
//! directory's group. An upload never replaces an existing file: delete
//! first. Changes are serialised by the appliance's file lock
//! ([`crate::appliance::Appliance::eq_lock`]).

use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Form;
use axum::extract::{Multipart, State};
use axum::extract::{Path as PathParam, multipart::Field};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use maud::{Markup, html};
use serde::Deserialize;

use crate::Shared;
use crate::appliance::camilla::{self, Active, Client};
use crate::appliance::services::Runner;
use crate::auth::Session;
use crate::config::CamillaDspConfig;
use crate::pages::{Chrome, layout, need, need_post, view};

/// Where the file forms send the operator back.
pub const EQ_ANCHOR: &str = "/speaker#eq";
/// Mode of a written file: owner and group may write.
pub const FILE_MODE: u32 = 0o664;
/// Longest accepted name.
pub const NAME_MAX: usize = 64;
/// Longest text field of the upload form (`csrf`, `kind`).
pub const FIELD_MAX: usize = 256;
/// Slack allowed on the request body above `max_upload_bytes` for the
/// multipart framing and the other fields.
pub const MULTIPART_SLACK: u64 = 64 * 1024;
/// The extensions of a configuration.
pub const CONFIG_EXTENSIONS: [&str; 2] = ["yml", "yaml"];
/// The extensions of a coefficient file.
pub const COEFF_EXTENSIONS: [&str; 5] = ["wav", "txt", "raw", "dbl", "pcm"];

/// Configuration or coefficients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A YAML configuration in `configs_dir`.
    Config,
    /// A coefficient file in `coeffs_dir`.
    Coeff,
}

impl Kind {
    /// The form and path value.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "config" => Some(Self::Config),
            "coeff" => Some(Self::Coeff),
            _ => None,
        }
    }

    /// The form and path value.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Coeff => "coeff",
        }
    }

    /// What the page calls one.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Config => "EQ configuration",
            Self::Coeff => "filter file",
        }
    }

    /// Its directory.
    #[must_use]
    pub fn dir(self, cfg: &CamillaDspConfig) -> &Path {
        match self {
            Self::Config => &cfg.configs_dir,
            Self::Coeff => &cfg.coeffs_dir,
        }
    }

    /// The accepted extensions.
    #[must_use]
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Config => &CONFIG_EXTENSIONS,
            Self::Coeff => &COEFF_EXTENSIONS,
        }
    }

    /// The `accept` attribute of the upload input.
    #[must_use]
    pub fn accept(self) -> String {
        self.extensions()
            .iter()
            .map(|e| format!(".{e}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// `^[A-Za-z0-9._-]{1,64}$`, not starting with `.`: one path component,
/// never `.`, `..`, hidden, or anything with a separator, NUL or
/// non-ASCII.
#[must_use]
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A name that is valid and has one of the kind's extensions (any case).
///
/// # Errors
/// What is wrong with it, for the operator.
pub fn check_name(kind: Kind, name: &str) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!(
            "`{}` is not an accepted name: letters, digits, `.`, `_`, `-`, at most {NAME_MAX} characters, not starting with `.`",
            name.chars()
                .take(NAME_MAX + 1)
                .filter(|c| !c.is_control())
                .collect::<String>()
        ));
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    if !ext.is_some_and(|e| kind.extensions().contains(&e.as_str())) {
        return Err(format!(
            "a {} must end in {}",
            kind.label(),
            kind.extensions()
                .iter()
                .map(|e| format!(".{e}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

/// `dir.join(name)` for a checked name.
///
/// # Errors
/// [`check_name`].
pub fn path_of(cfg: &CamillaDspConfig, kind: Kind, name: &str) -> Result<PathBuf, String> {
    check_name(kind, name)?;
    Ok(kind.dir(cfg).join(name))
}

/// Is `path` a regular file (not a symbolic link to one)?
#[must_use]
pub fn is_regular(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

/// Does anything (a file, a link, a directory) exist at `path`?
#[must_use]
pub fn exists_any(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// One listed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The file name.
    pub name: String,
    /// Bytes.
    pub size: u64,
}

/// The regular files of `dir` whose names pass [`check_name`] for `kind`,
/// sorted by name. Symbolic links are left out. A missing or unreadable
/// directory lists nothing.
pub async fn list(dir: &Path, kind: Kind) -> Vec<Entry> {
    let mut out = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return out;
    };
    while let Ok(Some(e)) = rd.next_entry().await {
        let Some(name) = e.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if check_name(kind, &name).is_err() {
            continue;
        }
        // `DirEntry::metadata` does not follow links.
        let Ok(md) = e.metadata().await else {
            continue;
        };
        if !md.file_type().is_file() {
            continue;
        }
        out.push(Entry {
            name,
            size: md.len(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// `3.9 kB`, `1.2 MB`, `812 B`.
#[must_use]
pub fn human_size(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let b = bytes as f64;
    if bytes < 1000 {
        format!("{bytes} B")
    } else if bytes < 1_000_000 {
        format!("{:.1} kB", b / 1000.0)
    } else {
        format!("{:.1} MB", b / 1_000_000.0)
    }
}

static TEMP_N: AtomicU64 = AtomicU64::new(0);

/// The mode and group a written file gets: those of the file being
/// replaced (its permission bits only, no setuid/setgid), else
/// [`FILE_MODE`] and the directory's group.
fn mode_and_group(path: &Path, dir: &Path) -> (u32, Option<u32>) {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_file() => (md.mode() & 0o777, Some(md.gid())),
        _ => (FILE_MODE, std::fs::metadata(dir).ok().map(|d| d.gid())),
    }
}

/// Set the mode explicitly (the unit's `UMask` does not apply then) and
/// the group as far as membership allows.
fn apply_mode_and_group(
    f: &std::fs::File,
    path: &Path,
    mode: u32,
    gid: Option<u32>,
) -> std::io::Result<()> {
    f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    if let Some(gid) = gid {
        if let Err(e) = std::os::unix::fs::fchown(f, None, Some(gid)) {
            tracing::debug!(path = %path.display(), gid, %e, "cannot set the file's group");
        }
    }
    Ok(())
}

fn dir_and_name(path: &Path) -> Result<(&Path, &str), String> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("{}: no directory", path.display()))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{}: no file name", path.display()))?;
    Ok((dir, name))
}

/// Write `data` to `path` atomically: a temporary file next to it (named
/// `.<name>.tmp-<pid>-<n>`, so listings never show it), synced, then
/// renamed over `path`. Mode and group per [`mode_and_group`].
///
/// # Errors
/// The I/O error, with what failed.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    let (dir, _) = dir_and_name(path)?;
    let (mode, gid) = mode_and_group(path, dir);
    write_atomic_as(path, data, mode, gid)
}

/// [`write_atomic`] with exactly `mode` (set explicitly, whatever the unit's
/// `UMask`) and the portal's own group, whatever the replaced file had.
///
/// # Errors
/// The I/O error, with what failed.
pub fn write_atomic_mode(path: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    write_atomic_as(path, data, mode, None)
}

fn write_atomic_as(path: &Path, data: &[u8], mode: u32, gid: Option<u32>) -> Result<(), String> {
    let (dir, name) = dir_and_name(path)?;
    let tmp = dir.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        TEMP_N.fetch_add(1, Ordering::SeqCst)
    ));
    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        apply_mode_and_group(&f, &tmp, mode, gid)?;
        std::fs::rename(&tmp, path)?;
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(())
}

/// Write `data` to `path` in place (open, truncate, write, sync): for a
/// file that is opened on its own in the unit's `ReadWritePaths` (the
/// engine's state file), where a rename is not possible. Mode and group
/// per [`mode_and_group`]; a symbolic link is refused.
///
/// # Errors
/// The I/O error, with what failed.
pub fn write_in_place(path: &Path, data: &[u8]) -> Result<(), String> {
    let (dir, _) = dir_and_name(path)?;
    if std::fs::symlink_metadata(path).is_ok_and(|m| !m.file_type().is_file()) {
        return Err(format!("{}: not a regular file", path.display()));
    }
    let nofollow = nix::fcntl::OFlag::O_NOFOLLOW.bits();
    let result = (|| -> std::io::Result<()> {
        // An existing file (the engine's, usually another owner) is
        // written as it is: no chmod or chown, which a non-owner may not
        // do. Only a file this call creates gets the mode and group.
        let existing = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(nofollow)
            .open(path);
        let (mut f, created) = match existing {
            Ok(f) => (f, false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .custom_flags(nofollow)
                    .mode(FILE_MODE)
                    .open(path)?,
                true,
            ),
            Err(e) => return Err(e),
        };
        // The new content first, then the length: no moment with an
        // empty file.
        f.write_all(data)?;
        f.set_len(data.len() as u64)?;
        f.sync_all()?;
        if created {
            let gid = std::fs::metadata(dir).ok().map(|d| d.gid());
            apply_mode_and_group(&f, path, FILE_MODE, gid)?;
        }
        Ok(())
    })();
    result.map_err(|e| format!("{}: {e}", path.display()))
}

/// Read a regular file without following a symbolic link at its path.
///
/// # Errors
/// Not there, not a regular file, or unreadable.
pub fn read_regular(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
        .open(path)?;
    let md = f.metadata()?;
    if !md.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let mut v = Vec::with_capacity(usize::try_from(md.len()).unwrap_or(0));
    f.read_to_end(&mut v)?;
    Ok(v)
}

// ---- the operations ----------------------------------------------------------------------

/// Remove `name` of `kind`: never the active configuration, nothing when
/// the active configuration is unknown, only a regular file. Call with
/// the file lock held.
///
/// # Errors
/// The reason, for the operator.
pub fn remove(
    cfg: &CamillaDspConfig,
    kind: Kind,
    name: &str,
    active: &Result<Active, String>,
) -> Result<(), String> {
    let path = path_of(cfg, kind, name)?;
    if kind == Kind::Config {
        match active {
            Ok(a) if a.is(cfg, name) => {
                return Err(format!(
                    "`{name}` is the active EQ configuration; activate another one first."
                ));
            }
            Ok(_) => {}
            Err(why) => {
                return Err(format!(
                    "`{name}` is not deleted while the active configuration is unknown: {why}"
                ));
            }
        }
    }
    if !is_regular(&path) {
        return Err(format!(
            "`{name}` is not a {} in the directory.",
            kind.label()
        ));
    }
    std::fs::remove_file(&path).map_err(|e| format!("cannot delete `{name}`: {e}"))
}

/// Store an upload as `name` of `kind`: a new name only. Call with the
/// file lock held.
///
/// # Errors
/// The name, an existing file (or link) of that name, or the write.
pub fn store(cfg: &CamillaDspConfig, kind: Kind, name: &str, data: &[u8]) -> Result<(), String> {
    let path = path_of(cfg, kind, name)?;
    if exists_any(&path) {
        return Err(format!(
            "`{name}` exists already; the portal never replaces a file: delete it first, then upload."
        ));
    }
    write_atomic(&path, data).map_err(|e| format!("cannot write the {}: {e}", kind.label()))
}

/// Why a download has no body.
#[derive(Debug, PartialEq, Eq)]
pub enum Fetch {
    /// The name does not pass the rules.
    BadName,
    /// No regular file of that name.
    NotFound,
}

/// The bytes of `name` of `kind` (a regular file, opened without
/// following a link).
///
/// # Errors
/// [`Fetch`].
pub fn fetch(cfg: &CamillaDspConfig, kind: Kind, name: &str) -> Result<Vec<u8>, Fetch> {
    let path = path_of(cfg, kind, name).map_err(|_| Fetch::BadName)?;
    read_regular(&path).map_err(|_| Fetch::NotFound)
}

// ---- the card ---------------------------------------------------------------------------

/// The EQ configs and Filter files card.
#[must_use]
pub fn card(
    cfg: &CamillaDspConfig,
    active: &Result<Active, String>,
    configs: &[Entry],
    coeffs: &[Entry],
    csrf: &str,
) -> Markup {
    html! {
        section.card.c8 id="eq" {
            h2 { "EQ configs " span.n { "the EQ engine's configurations" } }
            @if let Err(e) = active {
                p.flash.bad { "The active configuration is unknown; nothing is deleted meanwhile. " (e) }
            }
            @if configs.is_empty() {
                p.muted { "No configurations in " code { (cfg.configs_dir.display()) } "." }
            }
            @for e in configs {
                @let is_active = active.as_ref().is_ok_and(|a| a.is(cfg, &e.name));
                div.file data-live-key=(format!("config:{}", e.name)) {
                    span class="fn" {
                        (e.name)
                        @if is_active { span.tag { "active" } }
                        small { (human_size(e.size)) }
                    }
                    span.actions {
                        @if !is_active {
                            form method="post" action="/speaker/eq/activate" {
                                input type="hidden" name="csrf" value=(csrf);
                                input type="hidden" name="name" value=(e.name);
                                button.btn.sm.primary type="submit" { "Activate" }
                            }
                        }
                        a.btn.sm href=(format!("/speaker/eq/download/config/{}", e.name)) download=(e.name) { "Download" }
                        @if !is_active && active.is_ok() {
                            form method="post" action="/speaker/eq/delete" {
                                input type="hidden" name="csrf" value=(csrf);
                                input type="hidden" name="kind" value="config";
                                input type="hidden" name="name" value=(e.name);
                                button.btn.sm.danger type="submit" { "Delete" }
                            }
                        }
                    }
                }
            }
            (upload_form(Kind::Config, csrf, "Upload config", Some("checked with the EQ engine before it can be activated")))
            h2.links { "Filter files " span.n { "FIR / coefficients" } }
            @if coeffs.is_empty() {
                p.muted { "No filter files in " code { (cfg.coeffs_dir.display()) } "." }
            }
            @for e in coeffs {
                div.file data-live-key=(format!("coeff:{}", e.name)) {
                    span class="fn" { (e.name) small { (human_size(e.size)) } }
                    span.actions {
                        a.btn.sm href=(format!("/speaker/eq/download/coeff/{}", e.name)) download=(e.name) { "Download" }
                        form method="post" action="/speaker/eq/delete" {
                            input type="hidden" name="csrf" value=(csrf);
                            input type="hidden" name="kind" value="coeff";
                            input type="hidden" name="name" value=(e.name);
                            button.btn.sm.danger type="submit" { "Delete" }
                        }
                    }
                }
            }
            (upload_form(Kind::Coeff, csrf, "Upload filter", None))
            p.muted { "Uploads up to " (human_size(cfg.max_upload_bytes)) ", new names only (delete first to replace); names of letters, digits, " code { "." } " " code { "_" } " " code { "-" } "." }
        }
    }
}

fn upload_form(kind: Kind, csrf: &str, button: &str, note: Option<&str>) -> Markup {
    html! {
        form.upload method="post" action="/speaker/eq/upload" enctype="multipart/form-data" {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="kind" value=(kind.name());
            input type="file" name="file" accept=(kind.accept()) required aria-label=(format!("{} to upload", kind.label()));
            button.btn.sm type="submit" { (button) }
            @if let Some(n) = note { span { (n) } }
        }
    }
}

// ---- upload --------------------------------------------------------------------------------

/// The request body limit of the upload route.
#[must_use]
pub fn body_limit(cfg: &CamillaDspConfig) -> usize {
    usize::try_from(cfg.max_upload_bytes.saturating_add(MULTIPART_SLACK)).unwrap_or(usize::MAX)
}

/// A text field of at most [`FIELD_MAX`] bytes; `None` when it is longer
/// (reading stops there) or not text.
async fn small_text(mut field: Field<'_>) -> Option<String> {
    let mut v: Vec<u8> = Vec::new();
    while let Ok(Some(c)) = field.chunk().await {
        if v.len() + c.len() > FIELD_MAX {
            return None;
        }
        v.extend_from_slice(&c);
    }
    String::from_utf8(v).ok()
}

fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        "stale or missing form token; reload the page",
    )
        .into_response()
}

/// `POST /speaker/eq/upload`: multipart with `csrf`, `kind` and `file`, in
/// that order. The token is checked before any of the file is read; any
/// part before the token, or a token field longer than [`FIELD_MAX`], is
/// refused without reading it (403). One file per request. A declared
/// `Content-Length` over the limit is 413 before anything is read; a body
/// that turns out longer is stopped by the route's limit or the count
/// while reading, with nothing written.
pub async fn upload(
    State(st): State<Shared>,
    headers: HeaderMap,
    mut parts: Multipart,
) -> Response {
    // A session before anything is read; the token comes with the body.
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(a) = st
        .appliance
        .as_ref()
        .filter(|a| a.config.camilladsp.enabled)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cfg = &a.config.camilladsp;
    let max = cfg.max_upload_bytes;
    let too_large = format!(
        "The upload is larger than {} (max_upload_bytes).",
        human_size(max)
    );
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    // A declared size over the limit is refused before anything is read
    // (the session was checked; a message needs the token, which is in
    // the body, so this one is a plain 413).
    if declared.is_some_and(|n| n > max.saturating_add(MULTIPART_SLACK)) {
        return (StatusCode::PAYLOAD_TOO_LARGE, too_large).into_response();
    }
    let mut csrf_ok = false;
    let mut kind: Option<Kind> = None;
    let mut result: Option<Result<String, String>> = None;
    loop {
        let field = match parts.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                if !csrf_ok {
                    return forbidden();
                }
                result = Some(Err(format!("bad upload: {e}")));
                break;
            }
        };
        match field.name().unwrap_or("") {
            "csrf" if !csrf_ok => {
                let Some(v) = small_text(field).await else {
                    return forbidden();
                };
                if need_post(&st, &headers, &v).is_err() {
                    return forbidden();
                }
                csrf_ok = true;
            }
            "kind" if csrf_ok => {
                kind = small_text(field).await.as_deref().and_then(Kind::parse);
            }
            "file" => {
                if !csrf_ok {
                    return forbidden();
                }
                let Some(k) = kind else {
                    result = Some(Err("unknown file kind".into()));
                    break;
                };
                // The body is read into memory first; the lock is held
                // only around the store, which checks the name again.
                result = Some(match receive(cfg, k, field, max, &too_large).await {
                    Ok((name, data)) => {
                        let _g = a.eq_lock.lock().await;
                        let (c, n, size) = (cfg.clone(), name.clone(), data.len() as u64);
                        match tokio::task::spawn_blocking(move || store(&c, k, &n, &data))
                            .await
                            .map_err(|e| e.to_string())
                            .and_then(|r| r)
                        {
                            Ok(()) => Ok(format!(
                                "`{name}` uploaded ({}).{}",
                                human_size(size),
                                if k == Kind::Config {
                                    " Activate it to use it."
                                } else {
                                    ""
                                }
                            )),
                            Err(m) => Err(m),
                        }
                    }
                    Err(m) => Err(m),
                });
                // One file per request: the rest of the body is not read.
                break;
            }
            _ => {
                if !csrf_ok {
                    return forbidden();
                }
                // An unknown field after the token: skipped, bounded.
                let _ = small_text(field).await;
            }
        }
    }
    if !csrf_ok {
        return forbidden();
    }
    let r = result.unwrap_or_else(|| Err("no file was sent".into()));
    tracing::info!(session = %s.id, ok = r.is_ok(), result = ?r, "appliance EQ upload");
    match r {
        Ok(m) => st.set_flash(&token, true, m),
        Err(m) => st.set_flash(&token, false, m),
    }
    Redirect::to(EQ_ANCHOR).into_response()
}

/// Read the file field into memory (at most `max` bytes): its name and bytes.
async fn receive(
    cfg: &CamillaDspConfig,
    kind: Kind,
    mut field: Field<'_>,
    max: u64,
    too_large: &str,
) -> Result<(String, Vec<u8>), String> {
    // Browsers send the base name; anything with a separator in it fails
    // the name check as it is.
    let name = field.file_name().unwrap_or("").to_owned();
    let path = path_of(cfg, kind, &name)?;
    if exists_any(&path) {
        return Err(format!(
            "`{name}` exists already; the portal never replaces a file: delete it first, then upload."
        ));
    }
    let mut data: Vec<u8> = Vec::new();
    loop {
        match field.chunk().await {
            Ok(Some(c)) => {
                if (data.len() as u64).saturating_add(c.len() as u64) > max {
                    return Err(too_large.to_owned());
                }
                data.extend_from_slice(&c);
            }
            Ok(None) => break,
            Err(e) => return Err(format!("upload interrupted: {e}")),
        }
    }
    if data.is_empty() {
        return Err(format!("`{name}` is empty."));
    }
    Ok((name, data))
}

// ---- activate / delete ---------------------------------------------------------------------

/// The activate and delete forms.
#[derive(Debug, Deserialize)]
pub struct FileForm {
    #[serde(default)]
    csrf: String,
    /// `config` or `coeff` (delete only; activate is configs only).
    #[serde(default)]
    kind: String,
    /// The file name.
    #[serde(default)]
    name: String,
    /// `yes` from the confirmation page.
    #[serde(default)]
    confirm: String,
}

/// `<binary> -c <path>`: the engine checks the configuration and exits 0
/// when it is usable.
#[must_use]
pub fn check_args(path: &Path) -> Vec<String> {
    vec!["-c".to_owned(), path.to_string_lossy().into_owned()]
}

/// Run the engine's check on `path`.
///
/// # Errors
/// The check failed (its sanitised message) or could not run.
pub async fn check_config(
    cfg: &CamillaDspConfig,
    runner: &dyn Runner,
    path: &Path,
) -> Result<(), String> {
    let bin = cfg.binary.to_string_lossy().into_owned();
    match runner.run(&bin, &check_args(path)).await {
        Ok(o) if o.success() => Ok(()),
        Ok(o) => {
            let why = crate::appliance::services::sanitize(&format!("{}\n{}", o.stderr, o.stdout));
            Err(if why.is_empty() {
                format!(
                    "the EQ engine refused the configuration (exit code {})",
                    o.code.map_or("signal".to_owned(), |c| c.to_string())
                )
            } else {
                format!("the EQ engine refused the configuration: {why}")
            })
        }
        Err(e) => Err(format!("cannot run the EQ engine's check: {e}")),
    }
}

/// `POST /speaker/eq/activate`: check the configuration with the engine,
/// then make it active. The whole sequence holds the file lock.
pub async fn activate(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<FileForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(a) = st
        .appliance
        .as_ref()
        .filter(|a| a.config.camilladsp.enabled)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cfg = &a.config.camilladsp;
    let path = match path_of(cfg, Kind::Config, &f.name) {
        Ok(p) => p,
        Err(m) => {
            tracing::warn!(session = %s.id, name = ?f.name, "appliance: refused a file name");
            return (StatusCode::BAD_REQUEST, m).into_response();
        }
    };
    // The lock, the check and the switch run on a task of their own with
    // the lock's owned guard moved in: a client that goes away mid-way
    // neither cancels the sequence nor lets another change interleave.
    let (lock, config, runner, name) = (
        a.eq_lock.clone(),
        a.config.clone(),
        a.runner.clone(),
        f.name.clone(),
    );
    let r = tokio::spawn(async move {
        let _g = lock.lock_owned().await;
        if !is_regular(&path) {
            return Err(format!(
                "`{name}` is not a configuration file in the directory."
            ));
        }
        check_config(&config.camilladsp, &*runner, &path).await?;
        camilla::activate(&config, &*runner, &name).await
    })
    .await
    .unwrap_or_else(|e| Err(format!("the activation task failed: {e}")));
    tracing::info!(session = %s.id, name = %f.name, ok = r.is_ok(), result = ?r, "appliance EQ activate");
    match r {
        Ok(how) => st.set_flash(
            &token,
            true,
            format!(
                "`{}` is the active EQ configuration: {}",
                f.name,
                how.message()
            ),
        ),
        Err(m) => st.set_flash(&token, false, m),
    }
    Redirect::to(EQ_ANCHOR).into_response()
}

/// `POST /speaker/eq/delete`: a confirmation page first; the active
/// configuration is refused outright, and nothing is deleted while the
/// active configuration is unknown.
pub async fn delete(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<FileForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(a) = st
        .appliance
        .as_ref()
        .filter(|a| a.config.camilladsp.enabled)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cfg = &a.config.camilladsp;
    let Some(kind) = Kind::parse(&f.kind) else {
        return (StatusCode::BAD_REQUEST, "unknown file kind").into_response();
    };
    if let Err(m) = path_of(cfg, kind, &f.name) {
        tracing::warn!(session = %s.id, name = ?f.name, "appliance: refused a file name");
        return (StatusCode::BAD_REQUEST, m).into_response();
    }
    let r = {
        let _g = a.eq_lock.lock().await;
        let active = Active::read(cfg, &Client::new(&cfg.websocket)).await;
        let refused = match &active {
            Ok(act) if kind == Kind::Config && act.is(cfg, &f.name) => Some(format!(
                "`{}` is the active EQ configuration; activate another one first.",
                f.name
            )),
            Err(why) if kind == Kind::Config => Some(format!(
                "`{}` is not deleted while the active configuration is unknown: {why}",
                f.name
            )),
            _ => None,
        };
        match refused {
            Some(m) => Err(m),
            None if f.confirm != "yes" => {
                return confirm_page(&st, &headers, &s, kind, &f.name);
            }
            None => remove(cfg, kind, &f.name, &active),
        }
    };
    tracing::info!(session = %s.id, kind = kind.name(), name = %f.name, ok = r.is_ok(), "appliance EQ delete");
    match r {
        Ok(()) => st.set_flash(&token, true, format!("`{}` deleted.", f.name)),
        Err(m) => st.set_flash(&token, false, m),
    }
    Redirect::to(EQ_ANCHOR).into_response()
}

fn confirm_page(st: &Shared, headers: &HeaderMap, s: &Session, kind: Kind, name: &str) -> Response {
    let body = html! {
        section.card {
            p.flash.bad {
                strong { "Deleting the " (kind.label()) " " code { (name) } " removes it from the device." }
                @if kind == Kind::Coeff { " A configuration that uses it stops loading." }
            }
            form method="post" action="/speaker/eq/delete" {
                input type="hidden" name="csrf" value=(s.csrf);
                input type="hidden" name="kind" value=(kind.name());
                input type="hidden" name="name" value=(name);
                input type="hidden" name="confirm" value="yes";
                button.btn.danger type="submit" { "Delete " (name) }
                " "
                a href=(EQ_ANCHOR) { "Cancel" }
            }
            p.muted { "Cancel changes nothing." }
        }
    };
    (
        StatusCode::OK,
        layout(
            &view("Confirm the deletion", "/speaker"),
            &Chrome::of(st, headers),
            Some(s),
            None,
            &body,
        ),
    )
        .into_response()
}

// ---- download --------------------------------------------------------------------------------

/// `GET /speaker/eq/download/{kind}/{name}`: the file as an attachment.
pub async fn download(
    State(st): State<Shared>,
    headers: HeaderMap,
    PathParam((kind, name)): PathParam<(String, String)>,
) -> Response {
    if let Err(r) = need(&st, &headers) {
        return r;
    }
    let Some(a) = st
        .appliance
        .as_ref()
        .filter(|a| a.config.camilladsp.enabled)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(kind) = Kind::parse(&kind) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cfg = a.config.camilladsp.clone();
    let n = name.clone();
    let data = match tokio::task::spawn_blocking(move || fetch(&cfg, kind, &n)).await {
        Ok(Ok(d)) => d,
        Ok(Err(Fetch::BadName)) => {
            return (StatusCode::BAD_REQUEST, "not an accepted file name").into_response();
        }
        Ok(Err(Fetch::NotFound)) | Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    // The name passed `valid_name`: ASCII letters, digits, `.`, `_`, `-`.
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    (StatusCode::OK, h, data).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_one_safe_path_component() {
        for ok in [
            "a.yml",
            "Speaker1.flat.yaml",
            "left_fir-96k.wav",
            "x",
            "a..b",
        ] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            ".hidden.yml",
            "a/b.yml",
            "../a.yml",
            "a\\b.yml",
            "a b.yml",
            "a\0.yml",
            "é.yml",
            "a\n.yml",
            "a;b",
            "a:b",
            &"x".repeat(65),
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert!(valid_name(&"x".repeat(64)));
        assert!(check_name(Kind::Config, "a.yml").is_ok());
        assert!(check_name(Kind::Config, "A.YML").is_ok());
        assert!(check_name(Kind::Config, "a.YAML").is_ok());
        assert!(check_name(Kind::Config, "a.wav").is_err());
        assert!(check_name(Kind::Config, "yml").is_err());
        assert!(check_name(Kind::Config, &format!("{}.yml", "x".repeat(61))).is_err());
        assert!(check_name(Kind::Config, &format!("{}.yml", "x".repeat(60))).is_ok());
        assert!(check_name(Kind::Coeff, "a.wav").is_ok());
        assert!(check_name(Kind::Coeff, "a.txt").is_ok());
        assert!(check_name(Kind::Coeff, "a.yml").is_err());
        assert!(check_name(Kind::Coeff, "../a.wav").is_err());
        let c = CamillaDspConfig::default();
        assert_eq!(
            path_of(&c, Kind::Config, "a.yml").unwrap(),
            PathBuf::from("/opt/camilladsp/configs/a.yml")
        );
        assert_eq!(
            path_of(&c, Kind::Coeff, "a.raw").unwrap(),
            PathBuf::from("/opt/camilladsp/coeffs/a.raw")
        );
        assert!(path_of(&c, Kind::Config, "/etc/passwd").is_err());
        assert!(path_of(&c, Kind::Config, "..").is_err());
        assert_eq!(Kind::Config.accept(), ".yml,.yaml");
        assert_eq!(Kind::Coeff.accept(), ".wav,.txt,.raw,.dbl,.pcm");
        assert_eq!(Kind::parse("config"), Some(Kind::Config));
        assert_eq!(Kind::parse("other"), None);
    }

    #[test]
    fn sizes_and_limits() {
        assert_eq!(human_size(812), "812 B");
        assert_eq!(human_size(3_900), "3.9 kB");
        assert_eq!(human_size(1_234_567), "1.2 MB");
        assert_eq!(human_size(33_554_432), "33.6 MB");
        let c = CamillaDspConfig {
            max_upload_bytes: 4096,
            ..CamillaDspConfig::default()
        };
        assert_eq!(body_limit(&c), 4096 + 64 * 1024);
        assert_eq!(check_args(Path::new("/x/a.yml")), ["-c", "/x/a.yml"]);
    }

    fn temp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tessera-web-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_write_sets_the_mode_and_keeps_an_existing_one() {
        let dir = temp("atomic");
        let p = dir.join("a.yml");
        write_atomic(&p, b"one\n").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"one\n");
        // Explicit, whatever the umask.
        assert_eq!(std::fs::metadata(&p).unwrap().mode() & 0o7777, FILE_MODE);
        let gid = std::fs::metadata(&p).unwrap().gid();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o2640)).unwrap();
        write_atomic(&p, b"two\n").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two\n");
        // Permission bits kept, setgid not carried over, group kept.
        assert_eq!(std::fs::metadata(&p).unwrap().mode() & 0o7777, 0o640);
        assert_eq!(std::fs::metadata(&p).unwrap().gid(), gid);
        // Nothing left behind.
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.yml"]);
        assert!(write_atomic(&dir.join("missing").join("x"), b"").is_err());
        assert!(write_atomic(Path::new("/"), b"").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn in_place_write_keeps_the_inode_and_refuses_links() {
        let dir = temp("inplace");
        let p = dir.join("statefile.yml");
        write_in_place(&p, b"config_path: /a\n").unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().mode() & 0o7777, FILE_MODE);
        let ino = std::fs::metadata(&p).unwrap().ino();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        // An existing file keeps its inode and mode (nothing is chmod'ed:
        // the engine's user owns it on a device; the tests run as one
        // user, so the EPERM path itself is not exercised here). A longer
        // old content is cut to the new length.
        write_in_place(&p, b"config_path: /b\nmute: false\nvolume: -6.0\n").unwrap();
        write_in_place(&p, b"config_path: /c\n").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"config_path: /c\n");
        assert_eq!(std::fs::metadata(&p).unwrap().ino(), ino);
        assert_eq!(std::fs::metadata(&p).unwrap().mode() & 0o7777, 0o600);
        let target = dir.join("target");
        std::fs::write(&target, b"keep").unwrap();
        let link = dir.join("link.yml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(write_in_place(&link, b"x").is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        assert!(read_regular(&link).is_err());
        assert_eq!(read_regular(&target).unwrap(), b"keep");
        assert!(read_regular(&dir).is_err());
        assert!(is_regular(&target) && !is_regular(&link) && !is_regular(&dir));
        assert!(exists_any(&link) && !exists_any(&dir.join("none")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn store_never_replaces_and_remove_needs_a_known_active_config() {
        let dir = temp("store");
        let cfg = CamillaDspConfig {
            configs_dir: dir.join("configs"),
            coeffs_dir: dir.join("coeffs"),
            ..CamillaDspConfig::default()
        };
        std::fs::create_dir_all(&cfg.configs_dir).unwrap();
        std::fs::create_dir_all(&cfg.coeffs_dir).unwrap();
        store(&cfg, Kind::Config, "a.yml", b"x").unwrap();
        let e = store(&cfg, Kind::Config, "a.yml", b"y").unwrap_err();
        assert!(e.contains("exists already"), "{e}");
        assert_eq!(std::fs::read(cfg.configs_dir.join("a.yml")).unwrap(), b"x");
        std::os::unix::fs::symlink("/etc/hostname", cfg.coeffs_dir.join("l.wav")).unwrap();
        assert!(
            store(&cfg, Kind::Coeff, "l.wav", b"y")
                .unwrap_err()
                .contains("exists already")
        );
        assert!(store(&cfg, Kind::Coeff, "../x.wav", b"y").is_err());
        let active = Ok(Active {
            path: Some(cfg.configs_dir.join("a.yml")),
            label: None,
            boost_db: None,
            source: camilla::Source::Engine,
        });
        assert!(
            remove(&cfg, Kind::Config, "a.yml", &active)
                .unwrap_err()
                .contains("active")
        );
        assert!(
            remove(&cfg, Kind::Config, "a.yml", &Err("x".into()))
                .unwrap_err()
                .contains("unknown")
        );
        // A link is not removed (and not followed), a coefficient does not
        // need the active configuration.
        assert!(
            remove(&cfg, Kind::Coeff, "l.wav", &Err("x".into()))
                .unwrap_err()
                .contains("not a filter file")
        );
        assert!(exists_any(&cfg.coeffs_dir.join("l.wav")));
        store(&cfg, Kind::Coeff, "b.wav", b"x").unwrap();
        remove(&cfg, Kind::Coeff, "b.wav", &Err("x".into())).unwrap();
        store(&cfg, Kind::Config, "b.yml", b"x").unwrap();
        remove(&cfg, Kind::Config, "b.yml", &active).unwrap();
        assert!(remove(&cfg, Kind::Config, "b.yml", &active).is_err());
        assert_eq!(fetch(&cfg, Kind::Config, "a.yml"), Ok(b"x".to_vec()));
        assert_eq!(fetch(&cfg, Kind::Coeff, "l.wav"), Err(Fetch::NotFound));
        assert_eq!(fetch(&cfg, Kind::Config, "../a.yml"), Err(Fetch::BadName));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
