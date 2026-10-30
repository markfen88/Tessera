//! `/etc/tessera/web.toml` (ADR 0013). Every key is optional; a missing file
//! means the defaults.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tessera_state::dropin;

/// Default configuration file.
pub const DEFAULT_PATH: &str = "/etc/tessera/web.toml";

/// The portal's settings.
#[derive(Debug, Clone, PartialEq)]
pub struct WebConfig {
    /// Address and port to listen on (default `127.0.0.1:8090`).
    pub listen: SocketAddr,
    /// The daemon's control socket.
    pub control_socket: PathBuf,
    /// The Argon2id hash of the operator password.
    pub password_file: PathBuf,
    /// Mark the session cookie `Secure` (behind a TLS reverse proxy).
    pub cookie_secure: bool,
    /// A session unused for this long ends.
    pub session_idle: Duration,
    /// How long one request to the daemon may take.
    pub daemon_timeout: Duration,
    /// `[appliance]`: the Speaker landing page and its service switches.
    pub appliance: ApplianceConfig,
    /// Authenticated receiver-host routing.
    pub routing: crate::routing_peers::config::Config,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8090)),
            control_socket: "/run/tessera/control.sock".into(),
            password_file: "/etc/tessera/web-password".into(),
            cookie_secure: false,
            session_idle: Duration::from_secs(8 * 3600),
            daemon_timeout: Duration::from_secs(5),
            appliance: ApplianceConfig::default(),
            routing: Default::default(),
        }
    }
}

// ---- [appliance] -------------------------------------------------------------

/// `[appliance]`: appliance mode (off by default). When on, `/` is the
/// Speaker page with one switch per configured service; the overview moves
/// to `/device`.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplianceConfig {
    /// The Speaker page exists and is the landing page.
    pub enabled: bool,
    /// Display name of the appliance; `None`: the host name.
    pub hostname_label: Option<String>,
    /// The services shown, in this order (`[[appliance.service]]`).
    pub services: Vec<ServiceEntry>,
    /// `[appliance.camilladsp]` (used by the EQ pages; parsed here so the
    /// schema is complete).
    pub camilladsp: CamillaDspConfig,
    /// `[appliance.volume]`: the amplifier's mixer control. `None`: no
    /// Volume card.
    pub volume: Option<VolumeConfig>,
}

impl Default for ApplianceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            hostname_label: None,
            services: [
                ("tesserad", "Network audio", "Tessera audio daemon", true),
                (
                    "tessera-controller@main",
                    "Controller compatibility",
                    "Interoperable with Dante® Controller",
                    true,
                ),
                ("tessera-web", "This portal", "Tessera web portal", true),
                (
                    "tessera-network",
                    "Network configuration",
                    "Standalone network configuration",
                    true,
                ),
                (
                    "avahi-daemon",
                    "Device discovery",
                    "Local network service discovery",
                    true,
                ),
            ]
            .into_iter()
            .map(|(unit, label, description, essential)| ServiceEntry {
                units: vec![unit.into()],
                label: label.into(),
                description: description.into(),
                essential,
            })
            .collect(),
            camilladsp: CamillaDspConfig::default(),
            volume: None,
        }
    }
}

/// `[appliance.volume]`: the hardware mixer control the Volume card sets
/// with `amixer`.
#[derive(Debug, Clone, PartialEq)]
pub struct VolumeConfig {
    /// The ALSA card (`amixer -c <card>`), by name or index.
    pub card: String,
    /// The simple mixer control (default `Digital`).
    pub control: String,
    /// Lowest value offered, dB (default -60).
    pub min_db: f64,
    /// Highest value offered, dB (default 0).
    pub max_db: f64,
    /// Where the portal keeps the committed value, to set it again at start
    /// (default `/var/lib/tessera-web/volume.json`, the unit's state
    /// directory).
    pub state_file: PathBuf,
}

impl VolumeConfig {
    /// The default control name.
    pub const DEFAULT_CONTROL: &'static str = "Digital";
    /// The default range, dB.
    pub const DEFAULT_RANGE: (f64, f64) = (-60.0, 0.0);
    /// The default state file, in the unit's `StateDirectory=tessera-web`.
    pub const DEFAULT_STATE_FILE: &'static str = "/var/lib/tessera-web/volume.json";
}

/// An ALSA card name or index as `amixer -c` takes it: letters, digits,
/// `_` and `-`, at most 64 bytes.
#[must_use]
pub fn valid_card_name(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 64
        && c.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// A simple mixer control name: printable ASCII (spaces allowed inside,
/// as in `PCM Playback`), not starting with `-`, at most 64 bytes.
#[must_use]
pub fn valid_control_name(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 64
        && c == c.trim()
        && !c.starts_with('-')
        && c.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// One row of the Services card: one systemd unit, or a group of units that
/// share one switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceEntry {
    /// Shown in bold.
    pub label: String,
    /// Shown small under the label; default: the unit names.
    pub description: String,
    /// One unit, or several for a group. Never empty.
    pub units: Vec<String>,
    /// Disabling asks for confirmation first.
    pub essential: bool,
}

impl ServiceEntry {
    /// The value the form sends to name this entry: the unit, or the units
    /// of a group joined with `+`. Unique within a configuration.
    #[must_use]
    pub fn key(&self) -> String {
        self.units.join("+")
    }

    /// Several units behind one switch.
    #[must_use]
    pub fn is_group(&self) -> bool {
        self.units.len() > 1
    }

    /// Whether this entry acts on `unit` (`bluetooth` and
    /// `bluetooth.service` are the same unit).
    #[must_use]
    pub fn has_unit(&self, unit: &str) -> bool {
        let want = canonical_unit(unit);
        self.units.iter().any(|u| canonical_unit(u) == want)
    }

    /// The units as systemd names them (`name.service`), in order.
    #[must_use]
    pub fn canonical_units(&self) -> Vec<String> {
        self.units.iter().map(|u| canonical_unit(u)).collect()
    }
}

/// `name` → `name.service`; a name that already ends in `.service` is
/// returned as is. The portal accepts only service units.
#[must_use]
pub fn canonical_unit(u: &str) -> String {
    if u.ends_with(".service") {
        u.to_owned()
    } else {
        format!("{u}.service")
    }
}

/// The units that must always ask before they are disabled or restarted,
/// whatever the configuration says: the daemon and this portal.
#[must_use]
pub fn always_essential(unit: &str) -> bool {
    matches!(
        canonical_unit(unit).as_str(),
        "tesserad.service" | "tessera-web.service"
    )
}

/// Is `unit` this portal's own service?
#[must_use]
pub fn is_own_unit(unit: &str) -> bool {
    canonical_unit(unit) == "tessera-web.service"
}

/// `[appliance.camilladsp]`: where the EQ engine is and what the portal may
/// touch of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CamillaDspConfig {
    /// The EQ cards exist.
    pub enabled: bool,
    /// The engine's websocket (`ws://`, no TLS).
    pub websocket: String,
    /// Configurations the operator can activate.
    pub configs_dir: PathBuf,
    /// Filter coefficient files.
    pub coeffs_dir: PathBuf,
    /// The engine's state file (names the active configuration).
    pub statefile: PathBuf,
    /// The engine binary, for `--check` on uploads.
    pub binary: PathBuf,
    /// Name of the Gain filter that the signal boost edits.
    pub gain_filter: String,
    /// A link to the engine's own GUI, if one runs.
    pub gui_url: Option<String>,
    /// Largest accepted upload.
    pub max_upload_bytes: u64,
}

impl Default for CamillaDspConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            websocket: "ws://127.0.0.1:1234".into(),
            configs_dir: "/opt/camilladsp/configs".into(),
            coeffs_dir: "/opt/camilladsp/coeffs".into(),
            statefile: "/opt/camilladsp/statefile.yml".into(),
            binary: "/opt/camilladsp/camilladsp".into(),
            gain_filter: "dashboard_signal_boost".into(),
            gui_url: None,
            max_upload_bytes: 32 * 1024 * 1024,
        }
    }
}

/// The placeholder in `gui_url` that the page replaces with the host the
/// browser used to reach the portal ([`crate::appliance::camilla::gui_link`]).
pub const GUI_HOST_PLACEHOLDER: &str = "{host}";

/// Is `u` an acceptable `gui_url`: `http://` or `https://`, and braces only
/// as one `{host}` in the authority (between the scheme and the first `/`,
/// `?` or `#`)? A plain URL without the placeholder is used as it is.
///
/// # Errors
/// What is wrong with it, for the configuration error.
pub fn valid_gui_url(u: &str) -> Result<(), String> {
    let Some(rest) = u
        .strip_prefix("http://")
        .or_else(|| u.strip_prefix("https://"))
    else {
        return Err("must start with `http://` or `https://`".into());
    };
    let braces = u.matches(['{', '}']).count();
    if braces == 0 {
        return Ok(());
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if braces != 2 || u.matches(GUI_HOST_PLACEHOLDER).count() != 1 {
        return Err(format!(
            "may contain `{GUI_HOST_PLACEHOLDER}` once and no other braces"
        ));
    }
    if !authority.contains(GUI_HOST_PLACEHOLDER) {
        return Err(format!(
            "may use `{GUI_HOST_PLACEHOLDER}` only as the host, before the path"
        ));
    }
    Ok(())
}

/// A systemd service name the portal accepts in its allowlist: letters,
/// digits, `.`, `_`, `@`, `-`; not starting with `-` or `.`; at most 255
/// bytes; with no suffix or `.service` (no timers, sockets, ...). The same
/// characters are safe as a `systemctl` argument and inside the generated
/// polkit rule.
#[must_use]
pub fn valid_unit_name(u: &str) -> bool {
    let stem = u.strip_suffix(".service").unwrap_or(u);
    // The helper instance unit `tessera-web-unitfile@<verb>-<stem>.service`
    // must itself be a unit name (255 bytes); `@` is written `\x40` there.
    let escaped: usize = stem.bytes().map(|b| if b == b'@' { 4 } else { 1 }).sum();
    !stem.is_empty()
        && "tessera-web-unitfile@".len() + "disable-".len() + escaped + ".service".len() <= 255
        && !u.starts_with(['-', '.', '@'])
        && !stem.ends_with('@')
        && !stem.contains('.')
        && stem.matches('@').count() <= 1
        && u.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'@' | b'-'))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplianceFile {
    enabled: Option<bool>,
    hostname_label: Option<String>,
    #[serde(default, rename = "service")]
    services: Option<Vec<ServiceFile>>,
    camilladsp: Option<CamillaFile>,
    volume: Option<VolumeFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VolumeFile {
    card: Option<String>,
    control: Option<String>,
    min_db: Option<f64>,
    max_db: Option<f64>,
    state_file: Option<PathBuf>,
}

impl VolumeConfig {
    fn from_file(f: VolumeFile) -> Result<Self, String> {
        let card = f.card.map(|c| c.trim().to_owned()).unwrap_or_default();
        if card.is_empty() {
            return Err("`appliance.volume.card` is required (the ALSA card name or index)".into());
        }
        if !valid_card_name(&card) {
            return Err(format!(
                "`appliance.volume.card`: `{card}` is not a card name (letters, digits, `_`, `-`)"
            ));
        }
        let control = f
            .control
            .map(|c| c.trim().to_owned())
            .unwrap_or_else(|| Self::DEFAULT_CONTROL.to_owned());
        if !valid_control_name(&control) {
            return Err(format!(
                "`appliance.volume.control`: `{control}` is not a mixer control name (printable ASCII, not starting with `-`)"
            ));
        }
        let (dmin, dmax) = Self::DEFAULT_RANGE;
        let min_db = f.min_db.unwrap_or(dmin);
        let max_db = f.max_db.unwrap_or(dmax);
        if !(min_db.is_finite() && max_db.is_finite()) || min_db >= max_db {
            return Err("`appliance.volume`: `min_db` must be below `max_db`".into());
        }
        if max_db - min_db > 1000.0 {
            return Err(
                "`appliance.volume`: the range `min_db`..`max_db` is too wide (at most 1000 dB)"
                    .into(),
            );
        }
        let state_file = f
            .state_file
            .unwrap_or_else(|| Self::DEFAULT_STATE_FILE.into());
        if !state_file.is_absolute() || state_file.file_name().is_none() {
            return Err(format!(
                "`appliance.volume.state_file`: `{}` is not an absolute file path",
                state_file.display()
            ));
        }
        Ok(Self {
            card,
            control,
            min_db,
            max_db,
            state_file,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceFile {
    unit: Option<String>,
    units: Option<Vec<String>>,
    label: Option<String>,
    description: Option<String>,
    essential: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CamillaFile {
    enabled: Option<bool>,
    websocket: Option<String>,
    configs_dir: Option<PathBuf>,
    coeffs_dir: Option<PathBuf>,
    statefile: Option<PathBuf>,
    binary: Option<PathBuf>,
    gain_filter: Option<String>,
    gui_url: Option<String>,
    max_upload_bytes: Option<u64>,
}

impl ApplianceConfig {
    fn from_file(f: ApplianceFile) -> Result<Self, String> {
        let use_defaults = f.services.is_none();
        let mut services: Vec<ServiceEntry> = Vec::new();
        for (i, s) in f.services.unwrap_or_default().into_iter().enumerate() {
            let at = format!("`appliance.service[{i}]`");
            let units = match (s.unit, s.units) {
                (Some(_), Some(_)) => {
                    return Err(format!("{at}: set `unit` or `units`, not both"));
                }
                (None, None) => return Err(format!("{at}: one of `unit` or `units` is required")),
                (Some(u), None) => vec![u],
                (None, Some(us)) => {
                    if us.is_empty() {
                        return Err(format!(
                            "`appliance.service[{i}].units` must name at least one unit"
                        ));
                    }
                    us
                }
            };
            let key = if units.len() == 1 { "unit" } else { "units" };
            for u in &units {
                if !valid_unit_name(u) {
                    return Err(format!(
                        "`appliance.service[{i}].{key}`: `{u}` is not a service name (letters, digits, \
                         `.`, `_`, `@`, `-`; not starting with `-` or `.`; no suffix or `.service`)"
                    ));
                }
                let c = canonical_unit(u);
                if units.iter().filter(|x| canonical_unit(x) == c).count() > 1 {
                    return Err(format!(
                        "`appliance.service[{i}].{key}`: `{u}` is listed twice"
                    ));
                }
                if let Some(j) = services.iter().position(|e| e.has_unit(u)) {
                    return Err(format!(
                        "`appliance.service[{i}].{key}`: `{u}` is already in `appliance.service[{j}]`"
                    ));
                }
            }
            let label = s.label.map(|l| l.trim().to_owned()).unwrap_or_default();
            if label.is_empty() {
                return Err(format!("`appliance.service[{i}].label` is required"));
            }
            let description = s
                .description
                .map(|d| d.trim().to_owned())
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| units.join(" · "));
            // The daemon and the portal always ask, whatever the file says.
            let essential =
                s.essential.unwrap_or(false) || units.iter().any(|u| always_essential(u));
            services.push(ServiceEntry {
                label,
                description,
                units,
                essential,
            });
        }
        let d = CamillaDspConfig::default();
        let camilladsp = match f.camilladsp {
            None => d,
            Some(c) => {
                let websocket = c.websocket.unwrap_or(d.websocket);
                if !websocket.starts_with("ws://") {
                    return Err(
                        "`appliance.camilladsp.websocket` must be a `ws://` address (no TLS)"
                            .into(),
                    );
                }
                let gui_url = c
                    .gui_url
                    .map(|u| u.trim().to_owned())
                    .filter(|u| !u.is_empty());
                if let Some(u) = &gui_url {
                    valid_gui_url(u).map_err(|e| format!("`appliance.camilladsp.gui_url` {e}"))?;
                }
                let max_upload_bytes = c.max_upload_bytes.unwrap_or(d.max_upload_bytes);
                if max_upload_bytes == 0 {
                    return Err("`appliance.camilladsp.max_upload_bytes` must be at least 1".into());
                }
                let configs_dir = c.configs_dir.unwrap_or(d.configs_dir);
                let coeffs_dir = c.coeffs_dir.unwrap_or(d.coeffs_dir);
                let statefile = c.statefile.unwrap_or(d.statefile);
                let binary = c.binary.unwrap_or(d.binary);
                for (key, p) in [
                    ("configs_dir", &configs_dir),
                    ("coeffs_dir", &coeffs_dir),
                    ("statefile", &statefile),
                    ("binary", &binary),
                ] {
                    if !p.is_absolute() {
                        return Err(format!(
                            "`appliance.camilladsp.{key}` must be an absolute path (is `{}`)",
                            p.display()
                        ));
                    }
                }
                let gain_filter = c.gain_filter.unwrap_or(d.gain_filter);
                if gain_filter.trim().is_empty() {
                    return Err("`appliance.camilladsp.gain_filter` must not be empty".into());
                }
                CamillaDspConfig {
                    enabled: c.enabled.unwrap_or(d.enabled),
                    websocket,
                    configs_dir,
                    coeffs_dir,
                    statefile,
                    binary,
                    gain_filter,
                    gui_url,
                    max_upload_bytes,
                }
            }
        };
        Ok(Self {
            enabled: f.enabled.unwrap_or(false),
            hostname_label: f
                .hostname_label
                .map(|h| h.trim().to_owned())
                .filter(|h| !h.is_empty()),
            services: if use_defaults {
                Self::default().services
            } else {
                services
            },
            camilladsp,
            volume: f.volume.map(VolumeConfig::from_file).transpose()?,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    listen: Option<SocketAddr>,
    control_socket: Option<PathBuf>,
    password_file: Option<PathBuf>,
    cookie_secure: Option<bool>,
    session_idle_secs: Option<u64>,
    daemon_timeout_secs: Option<u64>,
    appliance: Option<ApplianceFile>,
    routing: Option<crate::routing_peers::config::Config>,
    // Validate the helper's policy here; only tessera-network enforces it.
    #[serde(rename = "network")]
    _network: Option<tessera_state::network_profiles::NetworkPolicy>,
}

impl WebConfig {
    /// Service controls are available even without the Speaker landing page.
    #[must_use]
    pub fn services_config(&self) -> ApplianceConfig {
        ApplianceConfig {
            enabled: true,
            ..self.appliance.clone()
        }
    }
    /// Parse the file's text.
    ///
    /// # Errors
    /// The TOML parser's message (unknown keys included), or a validation
    /// error naming the key.
    pub fn parse(text: &str) -> Result<Self, String> {
        let f: File = toml::from_str(text).map_err(|e| e.to_string())?;
        let d = Self::default();
        let routing = f.routing.unwrap_or_default();
        routing.validate()?;
        let secs = |v: Option<u64>, dflt: Duration, what: &str| match v {
            Some(0) => Err(format!("`{what}` must be at least 1")),
            Some(n) => Ok(Duration::from_secs(n)),
            None => Ok(dflt),
        };
        Ok(Self {
            routing,
            listen: f.listen.unwrap_or(d.listen),
            control_socket: f.control_socket.unwrap_or(d.control_socket),
            password_file: f.password_file.unwrap_or(d.password_file),
            cookie_secure: f.cookie_secure.unwrap_or(d.cookie_secure),
            session_idle: secs(f.session_idle_secs, d.session_idle, "session_idle_secs")?,
            daemon_timeout: secs(
                f.daemon_timeout_secs,
                d.daemon_timeout,
                "daemon_timeout_secs",
            )?,
            appliance: match f.appliance {
                Some(a) => ApplianceConfig::from_file(a)?,
                None => d.appliance,
            },
        })
    }

    /// Read `path` and the drop-ins in `web.d` next to it; a missing file gives the
    /// defaults.
    ///
    /// # Errors
    /// A file cannot be read or does not parse.
    pub fn load(path: &Path) -> Result<Self, String> {
        let base = match std::fs::read_to_string(path) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let files = dropins(path)?;
        if files.is_empty() {
            return match base {
                Some(t) => Self::parse(&t).map_err(|e| format!("{}: {e}", path.display())),
                None => Ok(Self::default()),
            };
        }
        let table = |file: &Path, text: &str| {
            text.parse::<toml::Table>()
                .map_err(|e| format!("{}: {e}", file.display()))
        };
        let mut merged = match &base {
            Some(t) => table(path, t)?,
            None => toml::Table::new(),
        };
        for file in &files {
            let text =
                std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
            dropin::merge(&mut merged, table(file, &text)?);
        }
        let text = toml::to_string(&merged).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{} with its drop-ins: {e}", path.display()))
    }
}

/// The drop-in files of a configuration file, in the order they apply: the `*.toml` files of the
/// directory named like the file with `.d` for its extension (`web.toml` -> `web.d`), by name.
/// Hidden files and other extensions are ignored; a missing directory has none.
///
/// # Errors
/// The directory cannot be read, or a drop-in is not a regular file or is too large.
pub fn dropins(path: &Path) -> Result<Vec<PathBuf>, String> {
    let dir = dropin::directory(path);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !dropin::counts(&name) {
            continue;
        }
        let file = entry.path();
        let meta = std::fs::metadata(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        if !meta.is_file() {
            return Err(format!("{}: not a regular file", file.display()));
        }
        if meta.len() > dropin::MAX_BYTES {
            return Err(format!(
                "{}: larger than {} bytes",
                file.display(),
                dropin::MAX_BYTES
            ));
        }
        files.push(file);
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_listen_on_loopback_only() {
        let c = WebConfig::parse("").unwrap();
        assert_eq!(c, WebConfig::default());
        assert!(c.listen.ip().is_loopback());
        assert!(!c.appliance.enabled);
        assert!(
            c.appliance
                .services
                .iter()
                .any(|s| s.has_unit("tessera-controller@main"))
        );
    }

    #[test]
    fn keys_are_read_and_unknown_ones_refused() {
        let c = WebConfig::parse(
            "listen = \"0.0.0.0:8443\"\ncookie_secure = true\nsession_idle_secs = 60\n",
        )
        .unwrap();
        assert_eq!(c.listen.port(), 8443);
        assert!(c.cookie_secure);
        assert_eq!(c.session_idle, Duration::from_secs(60));
        assert!(WebConfig::parse("listn = \"0.0.0.0:1\"").is_err());
        assert!(WebConfig::parse("session_idle_secs = 0").is_err());
    }

    const APPLIANCE: &str = r#"
[appliance]
enabled = true
hostname_label = " speaker "

[[appliance.service]]
unit = "tesserad"
label = "Network audio"
description = "tesserad · interoperable with Dante®"
essential = true

[[appliance.service]]
unit = "camilladsp"
label = "EQ engine"

[[appliance.service]]
label = "Bluetooth"
units = ["bluetooth", "bluealsa", "bluealsa-aplay"]

[appliance.camilladsp]
enabled = true
gui_url = "http://speaker.example:5005/gui/index.html"
max_upload_bytes = 1024
"#;

    #[test]
    fn appliance_section_is_parsed_with_defaults() {
        let c = WebConfig::parse(APPLIANCE).unwrap().appliance;
        assert!(c.enabled);
        assert_eq!(c.hostname_label.as_deref(), Some("speaker"));
        assert_eq!(c.services.len(), 3);
        let s = &c.services[0];
        assert_eq!(s.key(), "tesserad");
        assert!(s.essential && !s.is_group());
        assert_eq!(s.description, "tesserad · interoperable with Dante®");
        let s = &c.services[1];
        assert_eq!(s.description, "camilladsp");
        assert!(!s.essential);
        let g = &c.services[2];
        assert!(g.is_group());
        assert_eq!(g.key(), "bluetooth+bluealsa+bluealsa-aplay");
        assert_eq!(g.description, "bluetooth · bluealsa · bluealsa-aplay");
        assert!(g.has_unit("bluetooth") && g.has_unit("bluetooth.service"));
        assert_eq!(
            g.canonical_units(),
            [
                "bluetooth.service",
                "bluealsa.service",
                "bluealsa-aplay.service"
            ]
        );
        // The daemon and the portal are essential whatever the file says.
        let f = WebConfig::parse("[[appliance.service]]\nunit = \"tessera-web.service\"\nlabel = \"portal\"\nessential = false\n[[appliance.service]]\nunits = [\"x\", \"tesserad\"]\nlabel = \"d\"\n").unwrap().appliance;
        assert!(f.services.iter().all(|s| s.essential));
        assert!(is_own_unit("tessera-web") && !is_own_unit("tesserad"));
        assert!(c.camilladsp.enabled);
        assert_eq!(c.camilladsp.websocket, "ws://127.0.0.1:1234");
        assert_eq!(
            c.camilladsp.configs_dir,
            PathBuf::from("/opt/camilladsp/configs")
        );
        assert_eq!(c.camilladsp.gain_filter, "dashboard_signal_boost");
        assert_eq!(
            c.camilladsp.gui_url.as_deref(),
            Some("http://speaker.example:5005/gui/index.html")
        );
        assert_eq!(c.camilladsp.max_upload_bytes, 1024);
        // The whole table is optional and defaults to off.
        let c = WebConfig::parse("[appliance]\n").unwrap().appliance;
        assert!(!c.enabled);
        assert_eq!(c.camilladsp, CamillaDspConfig::default());
        assert!(c.volume.is_none());
    }

    #[test]
    fn volume_table_is_optional_and_validated() {
        let v = WebConfig::parse("[appliance.volume]\ncard = \"DigiAMP\"\n")
            .unwrap()
            .appliance
            .volume
            .unwrap();
        assert_eq!(v.card, "DigiAMP");
        assert_eq!(v.control, "Digital");
        assert_eq!((v.min_db, v.max_db), (-60.0, 0.0));
        assert_eq!(
            v.state_file,
            std::path::Path::new("/var/lib/tessera-web/volume.json")
        );
        let v = WebConfig::parse(
            "[appliance.volume]\ncard = \"0\"\ncontrol = \"PCM Playback\"\nmin_db = -40\nmax_db = -3.5\nstate_file = \"/srv/volume.json\"\n",
        )
        .unwrap()
        .appliance
        .volume
        .unwrap();
        assert_eq!(v.control, "PCM Playback");
        assert_eq!((v.min_db, v.max_db), (-40.0, -3.5));
        assert_eq!(v.state_file, std::path::Path::new("/srv/volume.json"));
        for bad in ["volume.json", "/", ""] {
            let e = err(&format!(
                "[appliance.volume]\ncard = \"x\"\nstate_file = {bad:?}\n"
            ));
            assert!(e.starts_with("`appliance.volume.state_file`"), "{bad}: {e}");
        }
        let e = err("[appliance.volume]\ncontrol = \"Digital\"\n");
        assert!(e.starts_with("`appliance.volume.card` is required"), "{e}");
        let e = err("[appliance.volume]\ncard = \"a b\"\n");
        assert!(e.starts_with("`appliance.volume.card`"), "{e}");
        let e = err("[appliance.volume]\ncard = \"x\"\ncontrol = \"-q\"\n");
        assert!(e.starts_with("`appliance.volume.control`"), "{e}");
        let e = err("[appliance.volume]\ncard = \"x\"\nmin_db = 0\nmax_db = 0\n");
        assert!(e.contains("`min_db` must be below `max_db`"), "{e}");
        let e = err("[appliance.volume]\ncard = \"x\"\ncolour = 1\n");
        assert!(e.contains("colour"), "{e}");
        assert!(valid_control_name("PCM") && valid_control_name("PCM Playback"));
        for bad in ["", " x", "-x", "a\tb", "é"] {
            assert!(!valid_control_name(bad), "{bad:?}");
        }
    }

    fn err(toml: &str) -> String {
        WebConfig::parse(toml).unwrap_err()
    }

    #[test]
    fn service_entries_are_validated_and_errors_name_the_key() {
        let e = err("[[appliance.service]]\nlabel = \"x\"\nunit = \"a\"\nunits = [\"b\"]\n");
        assert!(
            e.contains("`appliance.service[0]`") && e.contains("not both"),
            "{e}"
        );
        let e = err("[[appliance.service]]\nlabel = \"x\"\n");
        assert!(
            e.contains("`appliance.service[0]`") && e.contains("`unit` or `units`"),
            "{e}"
        );
        let e = err("[[appliance.service]]\nlabel = \"x\"\nunits = []\n");
        assert!(e.contains("`appliance.service[0].units`"), "{e}");
        let e = err("[[appliance.service]]\nunit = \"a\"\n");
        assert_eq!(e, "`appliance.service[0].label` is required");
        let e = err("[[appliance.service]]\nunit = \"a\"\nlabel = \"  \"\n");
        assert_eq!(e, "`appliance.service[0].label` is required");
        let e = err(
            "[[appliance.service]]\nunit = \"a\"\nlabel = \"a\"\n[[appliance.service]]\nunit = \"-rf\"\nlabel = \"b\"\n",
        );
        assert!(
            e.starts_with("`appliance.service[1].unit`: `-rf` is not a service name"),
            "{e}"
        );
        let e = err("[[appliance.service]]\nunits = [\"a\", \"b c\"]\nlabel = \"a\"\n");
        assert!(
            e.starts_with("`appliance.service[0].units`: `b c` is not a service name"),
            "{e}"
        );
        let e = err("[[appliance.service]]\nunit = \"a.timer\"\nlabel = \"a\"\n");
        assert!(
            e.starts_with("`appliance.service[0].unit`: `a.timer` is not a service name"),
            "{e}"
        );
        // `a` and `a.service` are the same unit.
        let e = err(
            "[[appliance.service]]\nunit = \"a\"\nlabel = \"a\"\n[[appliance.service]]\nunit = \"a.service\"\nlabel = \"b\"\n",
        );
        assert_eq!(
            e,
            "`appliance.service[1].unit`: `a.service` is already in `appliance.service[0]`"
        );
        let e = err("[[appliance.service]]\nunits = [\"a.service\", \"a\"]\nlabel = \"a\"\n");
        assert_eq!(
            e,
            "`appliance.service[0].units`: `a.service` is listed twice"
        );
        let e = err(
            "[[appliance.service]]\nunit = \"a\"\nlabel = \"a\"\n[[appliance.service]]\nunits = [\"b\", \"a\"]\nlabel = \"b\"\n",
        );
        assert_eq!(
            e,
            "`appliance.service[1].units`: `a` is already in `appliance.service[0]`"
        );
        let e = err("[[appliance.service]]\nunits = [\"a\", \"a\"]\nlabel = \"a\"\n");
        assert_eq!(e, "`appliance.service[0].units`: `a` is listed twice");
        let e = err("[[appliance.service]]\nunit = \"a\"\nlabel = \"a\"\ncolour = 1\n");
        assert!(e.contains("colour"), "{e}");
        let e = err("[appliance.camilladsp]\nwebsocket = \"wss://x\"\n");
        assert!(e.starts_with("`appliance.camilladsp.websocket`"), "{e}");
        let e = err("[appliance.camilladsp]\ngui_url = \"speaker.example\"\n");
        assert!(e.starts_with("`appliance.camilladsp.gui_url`"), "{e}");
        // `{host}`: once, as the host only.
        for bad in [
            "http://speaker.example/{host}/gui/",
            "http://{host}:5005/gui/?h={host}",
            "http://{host}:5005/{gui}",
            "http://{hos}:5005/",
            "http://speaker.example:5005/}{",
            "http://{host}.{host}:5005/",
        ] {
            let e = err(&format!("[appliance.camilladsp]\ngui_url = \"{bad}\"\n"));
            assert!(
                e.starts_with("`appliance.camilladsp.gui_url`"),
                "{bad}: {e}"
            );
        }
        for ok in [
            "http://{host}:5005/gui/index.html",
            "https://{host}/",
            "http://{host}",
            "http://{host}?x=1",
            "http://user@{host}:5005/gui/index.html",
            "http://speaker.example:5005/gui/index.html",
        ] {
            let c = WebConfig::parse(&format!("[appliance.camilladsp]\ngui_url = \"{ok}\"\n"))
                .unwrap_or_else(|e| panic!("{ok}: {e}"));
            assert_eq!(c.appliance.camilladsp.gui_url.as_deref(), Some(ok));
        }
        let e = err("[appliance.camilladsp]\nconfigs_dir = \"configs\"\n");
        assert!(
            e.starts_with("`appliance.camilladsp.configs_dir` must be an absolute"),
            "{e}"
        );
        let e = err("[appliance.camilladsp]\nbinary = \"./camilladsp\"\n");
        assert!(e.starts_with("`appliance.camilladsp.binary`"), "{e}");
        let e = err("[appliance.camilladsp]\nmax_upload_bytes = 0\n");
        assert!(
            e.starts_with("`appliance.camilladsp.max_upload_bytes`"),
            "{e}"
        );
    }

    /// The shipped example: every commented key uncommented (prose comments
    /// start with `# `; commented keys and tables with `#` and a letter or
    /// `[`) must parse.
    #[test]
    fn the_shipped_example_parses_when_uncommented() {
        let text = include_str!("../dist/web.toml")
            .lines()
            .map(|l| match l.strip_prefix('#') {
                Some(rest) if rest.starts_with(|c: char| c.is_ascii_alphabetic() || c == '[') => {
                    rest
                }
                _ => l,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let c = WebConfig::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert!(!c.appliance.enabled, "the example keeps appliance mode off");
        assert_eq!(c.appliance.services.len(), 5);
        assert!(c.appliance.services[0].essential);
        assert!(c.appliance.services[4].is_group());
        assert!(c.appliance.camilladsp.gui_url.is_some());
    }

    #[test]
    fn unit_names() {
        for ok in [
            "tesserad",
            "bluealsa-aplay",
            "getty@tty1.service",
            "a_b.service",
        ] {
            assert!(valid_unit_name(ok), "{ok}");
        }
        for bad in [
            "",
            "-x",
            ".hidden",
            "a b",
            "a,b",
            "a*",
            "a:b",
            "a/b",
            "a\tb",
            "a.timer",
            "a.socket",
            ".service",
            "a.b.service",
            "@tty1",
            "a@b@c",
            "x@",
            "x@.service",
        ] {
            assert!(!valid_unit_name(bad), "{bad:?}");
        }
    }

    /// A scratch directory for drop-in tests, removed on drop.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("tessera-web-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("web.d")).unwrap();
            Self(dir)
        }
        fn write(&self, rel: &str, text: &str) {
            std::fs::write(self.0.join(rel), text).unwrap();
        }
        fn load(&self) -> Result<WebConfig, String> {
            WebConfig::load(&self.0.join("web.toml"))
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn dropins_apply_in_name_order_and_later_files_win() {
        let s = Scratch::new("order");
        s.write(
            "web.toml",
            "listen = \"0.0.0.0:8090\"\ncookie_secure = true\n",
        );
        s.write("web.d/50-b.toml", "listen = \"0.0.0.0:9000\"\n");
        s.write(
            "web.d/10-a.toml",
            "listen = \"0.0.0.0:7000\"\nsession_idle_secs = 60\n",
        );
        let c = s.load().unwrap();
        assert_eq!(
            c.listen.port(),
            9000,
            "50-b is applied after 10-a and after web.toml"
        );
        assert_eq!(c.session_idle, Duration::from_secs(60));
        assert!(
            c.cookie_secure,
            "keys no drop-in sets stay as web.toml has them"
        );
    }

    #[test]
    fn dropin_tables_merge_but_service_lists_are_replaced() {
        let s = Scratch::new("merge");
        s.write(
            "web.toml",
            "[appliance]\nhostname_label = \"base\"\n\n[[appliance.service]]\nunit = \"tesserad\"\nlabel = \"Network audio\"\n",
        );
        s.write(
            "web.d/60-speaker.toml",
            "[appliance]\nenabled = true\n\n[[appliance.service]]\nunit = \"camilladsp\"\nlabel = \"EQ engine\"\n\n[appliance.camilladsp]\nenabled = true\n",
        );
        let c = s.load().unwrap();
        assert!(c.appliance.enabled);
        assert_eq!(
            c.appliance.hostname_label.as_deref(),
            Some("base"),
            "tables merge key by key"
        );
        assert_eq!(
            c.appliance.services.len(),
            1,
            "a list in a later file replaces the whole list"
        );
        assert!(c.appliance.services[0].has_unit("camilladsp"));
        assert!(c.appliance.camilladsp.enabled);
    }

    #[test]
    fn dropins_work_without_a_base_file_and_ignore_other_names() {
        let s = Scratch::new("nobase");
        s.write("web.d/20-x.toml", "listen = \"0.0.0.0:8123\"\n");
        s.write("web.d/.hidden.toml", "this is not toml");
        s.write("web.d/notes.txt", "neither is this");
        s.write("web.d/30-x.toml.bak", "listen = \"0.0.0.0:1\"\n");
        assert_eq!(s.load().unwrap().listen.port(), 8123);
    }

    #[test]
    fn no_dropin_directory_or_files_keeps_the_old_behaviour() {
        let s = Scratch::new("none");
        std::fs::remove_dir(s.0.join("web.d")).unwrap();
        assert_eq!(s.load().unwrap(), WebConfig::default());
        s.write("web.toml", "listen = \"0.0.0.0:8443\"\n");
        assert_eq!(s.load().unwrap().listen.port(), 8443);
        assert!(s.load().is_ok());
    }

    #[test]
    fn a_bad_dropin_is_refused_with_its_name() {
        let s = Scratch::new("bad");
        s.write("web.toml", "listen = \"0.0.0.0:8090\"\n");
        s.write("web.d/10-broken.toml", "listen = [");
        let e = s.load().unwrap_err();
        assert!(e.contains("10-broken.toml"), "{e}");
        s.write("web.d/10-broken.toml", "listn = \"0.0.0.0:1\"\n");
        assert!(s.load().is_err(), "an unknown key in a drop-in is an error");
        s.write("web.d/10-broken.toml", "session_idle_secs = 0\n");
        assert!(s.load().is_err(), "the merged result is validated");
    }

    #[test]
    fn the_bundles_speaker_dropin_is_a_valid_portal_configuration() {
        // The installer appends the volume table when the card has a mixer control;
        // both forms must load, on top of an empty web.toml.
        let template =
            include_str!("../../../packaging/bundle/profiles/speaker/web.d/60-speaker.toml.in");
        let volume = "\n[appliance.volume]\ncard = \"DigiAMP\"\ncontrol = \"Digital\"\nmin_db = -60\nmax_db = 0\n";
        for text in [template.to_owned(), format!("{template}{volume}")] {
            let s = Scratch::new("bundle-dropin");
            s.write(
                "web.toml",
                "# every key commented out, as the package ships it\n",
            );
            s.write("web.d/60-speaker.toml", &text);
            let c = s.load().unwrap();
            assert!(c.appliance.enabled);
            assert!(c.appliance.camilladsp.enabled);
            // The vendored engine is in the root-owned tree, not the writable data directory.
            assert_eq!(
                c.appliance.camilladsp.binary,
                Path::new("/usr/lib/tessera-dsp/camilladsp")
            );
            assert_eq!(
                c.appliance.camilladsp.gui_url.as_deref(),
                Some("http://{host}:5005/gui/index.html")
            );
            for unit in ["tesserad", "tessera-web", "camilladsp", "avahi-daemon"] {
                assert!(
                    c.appliance
                        .services
                        .iter()
                        .any(|entry| entry.has_unit(unit)),
                    "{unit}"
                );
            }
            assert_eq!(
                c.appliance.volume.is_some(),
                text.contains("appliance.volume")
            );
        }
    }

    #[test]
    fn dropins_lists_only_regular_toml_files_sorted() {
        let s = Scratch::new("list");
        s.write("web.d/b.toml", "");
        s.write("web.d/a.toml", "");
        std::fs::create_dir(s.0.join("web.d/dir.toml")).unwrap();
        let e = dropins(&s.0.join("web.toml")).unwrap_err();
        assert!(e.contains("not a regular file"), "{e}");
        std::fs::remove_dir(s.0.join("web.d/dir.toml")).unwrap();
        let names: Vec<_> = dropins(&s.0.join("web.toml"))
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.toml", "b.toml"]);
    }
}
