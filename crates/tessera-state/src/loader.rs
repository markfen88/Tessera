//! Loading `config.toml` plus `config.d/*.toml`: merge in lexical order with
//! provenance, extract, validate.
//!
//! Merge rules (`docs/configuration.md`): later files override earlier ones;
//! **tables merge key by key, scalars and arrays replace**, with two
//! exceptions for the array-of-tables keys:
//!
//! * `[[device]]` entries merge **by `id`**. An entry whose `id` exists in an
//!   earlier file overrides only the keys it sets (tables inside it, such as
//!   the label tables and `multicast`, merge key by key too) and *extends*
//!   its `locked` list; an entry with a new `id` is appended. The device order
//!   is the order of the first definition of each `id`. A drop-in cannot
//!   remove a device. Two entries with the same `id` inside one file are not
//!   merged with each other: validation reports the duplicate.
//! * `[[bridge]]` entries have no key, so a drop-in that contains `[[bridge]]`
//!   replaces the whole bridge list (with a warning).
//!
//! `[clock] locked` and `[network] locked` combine like a device's `locked` list (set union, each
//! element's provenance names the file that added it), so a later file cannot
//! silently drop a lock. Tables outside arrays, inline or not, merge; see
//! [`merge_tables`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use toml::{Table, Value};

use crate::config::{Config, extract, join_path};
use crate::env::Environment;
use crate::error::{ConfigError, Issue};
use crate::validate::validate;

/// Where the configuration lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    /// The main file (`/etc/tessera/config.toml`). A missing file is the same
    /// as an empty one.
    pub main: PathBuf,
    /// The drop-in directory (`/etc/tessera/config.d`). A missing directory
    /// is the same as an empty one. Only regular files ending in `.toml`
    /// whose names do not start with `.` are read.
    pub dropin_dir: PathBuf,
}

impl ConfigPaths {
    /// Explicit paths (tests use temporary directories).
    #[must_use]
    pub fn new(main: impl Into<PathBuf>, dropin_dir: impl Into<PathBuf>) -> Self {
        Self {
            main: main.into(),
            dropin_dir: dropin_dir.into(),
        }
    }

    /// The system locations.
    #[must_use]
    pub fn system() -> Self {
        Self::new("/etc/tessera/config.toml", "/etc/tessera/config.d")
    }
}

/// One configuration file's text, already read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Where it came from (used in messages and provenance).
    pub path: PathBuf,
    /// Its content.
    pub text: String,
}

impl Source {
    /// Build a source from inline text (golden tests).
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, text: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            text: text.into(),
        }
    }
}

/// Which file set which key.
///
/// Keys are leaf key paths such as `daemon.log_level`, `network.interfaces[0]`
/// or `device[1].rx_channel_labels.5`. Keys that no file set are absent
/// (built-in defaults). When an array is replaced, the entries of the earlier
/// array are forgotten, so provenance always names the file whose value is in
/// effect. Device entries are numbered in merged order (`device[0]` is the
/// first device of the base file), so a drop-in key that overrides a base
/// device is reported under the base device's index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    map: BTreeMap<String, PathBuf>,
}

impl Provenance {
    /// The file that set `path`. If `path` is not a leaf, the file that set
    /// something under it, or failing that the nearest ancestor, is returned.
    #[must_use]
    pub fn source_of(&self, path: &str) -> Option<&Path> {
        if let Some(p) = self.map.get(path) {
            return Some(p);
        }
        if let Some((_, p)) = self
            .map
            .range(path.to_owned()..)
            .take_while(|(k, _)| k.starts_with(path))
            .find(|(k, _)| matches!(k.as_bytes().get(path.len()), Some(b'.' | b'[')))
        {
            return Some(p);
        }
        // Walk up through ancestors that were set as a whole (exact match
        // only: a sibling's entries say nothing about this path).
        let mut rest = path;
        while let Some(cut) = rest.rfind(['.', '[']) {
            rest = &rest[..cut];
            if let Some(p) = self.map.get(rest) {
                return Some(p);
            }
        }
        None
    }

    /// Every leaf key with its source, in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Path)> {
        self.map.iter().map(|(k, v)| (k.as_str(), v.as_path()))
    }

    /// Number of keys with a known source.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True if no file set anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn forget_under(&mut self, prefix: &str) {
        self.map.retain(|k, _| {
            !(k == prefix
                || (k.starts_with(prefix)
                    && matches!(k.as_bytes().get(prefix.len()), Some(b'.' | b'['))))
        });
    }

    fn record_leaves(&mut self, v: &Value, path: &str, file: &Path) {
        match v {
            Value::Table(t) if !t.is_empty() => {
                for (k, x) in t {
                    self.record_leaves(x, &join_path(path, k), file);
                }
            }
            Value::Array(a) if !a.is_empty() => {
                for (i, x) in a.iter().enumerate() {
                    self.record_leaves(x, &format!("{path}[{i}]"), file);
                }
            }
            _ => {
                self.map.insert(path.to_owned(), file.to_path_buf());
            }
        }
    }
}

/// Merge `over` into `base`: tables merge recursively, anything else
/// (scalars, arrays, a table replacing a non-table) replaces. The root
/// `device` array merges by `id` (see the module documentation).
pub fn merge_tables(base: &mut Table, over: Table) {
    let mut prov = Provenance::default();
    merge_into(base, over, "", Path::new(""), &mut prov);
}

fn merge_into(base: &mut Table, over: Table, path: &str, file: &Path, prov: &mut Provenance) {
    for (k, v) in over {
        let p = join_path(path, &k);
        let both_tables =
            matches!(base.get(&k), Some(Value::Table(_))) && matches!(v, Value::Table(_));
        if both_tables {
            if let (Some(Value::Table(b)), Value::Table(o)) = (base.get_mut(&k), v) {
                merge_into(b, o, &p, file, prov);
            }
        } else if (path == "clock" || path == "network")
            && k == "locked"
            && both_arrays(base.get(&k), &v)
        {
            if let (Some(Value::Array(b)), Value::Array(o)) = (base.get_mut(&k), v) {
                extend_set(b, o, &p, file, prov);
            }
        } else if path.is_empty() && k == "device" && device_arrays(base.get(&k), &v) {
            if let (Some(Value::Array(b)), Value::Array(o)) = (base.get_mut(&k), v) {
                merge_devices(b, o, file, prov);
            }
        } else {
            prov.forget_under(&p);
            prov.record_leaves(&v, &p, file);
            base.insert(k, v);
        }
    }
}

fn both_arrays(base: Option<&Value>, over: &Value) -> bool {
    matches!((base, over), (Some(Value::Array(_)), Value::Array(_)))
}

/// Append the items of `extra` that `old` does not contain (set union, order
/// kept). Each appended element's provenance names `file`; elements already
/// present keep theirs.
fn extend_set(
    old: &mut Vec<Value>,
    extra: Vec<Value>,
    path: &str,
    file: &Path,
    prov: &mut Provenance,
) {
    if old.is_empty() && !extra.is_empty() {
        // The list as a whole (set by an earlier file, possibly as `[]`) is
        // now described by its elements.
        prov.forget_under(path);
    }
    for item in extra {
        if !old.contains(&item) {
            prov.record_leaves(&item, &format!("{path}[{}]", old.len()), file);
            old.push(item);
        }
    }
}

fn entry_id(v: &Value) -> Option<&str> {
    v.get("id").and_then(Value::as_str)
}

/// True if both sides are arrays made only of tables.
fn device_arrays(base: Option<&Value>, over: &Value) -> bool {
    let tables = |v: &Value| matches!(v, Value::Array(a) if a.iter().all(Value::is_table));
    base.is_some_and(tables) && tables(over)
}

/// Merge the `[[device]]` entries of one file into the entries of the earlier
/// files, by `id`.
fn merge_devices(base: &mut Vec<Value>, over: Vec<Value>, file: &Path, prov: &mut Provenance) {
    let earlier = base.len();
    let mut seen: Vec<String> = Vec::new();
    for v in over {
        // Only entries from earlier files are merge targets, and each is
        // merged into at most once per file; a repeated `id` within one file
        // is appended so that validation reports the duplicate.
        let target = entry_id(&v)
            .filter(|id| !seen.iter().any(|s| s == id))
            .and_then(|id| base[..earlier].iter().position(|b| entry_id(b) == Some(id)));
        if let Some(id) = entry_id(&v) {
            seen.push(id.to_owned());
        }
        match (target, v) {
            (Some(i), Value::Table(t)) => {
                merge_device_entry(&mut base[i], t, &format!("device[{i}]"), file, prov);
            }
            (_, v) => {
                prov.record_leaves(&v, &format!("device[{}]", base.len()), file);
                base.push(v);
            }
        }
    }
}

/// Merge one drop-in device entry into the entry with the same `id`: keys
/// override, tables merge, and `locked` is extended (set union, order kept).
fn merge_device_entry(
    base: &mut Value,
    mut over: Table,
    path: &str,
    file: &Path,
    prov: &mut Provenance,
) {
    let Value::Table(b) = base else { return };
    // The `id` is the key the entries were matched by; it stays attributed to
    // the file that defined the device.
    over.remove("id");
    if both_arrays(
        b.get("locked"),
        over.get("locked").unwrap_or(&Value::Boolean(false)),
    ) {
        if let (Some(Value::Array(old)), Some(Value::Array(extra))) =
            (b.get_mut("locked"), over.remove("locked"))
        {
            extend_set(old, extra, &join_path(path, "locked"), file, prov);
        }
    }
    merge_into(b, over, path, file, prov);
}

/// The provenance label of keys set through [`Overrides`].
pub const OVERRIDES_SOURCE: &str = "<command line>";

/// Overridable keys of apply class `live` and the lock an override puts on
/// them (a test checks that every such key of the schema is listed).
pub const OVERRIDE_LOCKS: &[(&str, crate::config::ClockLock)] = &[
    ("clock.role", crate::config::ClockLock::Role),
    ("clock.priority", crate::config::ClockLock::Priority),
];

/// Values that override every configuration file: what `tesserad` fills
/// from its command line (precedence: command line > `locked` config >
/// `state.toml` > `config.d/*.toml` > `config.toml` > built-in default).
///
/// Only keys of the schema's plain tables can be overridden (not entries
/// of `[[device]]`, `[[bridge]]` or `[[flow]]`). The values go through the
/// same type checks and validation as the files; errors name the key and
/// the source [`OVERRIDES_SOURCE`]. A reload applies the same overrides
/// again, so a `SIGHUP` never changes an overridden key, and
/// [`LoadedConfig::overridden`] lists them so the daemon can log each one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overrides {
    table: Table,
    keys: std::collections::BTreeSet<String>,
}

impl Overrides {
    /// No overrides.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override `path` (`section.key`, as in the schema) with `value`.
    ///
    /// # Errors
    /// The path is not a key of a plain table of the schema.
    pub fn set(&mut self, path: &str, value: impl Into<Value>) -> Result<(), String> {
        let spec = crate::schema::key(path)
            .ok_or_else(|| format!("`{path}` is not a configuration key"))?;
        if crate::schema::section(spec.section).is_some_and(|s| s.array) {
            return Err(format!(
                "`{path}` belongs to [[{}]] entries, which cannot be overridden",
                spec.section
            ));
        }
        // A live key must be locked by its override, or a run-time value
        // would win over the command line: only those in OVERRIDE_LOCKS.
        if spec.apply == crate::schema::ApplyClass::Live
            && !OVERRIDE_LOCKS.iter().any(|(p, _)| *p == path)
        {
            return Err(format!(
                "`{path}` can be changed at run time and has no lock for an override"
            ));
        }
        let mut t = &mut self.table;
        for part in spec.section.split('.') {
            let e = t
                .entry(part.to_owned())
                .or_insert_with(|| Value::Table(Table::new()));
            if !e.is_table() {
                *e = Value::Table(Table::new());
            }
            let Value::Table(next) = e else {
                return Err(format!("`{path}`: internal error"));
            };
            t = next;
        }
        t.insert(spec.key.to_owned(), value.into());
        self.keys.insert(path.to_owned());
        Ok(())
    }

    /// Override an unsigned value (`u64` does not convert into a TOML
    /// integer by itself).
    ///
    /// # Errors
    /// As [`set`](Self::set), or the value is above `i64::MAX`.
    pub fn set_uint(&mut self, path: &str, value: u64) -> Result<(), String> {
        let v = i64::try_from(value).map_err(|_| format!("`{path}`: {value} is too large"))?;
        self.set(path, v)
    }

    /// The overridden keys, in key order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.keys.iter().map(String::as_str)
    }

    /// Is `path` overridden?
    #[must_use]
    pub fn contains(&self, path: &str) -> bool {
        self.keys.contains(path)
    }

    /// No overrides at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// A successfully loaded configuration.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// Ids of the devices whose `name` was not set by any file (they got the
    /// default name, which depends on the number of devices).
    pub defaulted_names: std::collections::BTreeSet<String>,
    /// The merged, validated configuration.
    pub config: Config,
    /// Which file set which key.
    pub provenance: Provenance,
    /// Non-fatal findings.
    pub warnings: Vec<Issue>,
    /// The files that were read, in merge order.
    pub files: Vec<PathBuf>,
    /// Keys set by [`Overrides`] (the command line), whatever the files say.
    pub overridden: std::collections::BTreeSet<String>,
}

fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let mut end = offset.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let upto = &text[..end];
    let line = upto.matches('\n').count() + 1;
    let col = upto.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, col)
}

/// Merge, extract and validate already-read sources (in the given order).
///
/// # Errors
/// [`ConfigError::Invalid`] with *all* syntax errors, unknown keys, type
/// errors and validation errors found. Semantic validation runs only when
/// parsing and extraction found nothing wrong, to avoid cascades of
/// consequential errors.
pub fn load_from_sources(
    sources: &[Source],
    env: &Environment,
) -> Result<LoadedConfig, ConfigError> {
    load_from_sources_with(sources, env, &Overrides::new())
}

/// [`load_from_sources`] with [`Overrides`] applied over every file.
///
/// # Errors
/// As [`load_from_sources`]; an invalid override is reported like a key of
/// a file, with the source [`OVERRIDES_SOURCE`].
pub fn load_from_sources_with(
    sources: &[Source],
    env: &Environment,
    overrides: &Overrides,
) -> Result<LoadedConfig, ConfigError> {
    let mut issues: Vec<Issue> = Vec::new();
    let mut merged = Table::new();
    let mut prov = Provenance::default();
    let mut merge_warnings: Vec<Issue> = Vec::new();
    for s in sources {
        match s.text.parse::<Table>() {
            Ok(t) => {
                // The Controller runtime can write this reserved drop-in.
                // Enforce its scope here too, even if it bypasses its adapter.
                if s.path.file_name().is_some_and(|n| n == "90-probe-flows.toml")
                    && (t.keys().any(|key| key != "flow")
                        || t.get("flow").is_some_and(|flows| {
                            !matches!(flows, Value::Array(a) if a.iter().all(Value::is_table))
                        }))
                {
                    let mut issue = Issue::error(
                        "",
                        "reserved Controller drop-in accepts only [[flow]] entries",
                    );
                    issue.source = Some(s.path.clone());
                    issues.push(issue);
                    continue;
                }
                if let (Some(Value::Array(_)), Some(Value::Array(new))) =
                    (merged.get("device"), t.get("device"))
                {
                    for (n, d) in new.iter().enumerate() {
                        if d.is_table() && entry_id(d).is_none() {
                            let mut w = Issue::warning(
                                "device",
                                format!(
                                    "[[device]] entry {} of this file has no string `id`, so it cannot be merged with an earlier device and counts as a new one; every [[device]] entry needs an `id`",
                                    n + 1
                                ),
                            );
                            w.source = Some(s.path.clone());
                            merge_warnings.push(w);
                        }
                    }
                }
                for key in ["bridge", "flow"] {
                    if let (Some(Value::Array(old)), true) = (merged.get(key), t.contains_key(key))
                    {
                        if !old.is_empty() {
                            let earlier = prov.source_of(key).map_or_else(
                                || "an earlier file".to_owned(),
                                |p| p.display().to_string(),
                            );
                            let mut w = Issue::warning(
                                key,
                                format!(
                                    "replaces the {} [[{key}]] entr{} from {earlier}; [[{key}]] entries are not merged, so the list is replaced as a whole",
                                    old.len(),
                                    if old.len() == 1 { "y" } else { "ies" },
                                ),
                            );
                            w.source = Some(s.path.clone());
                            merge_warnings.push(w);
                        }
                    }
                }
                merge_into(&mut merged, t, "", &s.path, &mut prov);
            }
            Err(e) => {
                let loc = e.span().map(|r| line_col(&s.text, r.start));
                let msg = match loc {
                    Some((l, c)) => {
                        format!("syntax error at line {l}, column {c}: {}", e.message())
                    }
                    None => format!("syntax error: {}", e.message()),
                };
                let mut i = Issue::error("", msg);
                i.source = Some(s.path.clone());
                issues.push(i);
            }
        }
        if decimal_socket_mode(&s.text) {
            let mut i = Issue::error(
                "control.socket_mode",
                "a decimal integer is not a permission mode (416 would mean 0o640): write it in octal, `0o640` or \"640\"",
            );
            i.source = Some(s.path.clone());
            issues.push(i);
        }
    }
    if !issues.is_empty() {
        // Report bad overrides in the same pass as the files' syntax.
        if !overrides.is_empty() {
            let (_, found) = extract(overrides.table.clone(), env);
            for mut i in found.into_iter().filter(Issue::is_error) {
                i.source = Some(PathBuf::from(OVERRIDES_SOURCE));
                issues.push(i);
            }
        }
        return Err(ConfigError::Invalid { issues });
    }
    if !overrides.is_empty() {
        merge_into(
            &mut merged,
            overrides.table.clone(),
            "",
            Path::new(OVERRIDES_SOURCE),
            &mut prov,
        );
    }

    let defaulted_names = match merged.get("device") {
        Some(Value::Array(a)) => a
            .iter()
            .filter(|d| d.get("name").is_none())
            .filter_map(|d| entry_id(d).map(str::to_owned))
            .collect(),
        _ => std::collections::BTreeSet::new(),
    };
    let (mut config, mut found) = extract(merged, env);
    // A value set on the command line wins over `state.toml` too: an
    // overridden key that commands can change (apply class `live`) is
    // treated as locked, so the stored value is skipped and commands are
    // refused with `Locked`.
    for (path, lock) in OVERRIDE_LOCKS {
        if overrides.contains(path) {
            config.clock.locked.insert(*lock);
        }
    }
    found.extend(merge_warnings);
    // Semantic checks run even after type errors, so that everything is
    // reported in one pass; only the checks that read a value that failed
    // to parse (and so holds its default) are left out.
    let bad: Vec<String> = found
        .iter()
        .filter(|i| i.is_error())
        .map(|i| i.path.clone())
        .collect();
    found.extend(
        validate(&config, env)
            .into_iter()
            .filter(|i| !suppressed(&i.path, &bad)),
    );
    for i in &mut found {
        if i.source.is_none() {
            i.source = prov.source_of(&i.path).map(Path::to_path_buf);
        }
    }
    if found.iter().any(Issue::is_error) {
        found.sort_by_key(|i| std::cmp::Reverse(i.severity));
        return Err(ConfigError::Invalid { issues: found });
    }
    Ok(LoadedConfig {
        defaulted_names,
        config,
        provenance: prov,
        warnings: found,
        files: sources.iter().map(|s| s.path.clone()).collect(),
        overridden: overrides.keys.clone(),
    })
}

fn read(path: &Path) -> Result<Option<String>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Read the main file and the drop-ins from disk (lexical file-name order).
///
/// # Errors
/// [`ConfigError::Io`] if a file or the directory cannot be read.
pub fn read_sources(paths: &ConfigPaths) -> Result<Vec<Source>, ConfigError> {
    let mut sources = Vec::new();
    if let Some(text) = read(&paths.main)? {
        sources.push(Source::new(&paths.main, text));
    }
    let rd = match std::fs::read_dir(&paths.dropin_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(sources),
        Err(source) => {
            return Err(ConfigError::Io {
                path: paths.dropin_dir.clone(),
                source,
            });
        }
    };
    let mut files = Vec::new();
    for entry in rd {
        let entry = entry.map_err(|source| ConfigError::Io {
            path: paths.dropin_dir.clone(),
            source,
        })?;
        let name = entry.file_name();
        let Some(n) = name.to_str() else { continue };
        if n.starts_with('.') || !n.ends_with(".toml") {
            continue;
        }
        let path = entry.path();
        if std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
            files.push((n.to_owned(), path));
        }
    }
    files.sort();
    for (_, path) in files {
        if let Some(text) = read(&path)? {
            sources.push(Source::new(path, text));
        }
    }
    Ok(sources)
}

/// Load the configuration from disk: [`read_sources`] then
/// [`load_from_sources`].
///
/// # Errors
/// See those two functions.
pub fn load_config(paths: &ConfigPaths, env: &Environment) -> Result<LoadedConfig, ConfigError> {
    load_from_sources(&read_sources(paths)?, env)
}

/// [`load_config`] with [`Overrides`] applied over every file.
///
/// # Errors
/// See [`load_from_sources_with`].
pub fn load_config_with(
    paths: &ConfigPaths,
    env: &Environment,
    overrides: &Overrides,
) -> Result<LoadedConfig, ConfigError> {
    load_from_sources_with(&read_sources(paths)?, env, overrides)
}

/// Checks that read another key: an issue at the first path depends on the
/// value of the second.
const CROSS_KEY: &[(&str, &str)] = &[
    ("audio.default_period_frames", "audio.max_period_frames"),
    ("audio.max_ring_mib", "audio.ring_budget_mib"),
    (
        "control.admin_reserved_connections",
        "control.max_connections",
    ),
    (
        "defaults.bridge.error_backoff_max_ms",
        "defaults.bridge.error_backoff_min_ms",
    ),
    (
        "defaults.bridge.error_backoff_min_ms",
        "defaults.bridge.error_backoff_max_ms",
    ),
    (
        "defaults.bridge.error_sleep_max_ms",
        "defaults.bridge.error_sleep_min_ms",
    ),
    (
        "defaults.bridge.error_sleep_min_ms",
        "defaults.bridge.error_sleep_max_ms",
    ),
    ("network.secondary", "network.interfaces"),
    // The RLIMIT_MEMLOCK warning reads all of these.
    ("audio.ring_budget_mib", "engine.mlock"),
    ("audio.ring_budget_mib", "engine.rx_batch"),
    ("audio.ring_budget_mib", "engine.tx_batch"),
    ("audio.ring_budget_mib", "network.mtu_payload"),
    ("audio.ring_budget_mib", "daemon.realtime"),
    ("control.event_queue", "control.max_connections"),
];

fn under(path: &str, prefix: &str) -> bool {
    path == prefix
        || (path.starts_with(prefix)
            && matches!(path.as_bytes().get(prefix.len()), Some(b'.' | b'[')))
}

/// The `[[...]]` entry a path is in (`device[1]`), if any.
fn entry(path: &str) -> Option<&str> {
    let end = path.find(']')?;
    ["device[", "bridge[", "flow["]
        .iter()
        .any(|p| path.starts_with(p))
        .then(|| &path[..=end])
}

/// Is a semantic issue at `path` a consequence of the type errors at
/// `bad`? It is if it is about a bad key (or below it), in the same
/// `[[...]]` entry as one, depends on one (`CROSS_KEY`, `[defaults.*]`
/// for the entries), or is a `[[flow]]`/`[[bridge]]` issue while a device
/// failed to parse (they are checked against the devices).
fn suppressed(path: &str, bad: &[String]) -> bool {
    bad.iter().any(|b| {
        under(path, b)
            || entry(b).is_some_and(|e| path.starts_with(e))
            || CROSS_KEY
                .iter()
                .any(|(p, dep)| under(path, p) && under(b, dep))
            || (b.starts_with("device") && (path.starts_with("flow") || path.starts_with("bridge")))
            || (b.starts_with("defaults.bridge") && path.starts_with("bridge["))
            || (b.starts_with("defaults.flow") && path.starts_with("flow["))
            || b.is_empty()
    })
}

/// Is `[control] socket_mode` in `text` a decimal integer? The parsed
/// value no longer knows how it was written, so the source is read again
/// with spans.
fn decimal_socket_mode(text: &str) -> bool {
    use toml::de::{DeTable, DeValue};
    fn find<'a>(t: &'a DeTable<'_>, key: &str) -> Option<&'a DeValue<'a>> {
        t.iter()
            .find(|(k, _)| k.get_ref().as_ref() == key)
            .map(|(_, v)| v.get_ref())
    }
    let Ok(root) = DeTable::parse(text) else {
        return false;
    };
    let Some(DeValue::Table(control)) = find(root.get_ref(), "control") else {
        return false;
    };
    matches!(find(control, "socket_mode"), Some(DeValue::Integer(i)) if i.radix() == 10)
}
