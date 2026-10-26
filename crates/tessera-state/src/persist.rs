//! The machine-owned `state.toml`: schema, versioning, atomic writes,
//! recovery from a missing or corrupt file, and the write debouncer.
//!
//! `state.toml` holds **operator intent that changed at runtime** (names,
//! labels, subscriptions, rate/encoding/latency, clock role and priority,
//! controller flows) and the generated per-device identity. It holds nothing
//! derived. Unknown keys inside a known schema version are ignored on load
//! (and lost on the next write); a file from a *newer* schema version is never
//! overwritten, it is moved aside (see [`load_state`]).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tessera_control_types::{ClockRole, FlowSpec};
use toml::{Table, Value};

use crate::error::StateError;

/// The schema version this build writes.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// One schema migration step: upgrades a table from version `n` to `n + 1`
/// in place, where `n` is the step's index plus one.
pub type Migration = fn(&mut Table) -> Result<(), String>;

/// The migrations from older versions. Version 1 is the first schema, so
/// there are none yet; the length of this list plus one must equal
/// [`CURRENT_SCHEMA_VERSION`].
pub const MIGRATIONS: &[Migration] = &[];

const HEADER: &str = "# managed by tesserad - do not edit while the daemon is running\n";

fn default_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

/// A persisted subscription: stored by name, never by resolved address
/// (ADR 0008).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionIntent {
    /// Local RX channel (1-based).
    pub rx_channel: u16,
    /// Name of the TX channel on the remote device.
    pub tx_channel: String,
    /// Name of the remote device.
    pub tx_device: String,
}

/// Persisted values of one device.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DeviceIntent {
    /// 16 hex digits; generated once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_device_id: Option<String>,
    /// Device name set at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Sample rate set at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    /// Encoding set at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<u8>,
    /// Latency set at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_us: Option<u32>,
    /// RX channel count set at runtime (`SetDeviceChannels`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rx_channels: Option<u16>,
    /// TX channel count set at runtime (`SetDeviceChannels`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_channels: Option<u16>,
    /// RX labels by channel number written as a string key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rx_channel_labels: BTreeMap<String, String>,
    /// TX labels by channel number written as a string key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tx_channel_labels: BTreeMap<String, String>,
    /// Subscriptions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscription: Vec<SubscriptionIntent>,
}

/// Persisted clock values.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClockIntent {
    /// Role set at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<ClockRole>,
    /// Priority set at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,
}

impl ClockIntent {
    fn is_empty(&self) -> bool {
        self.role.is_none() && self.priority.is_none()
    }
}

/// Persisted network values (ADR 0012).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NetworkIntent {
    /// Primary interface selected at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
}

impl NetworkIntent {
    fn is_empty(&self) -> bool {
        self.primary.is_none()
    }
}

/// Contents of `state.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateFile {
    /// Schema version (a missing key is read as version 1).
    #[serde(default = "default_version")]
    pub schema_version: u32,
    /// Complete controller flow intent. `None` permits legacy-file fallback;
    /// `Some([])` records an explicit clear and never resurrects that file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_flows: Option<Vec<FlowSpec>>,
    /// `[clock]`.
    #[serde(default, skip_serializing_if = "ClockIntent::is_empty")]
    pub clock: ClockIntent,
    /// `[network]`.
    #[serde(default, skip_serializing_if = "NetworkIntent::is_empty")]
    pub network: NetworkIntent,
    /// `[device.<key>]`, keyed by the config `device.id`.
    #[serde(default)]
    pub device: BTreeMap<String, DeviceIntent>,
}

impl Default for StateFile {
    fn default() -> Self {
        Self {
            protocol_flows: None,
            schema_version: CURRENT_SCHEMA_VERSION,
            clock: ClockIntent::default(),
            network: NetworkIntent::default(),
            device: BTreeMap::new(),
        }
    }
}

impl StateFile {
    /// Render the file content, including the header comment.
    ///
    /// # Errors
    /// [`StateError::Serialize`] (a bug in this crate).
    pub fn render(&self) -> Result<String, StateError> {
        let body = toml::to_string(self).map_err(|e| StateError::Serialize(e.to_string()))?;
        Ok(format!("{HEADER}{body}"))
    }

    /// Parse file content (after migration). Mostly for tests; use
    /// [`load_state`] for the recovery behaviour.
    ///
    /// # Errors
    /// The parser's message.
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }
}

/// Why migration did not produce a current table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrateFailure {
    /// `schema_version` is not a positive integer, or a step failed.
    Corrupt(String),
    /// The file is from a newer version than this build understands.
    Newer(u32),
}

/// Bring `table` to the current version using `migrations` (index `i`
/// upgrades version `i + 1` to `i + 2`). Returns the version it started at if
/// anything was migrated.
///
/// # Errors
/// [`MigrateFailure`].
pub fn migrate_table(
    table: &mut Table,
    migrations: &[Migration],
) -> Result<Option<u32>, MigrateFailure> {
    let current = u32::try_from(migrations.len()).unwrap_or(u32::MAX - 1) + 1;
    let from = match table.get("schema_version") {
        None => 1,
        Some(Value::Integer(i)) => match u32::try_from(*i) {
            Ok(n) if n >= 1 => n,
            _ => {
                return Err(MigrateFailure::Corrupt(format!(
                    "schema_version {i} is not a positive version number"
                )));
            }
        },
        Some(other) => {
            return Err(MigrateFailure::Corrupt(format!(
                "schema_version must be an integer, found {}",
                other.type_str()
            )));
        }
    };
    if from > current {
        return Err(MigrateFailure::Newer(from));
    }
    for v in from..current {
        let Some(step) = migrations.get((v - 1) as usize) else {
            return Err(MigrateFailure::Corrupt(format!(
                "no migration from version {v}"
            )));
        };
        step(table)
            .map_err(|e| MigrateFailure::Corrupt(format!("migration from version {v}: {e}")))?;
    }
    table.insert("schema_version".into(), Value::Integer(i64::from(current)));
    Ok((from < current).then_some(from))
}

/// What happened when the state file was loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateLoad {
    /// There was no file; starting empty.
    Missing,
    /// Loaded; `migrated_from` is set if it was upgraded from an older schema.
    Loaded {
        /// The old version, if a migration ran.
        migrated_from: Option<u32>,
    },
    /// The file was unusable (corrupt, or from a newer schema). It was moved
    /// to `backup`; the daemon starts with empty state.
    Recovered {
        /// Where the old file now lives.
        backup: PathBuf,
        /// Why it was set aside.
        reason: String,
    },
}

fn io_err(action: &'static str, path: &Path, source: std::io::Error) -> StateError {
    StateError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}

/// Move `path` to `<path>.<kind>-<n>` with the smallest unused `n`.
fn set_aside(path: &Path, kind: &str) -> Result<PathBuf, StateError> {
    let name = path.file_name().map_or_else(
        || "state.toml".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    for n in 1u32..=u32::MAX {
        let candidate = path.with_file_name(format!("{name}.{kind}-{n}"));
        if candidate.exists() {
            continue;
        }
        std::fs::rename(path, &candidate).map_err(|e| io_err("moving aside", path, e))?;
        sync_dir_of(&candidate);
        return Ok(candidate);
    }
    Err(io_err(
        "moving aside",
        path,
        std::io::Error::other("no free backup name"),
    ))
}

/// Load `state.toml`.
///
/// * missing file: empty state ([`StateLoad::Missing`]);
/// * unreadable for any reason other than "not found": an error, nothing is
///   touched;
/// * not UTF-8, not TOML, wrong shape, bad `schema_version`, or a failing
///   migration: the file is renamed to `state.toml.corrupt-N`, empty state is
///   returned ([`StateLoad::Recovered`]);
/// * schema newer than [`CURRENT_SCHEMA_VERSION`]: renamed to
///   `state.toml.newer-N` and treated the same way, so a downgrade never
///   destroys data written by a newer build.
///
/// Starting empty means new device identities are generated; callers should
/// surface that.
///
/// # Errors
/// [`StateError::Io`] when the file cannot be read or set aside.
pub fn load_state(path: &Path) -> Result<(StateFile, StateLoad), StateError> {
    load_state_with(path, MIGRATIONS)
}

pub(crate) fn load_state_with(
    path: &Path,
    migrations: &[Migration],
) -> Result<(StateFile, StateLoad), StateError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((StateFile::default(), StateLoad::Missing));
        }
        Err(e) => return Err(io_err("reading", path, e)),
    };
    let recover = |kind: &str, reason: String| -> Result<(StateFile, StateLoad), StateError> {
        let backup = set_aside(path, kind)?;
        Ok((
            StateFile::default(),
            StateLoad::Recovered { backup, reason },
        ))
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return recover("corrupt", "not valid UTF-8".into());
    };
    let mut table = match text.parse::<Table>() {
        Ok(t) => t,
        Err(e) => return recover("corrupt", format!("not valid TOML: {}", e.message())),
    };
    let migrated_from = match migrate_table(&mut table, migrations) {
        Ok(m) => m,
        Err(MigrateFailure::Corrupt(r)) => return recover("corrupt", r),
        Err(MigrateFailure::Newer(v)) => {
            return recover(
                "newer",
                format!("schema version {v} is newer than the supported {CURRENT_SCHEMA_VERSION}"),
            );
        }
    };
    match StateFile::deserialize(Value::Table(table.clone())) {
        Ok(s) => Ok((s, StateLoad::Loaded { migrated_from })),
        Err(e) => {
            // One wrongly typed field must not cost every device its network
            // identity: keep what still parses, device by device, and set the
            // original file aside as usual.
            let (salvaged, devices) = salvage(&table);
            let backup = set_aside(path, "corrupt")?;
            Ok((
                salvaged,
                StateLoad::Recovered {
                    backup,
                    reason: format!(
                        "unexpected structure: {e}; salvaged {devices} device section(s) (complete or identity only)"
                    ),
                },
            ))
        }
    }
}

/// Rebuild as much state as possible from a table that does not fit the
/// schema as a whole. Per device: the whole section if it parses, else just
/// its `wire_device_id` (if that is a string). Clock, network and controller
/// flow sections are kept if they parse. A malformed controller section clears
/// that set explicitly so recovery cannot resurrect the legacy file. Returns
/// the state and the number of devices kept.
fn salvage(table: &Table) -> (StateFile, usize) {
    let mut out = StateFile::default();
    if let Some(flows) = table.get("protocol_flows") {
        out.protocol_flows = Some(Vec::<FlowSpec>::deserialize(flows.clone()).unwrap_or_default());
    }
    if let Some(c) = table.get("clock") {
        if let Ok(c) = ClockIntent::deserialize(c.clone()) {
            out.clock = c;
        }
    }
    if let Some(n) = table.get("network") {
        if let Ok(n) = NetworkIntent::deserialize(n.clone()) {
            out.network = n;
        }
    }
    if let Some(Value::Table(devs)) = table.get("device") {
        for (key, v) in devs {
            let whole = DeviceIntent::deserialize(v.clone()).ok();
            let intent = whole.or_else(|| {
                let id = v.get("wire_device_id")?.as_str()?.to_owned();
                Some(DeviceIntent {
                    wire_device_id: Some(id),
                    ..DeviceIntent::default()
                })
            });
            if let Some(i) = intent {
                out.device.insert(key.clone(), i);
            }
        }
    }
    let n = out.device.len();
    (out, n)
}

/// Remove temporary files left behind by an interrupted [`write_atomic`] of
/// `path` (named `.<file>.tmp-*` in the same directory). Call at start-up,
/// when no writer can be active. Returns how many were removed; failures are
/// ignored (best effort).
pub fn remove_stale_temporaries(path: &Path) -> usize {
    let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) else {
        return 0;
    };
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return 0;
    };
    let prefix = format!(".{name}.tmp-");
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut n = 0;
    for e in rd.flatten() {
        let is_stale = e.file_name().to_string_lossy().starts_with(&prefix);
        if is_stale && std::fs::remove_file(e.path()).is_ok() {
            n += 1;
        }
    }
    n
}

fn sync_dir_of(path: &Path) {
    if let Some(dir) = path.parent() {
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Replace `path` with `contents` atomically: write a temporary file in the
/// same directory, `fsync` it, rename it over the target, `fsync` the
/// directory. A crash leaves either the old or the new file, never a mix.
/// The file gets mode 0640 exactly (regardless of umask). A missing parent
/// directory is created with mode 0750 (subject to the umask, which can only
/// remove bits).
///
/// This does blocking file I/O; call it from a blocking context, not an async
/// executor thread.
///
/// # Errors
/// [`StateError::Io`]; the temporary file is removed on failure.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), StateError> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    if !dir.exists() {
        std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o750)
            .create(dir)
            .map_err(|e| io_err("creating directory", dir, e))?;
    }
    let name = path
        .file_name()
        .map_or_else(|| "state".to_owned(), |n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<(), StateError> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o640)
            .open(&tmp)
            .map_err(|e| io_err("creating", &tmp, e))?;
        // The creation mode is subject to the umask; make the result exact.
        f.set_permissions(std::fs::Permissions::from_mode(0o640))
            .map_err(|e| io_err("setting permissions on", &tmp, e))?;
        f.write_all(contents)
            .map_err(|e| io_err("writing", &tmp, e))?;
        f.sync_all().map_err(|e| io_err("syncing", &tmp, e))?;
        drop(f);
        std::fs::rename(&tmp, path).map_err(|e| io_err("renaming over", path, e))?;
        let d = std::fs::File::open(dir).map_err(|e| io_err("opening directory", dir, e))?;
        d.sync_all()
            .map_err(|e| io_err("syncing directory", dir, e))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Coalesces bursts of changes into one write. Sans-IO: time is passed in as
/// a monotonic [`Duration`] since any fixed origin.
///
/// The first change opens a window of `delay`; further changes inside the
/// window join it and do **not** extend it, so a continuous stream of changes
/// still reaches disk at most `delay` after the first unsaved change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Debouncer {
    delay: Duration,
    due: Option<Duration>,
}

impl Debouncer {
    /// A debouncer with the given window.
    #[must_use]
    pub fn new(delay: Duration) -> Self {
        Self { delay, due: None }
    }

    /// Change the window. A window that is already open keeps its deadline;
    /// the new length applies from the next change on.
    pub fn set_delay(&mut self, delay: Duration) {
        self.delay = delay;
    }

    /// Record that unsaved changes exist.
    pub fn mark_dirty(&mut self, now: Duration) {
        if self.due.is_none() {
            self.due = Some(now.saturating_add(self.delay));
        }
    }

    /// True (once) when the window has elapsed; the caller must then write.
    pub fn poll(&mut self, now: Duration) -> bool {
        match self.due {
            Some(d) if now >= d => {
                self.due = None;
                true
            }
            _ => false,
        }
    }

    /// When [`poll`](Self::poll) will next return true, if changes are pending.
    #[must_use]
    pub fn deadline(&self) -> Option<Duration> {
        self.due
    }

    /// Are there unsaved changes?
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.due.is_some()
    }

    /// Take the pending flag regardless of time (shutdown flush). Returns
    /// whether a write is needed.
    pub fn take(&mut self) -> bool {
        self.due.take().is_some()
    }
}
