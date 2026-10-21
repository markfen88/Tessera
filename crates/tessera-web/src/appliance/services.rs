//! The Services card: an allowlist of systemd services from the
//! configuration, their state (one `systemctl show` call, no privilege) and
//! the three actions the operator may take on them. The portal runs plain
//! `systemctl` as its own user; PID 1 asks polkit, and a rule generated from
//! the same allowlist ([`polkit_rules`]) answers yes for exactly those units
//! and verbs. Every host command goes through the [`Runner`] trait, so the
//! whole card is testable without a systemd.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use crate::config::{ApplianceConfig, ServiceEntry, canonical_unit, is_own_unit};

/// Absolute path of `systemctl`: the portal never searches `PATH` for it.
pub const SYSTEMCTL: &str = "/usr/bin/systemctl";
/// The user the portal runs as; the subject of the polkit rule.
pub const SERVICE_USER: &str = "tessera-web";
/// How long the portal waits for one host command. The command is killed
/// then, but what it asked systemd for may still complete.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// Longest message shown to the operator for a failed action.
pub const STDERR_MAX: usize = 300;

// ---- runner ------------------------------------------------------------------------

/// What a finished command left behind.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Output {
    /// Exit code; `None` when killed by a signal.
    pub code: Option<i32>,
    /// Standard output, lossily decoded.
    pub stdout: String,
    /// Standard error, lossily decoded.
    pub stderr: String,
}

impl Output {
    /// Exit code 0 with this standard output.
    #[must_use]
    pub fn ok(stdout: &str) -> Self {
        Self {
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// A failure with this exit code and standard error.
    #[must_use]
    pub fn fail(code: i32, stderr: &str) -> Self {
        Self {
            code: Some(code),
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    /// Exit code 0.
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Why a command produced no [`Output`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// The portal stopped waiting after [`TIMEOUT`]; the action may still
    /// complete.
    Timeout,
    /// It could not be started.
    Spawn(String),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => write!(
                f,
                "the portal stopped waiting after {} s; the action may still complete",
                TIMEOUT.as_secs()
            ),
            Self::Spawn(e) => write!(f, "cannot start: {e}"),
        }
    }
}

/// A boxed future, so the trait can be a `dyn` object.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Runs host commands: a program and its arguments, never a shell. The
/// real one spawns processes; tests script answers.
pub trait Runner: Send + Sync + fmt::Debug {
    /// Run `program` with `args` to completion, capturing both streams.
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
    ) -> BoxFuture<'a, Result<Output, RunError>>;
}

/// The real runner: `tokio::process::Command`, no standard input, killed
/// after [`TIMEOUT`].
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessRunner;

impl Runner for ProcessRunner {
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
    ) -> BoxFuture<'a, Result<Output, RunError>> {
        Box::pin(async move {
            let child = tokio::process::Command::new(program)
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .output();
            match tokio::time::timeout(TIMEOUT, child).await {
                Err(_) => Err(RunError::Timeout),
                Ok(Err(e)) => Err(RunError::Spawn(e.to_string())),
                Ok(Ok(o)) => Ok(Output {
                    code: o.status.code(),
                    stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
                }),
            }
        })
    }
}

/// A command line as the runner sees it: the program, then its arguments.
pub type Argv = Vec<String>;

/// `[program, args...]` from string slices.
#[must_use]
pub fn argv(parts: &[&str]) -> Argv {
    parts.iter().map(|p| (*p).to_owned()).collect()
}

/// A scripted runner for tests: answers by exact argument vector and records
/// every call. Unscripted commands fail with exit code 1 and `unscripted
/// command` on stderr.
#[derive(Debug, Default)]
pub struct FakeRunner {
    scripts: Mutex<HashMap<Argv, Result<Output, RunError>>>,
    calls: Mutex<Vec<Argv>>,
}

impl FakeRunner {
    /// Answer exactly `cmd` (program first) with `out`.
    pub fn script(&self, cmd: &[&str], out: Output) {
        if let Ok(mut m) = self.scripts.lock() {
            m.insert(argv(cmd), Ok(out));
        }
    }

    /// Make exactly `cmd` fail to run at all.
    pub fn script_error(&self, cmd: &[&str], e: RunError) {
        if let Ok(mut m) = self.scripts.lock() {
            m.insert(argv(cmd), Err(e));
        }
    }

    /// Every command run so far, in order, program first.
    #[must_use]
    pub fn calls(&self) -> Vec<Argv> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Forget the recorded calls.
    pub fn clear_calls(&self) {
        if let Ok(mut c) = self.calls.lock() {
            c.clear();
        }
    }
}

impl Runner for FakeRunner {
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
    ) -> BoxFuture<'a, Result<Output, RunError>> {
        Box::pin(async move {
            let mut line = Vec::with_capacity(args.len() + 1);
            line.push(program.to_owned());
            line.extend(args.iter().cloned());
            if let Ok(mut c) = self.calls.lock() {
                c.push(line.clone());
            }
            self.scripts
                .lock()
                .ok()
                .and_then(|m| m.get(&line).cloned())
                .unwrap_or_else(|| Ok(Output::fail(1, "unscripted command")))
        })
    }
}

// ---- state ------------------------------------------------------------------------------

/// `UnitFileState` folded to what the row needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enabled {
    /// Starts at boot (`enabled`, `enabled-runtime`, `alias`, `generated`,
    /// `transient`, `linked`, `linked-runtime`): switch ON.
    On,
    /// Does not start at boot (`disabled`, `indirect`, `bad`, anything
    /// else): switch OFF.
    Off,
    /// `static`: cannot be enabled or disabled, only started by what wants
    /// it. No switch; the status is shown.
    Static,
    /// `masked` or `masked-runtime`: the switch is greyed, no restart.
    Masked,
    /// No such unit: switch greyed, no restart.
    NotFound,
    /// `systemctl` could not be asked; the switch is greyed and says why.
    Unknown(String),
}

impl Enabled {
    /// Fold the `LoadState` and `UnitFileState` of one unit.
    #[must_use]
    pub fn of(load_state: &str, unit_file_state: &str) -> Self {
        if load_state == "not-found" {
            return Self::NotFound;
        }
        match unit_file_state {
            "enabled" | "enabled-runtime" | "alias" | "generated" | "transient" | "linked"
            | "linked-runtime" => Self::On,
            "static" => Self::Static,
            "masked" | "masked-runtime" => Self::Masked,
            // `disabled`, `indirect`, `bad` and anything systemd adds.
            _ => Self::Off,
        }
    }

    /// The switch is ON.
    #[must_use]
    pub fn is_on(&self) -> bool {
        matches!(self, Self::On)
    }
}

/// The state of one unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitState {
    /// The unit as systemd names it (`name.service`).
    pub unit: String,
    /// Boot state.
    pub enabled: Enabled,
    /// `ActiveState` is `active` or `reloading`.
    pub active: bool,
}

/// The arguments of the one read the page makes: the three properties of
/// every unit, plus `Id` to match the answer blocks to the units asked.
#[must_use]
pub fn show_args(units: &[String]) -> Vec<String> {
    let mut v = vec![
        "show".to_owned(),
        "-p".to_owned(),
        "Id,LoadState,UnitFileState,ActiveState".to_owned(),
    ];
    v.extend(units.iter().cloned());
    v
}

/// Parse `systemctl show` output: blocks of `Key=value` lines separated by
/// an empty line, one per unit. Keyed by `Id`.
#[must_use]
pub fn parse_show(stdout: &str) -> HashMap<String, (Enabled, bool)> {
    let mut out = HashMap::new();
    for block in stdout.split("\n\n") {
        let mut id = None;
        let (mut load, mut file, mut active) = ("", "", "");
        for line in block.lines() {
            if let Some((k, v)) = line.split_once('=') {
                match k {
                    "Id" => id = Some(v.to_owned()),
                    "LoadState" => load = v,
                    "UnitFileState" => file = v,
                    "ActiveState" => active = v,
                    _ => {}
                }
            }
        }
        if let Some(id) = id {
            out.insert(
                id,
                (
                    Enabled::of(load, file),
                    matches!(active, "active" | "reloading"),
                ),
            );
        }
    }
    out
}

/// One row of the card: an entry and the state of its units, folded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The configured entry.
    pub entry: ServiceEntry,
    /// Per unit, in the entry's order (canonical names).
    pub units: Vec<UnitState>,
}

impl Row {
    fn new(
        entry: &ServiceEntry,
        states: &Result<HashMap<String, (Enabled, bool)>, String>,
    ) -> Self {
        let units = entry
            .canonical_units()
            .into_iter()
            .map(|unit| {
                let (enabled, active) = match states {
                    Ok(m) => m.get(&unit).cloned().unwrap_or((Enabled::NotFound, false)),
                    Err(e) => (Enabled::Unknown(e.clone()), false),
                };
                UnitState {
                    unit,
                    enabled,
                    active,
                }
            })
            .collect();
        Self {
            entry: entry.clone(),
            units,
        }
    }

    /// Every unit is `static`: there is no switch.
    #[must_use]
    pub fn static_only(&self) -> bool {
        !self.units.is_empty()
            && self
                .units
                .iter()
                .all(|u| matches!(u.enabled, Enabled::Static))
    }

    /// The switch is ON: every unit that can be enabled is.
    #[must_use]
    pub fn on(&self) -> bool {
        !self.static_only()
            && self
                .units
                .iter()
                .all(|u| u.enabled.is_on() || matches!(u.enabled, Enabled::Static))
    }

    /// Every unit is running.
    #[must_use]
    pub fn active(&self) -> bool {
        !self.units.is_empty() && self.units.iter().all(|u| u.active)
    }

    /// Why the switch is greyed out, if it is: the first unit that is
    /// masked, missing or could not be asked.
    #[must_use]
    pub fn blocked(&self) -> Option<String> {
        self.units.iter().find_map(|u| match &u.enabled {
            Enabled::Masked => Some(format!("{} is masked", u.unit)),
            Enabled::NotFound => Some(format!("{} is not installed", u.unit)),
            Enabled::Unknown(why) => Some(format!("{}: {why}", u.unit)),
            Enabled::On | Enabled::Off | Enabled::Static => None,
        })
    }

    /// The status pill is shown: the switch is ON (or there is none) and
    /// nothing blocks the row.
    #[must_use]
    pub fn shows_status(&self) -> bool {
        (self.on() || self.static_only()) && self.blocked().is_none()
    }

    /// A restart button makes sense: no unit is missing or masked.
    #[must_use]
    pub fn restartable(&self) -> bool {
        !self
            .units
            .iter()
            .any(|u| matches!(u.enabled, Enabled::NotFound | Enabled::Masked))
    }
}

/// Ask `systemctl` about every configured unit in one call (so a stuck
/// `systemctl` costs one [`TIMEOUT`], not one per unit) and fold the answer
/// into rows.
pub async fn query_all(r: &dyn Runner, cfg: &ApplianceConfig) -> Vec<Row> {
    let units: Vec<String> = cfg
        .services
        .iter()
        .flat_map(ServiceEntry::canonical_units)
        .collect();
    if units.is_empty() {
        return Vec::new();
    }
    let states = match r.run(SYSTEMCTL, &show_args(&units)).await {
        Ok(o) if o.success() => Ok(parse_show(&o.stdout)),
        Ok(o) => Err(if o.stderr.trim().is_empty() {
            format!("systemctl show failed (exit code {})", code_text(o.code))
        } else {
            sanitize(&o.stderr)
        }),
        Err(e) => Err(e.to_string()),
    };
    cfg.services.iter().map(|e| Row::new(e, &states)).collect()
}

fn code_text(code: Option<i32>) -> String {
    code.map_or("signal".to_owned(), |c| c.to_string())
}

// ---- actions ------------------------------------------------------------------------------

/// What the operator may do to a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `systemctl enable --now --no-reload`.
    Enable,
    /// `systemctl disable --now --no-reload`.
    Disable,
    /// `systemctl restart`.
    Restart,
}

impl Action {
    /// The form value.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "enable" => Some(Self::Enable),
            "disable" => Some(Self::Disable),
            "restart" => Some(Self::Restart),
            _ => None,
        }
    }

    /// The form value.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Restart => "restart",
        }
    }

    /// The `systemctl` arguments the portal runs for `unit` (a canonical
    /// name): always `start` of the root oneshot helper
    /// `tessera-web-unitfile@<verb>-<name>.service`, which runs
    /// [`unitfile_args`] after checking the allowlist again. The portal
    /// never names a service directly to PID 1. `--no-block` when the
    /// portal's own unit is stopped, so the reply reaches the browser first.
    #[must_use]
    pub fn systemctl_args(self, unit: &str) -> Vec<String> {
        let mut v: Vec<String> = vec!["start".into()];
        if self.stops_own_unit(unit) {
            v.push("--no-block".into());
        }
        v.push(helper_unit(self.helper_verb(), unit));
        v
    }

    /// The helper verb of this action.
    #[must_use]
    pub fn helper_verb(self) -> HelperVerb {
        match self {
            Self::Enable => HelperVerb::Enable,
            Self::Disable => HelperVerb::Disable,
            Self::Restart => HelperVerb::Restart,
        }
    }

    /// Running this on `unit` stops the portal itself (so the portal does
    /// not wait and does not learn the outcome).
    #[must_use]
    pub fn stops_own_unit(self, unit: &str) -> bool {
        self != Self::Enable && is_own_unit(unit)
    }

    /// Past tense for messages.
    #[must_use]
    pub fn done(self) -> &'static str {
        match self {
            Self::Enable => "enabled and started",
            Self::Disable => "disabled and stopped",
            Self::Restart => "restarted",
        }
    }

    /// The action needs a confirmation page for an essential entry.
    #[must_use]
    pub fn asks_when_essential(self) -> bool {
        self != Self::Enable
    }
}

// ---- the helper ------------------------------------------------------------------------

/// The template of the root oneshot that acts on one allowlisted service
/// (`dist/tessera-web-unitfile@.service`). Its instance is
/// `<verb>-<service name without .service>`, escaped ([`instance`]). The
/// portal may only *start* these instances; it never names a service to
/// PID 1 itself (a direct `manage-units` grant on a service would also
/// allow `systemd-run --unit=<service>` while that unit is not loaded).
pub const UNITFILE_TEMPLATE: &str = "tessera-web-unitfile@";

/// What the helper may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperVerb {
    /// `systemctl enable --now --no-reload`.
    Enable,
    /// `systemctl disable --now --no-reload`.
    Disable,
    /// `systemctl restart`.
    Restart,
}

impl HelperVerb {
    /// All, in rule order.
    pub const ALL: [Self; 3] = [Self::Enable, Self::Disable, Self::Restart];

    /// The word in the instance name and on the `systemctl` line.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Restart => "restart",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "enable" => Some(Self::Enable),
            "disable" => Some(Self::Disable),
            "restart" => Some(Self::Restart),
            _ => None,
        }
    }
}

/// The instance string for `verb` and `unit` (any form; the `.service`
/// suffix is dropped): `enable-tesserad`. Letters, digits, `_`, `.` and
/// `-` stand for themselves; every other character of the name (only `@`
/// can occur in an allowlisted name) is written `\xNN` as `systemd-escape`
/// writes it, so the instance is a valid unit-name part. The first `-`
/// separates the verb; verbs have none, names may.
#[must_use]
pub fn instance(verb: HelperVerb, unit: &str) -> String {
    let stem = unit.strip_suffix(".service").unwrap_or(unit);
    let mut s = String::from(verb.name());
    s.push('-');
    for c in stem.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
            s.push(c);
        } else {
            for b in c.to_string().bytes() {
                s.push_str(&format!("\\x{b:02x}"));
            }
        }
    }
    s
}

/// The full name of the helper unit for `verb` and `unit`:
/// `tessera-web-unitfile@enable-tesserad.service`.
#[must_use]
pub fn helper_unit(verb: HelperVerb, unit: &str) -> String {
    format!("{UNITFILE_TEMPLATE}{}.service", instance(verb, unit))
}

/// The reverse of [`instance`]: the verb and the canonical service name.
/// `%i` in the template's `ExecStart` may hand the helper either the
/// escaped form (`\x40`) or, after systemd's own unescaping of the command
/// line, a literal `@`; both are accepted (the allowlist is the gate, not
/// the spelling). Refuses anything else that is not exactly an instance:
/// no `.service` suffix, no template prefix, no stray characters.
///
/// # Errors
/// A message naming what is wrong with `s`.
pub fn parse_instance(s: &str) -> Result<(HelperVerb, String), String> {
    let (verb, rest) = s
        .split_once('-')
        .ok_or_else(|| format!("instance `{s}`: expected `<enable|disable|restart>-<service>`"))?;
    let verb = HelperVerb::parse(verb).ok_or_else(|| {
        format!("instance `{s}`: the verb must be `enable`, `disable` or `restart`")
    })?;
    let mut stem = String::with_capacity(rest.len());
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let hex: String = chars.by_ref().take(3).collect();
            let byte = hex
                .strip_prefix('x')
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| format!("instance `{s}`: bad escape `\\{hex}`"))?;
            if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-') || byte == 0 {
                return Err(format!("instance `{s}`: needless escape `\\{hex}`"));
            }
            stem.push(char::from(byte));
        } else if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '@') {
            stem.push(c);
        } else {
            return Err(format!("instance `{s}`: character `{c}` is not allowed"));
        }
    }
    if stem.is_empty() || stem.ends_with(".service") || !crate::config::valid_unit_name(&stem) {
        return Err(format!("instance `{s}`: `{stem}` is not a service name"));
    }
    Ok((verb, canonical_unit(&stem)))
}

/// What the helper runs as root for a checked instance.
#[must_use]
pub fn unitfile_args(verb: HelperVerb, unit: &str) -> Vec<String> {
    match verb {
        HelperVerb::Enable | HelperVerb::Disable => vec![
            verb.name().to_owned(),
            "--now".to_owned(),
            "--no-reload".to_owned(),
            unit.to_owned(),
        ],
        HelperVerb::Restart => vec!["restart".to_owned(), unit.to_owned()],
    }
}

/// The helper, `tessera-web appliance unitfile <instance>`: parse the
/// instance, refuse a service that is not in the allowlist of `cfg` (the
/// file re-read by the caller) or an appliance mode that is off, then run
/// one `systemctl` ([`unitfile_args`]). `Ok` is the exit code to propagate;
/// `Err` is a refusal (exit code 1 for the caller).
pub async fn run_unitfile(
    r: &dyn Runner,
    cfg: &ApplianceConfig,
    inst: &str,
) -> Result<i32, String> {
    let (verb, unit) = parse_instance(inst)?;
    if !cfg.enabled {
        return Err("appliance mode is off ([appliance] enabled = false)".into());
    }
    if !cfg.services.iter().any(|e| e.has_unit(&unit)) {
        return Err(format!("`{unit}` is not an [[appliance.service]] unit"));
    }
    match r.run(SYSTEMCTL, &unitfile_args(verb, &unit)).await {
        Ok(o) => {
            if !o.stderr.is_empty() {
                eprint!("{}", o.stderr);
            }
            Ok(o.code.unwrap_or(1))
        }
        Err(e) => Err(format!("systemctl {} {unit}: {e}", verb.name())),
    }
}

/// The entry named by a form's `unit` field, if it is allowlisted. The
/// form sends [`ServiceEntry::key`]; nothing else is accepted, and nothing
/// runs for an unknown key.
#[must_use]
pub fn allowlisted<'a>(cfg: &'a ApplianceConfig, key: &str) -> Option<&'a ServiceEntry> {
    cfg.services.iter().find(|e| e.key() == key)
}

/// Run `action` on every unit of `entry`, in order (`static` units are
/// skipped by enable and disable: they have no boot state; the portal's
/// own unit goes last, so the others are done before it stops), stopping
/// at the first failure. `Err` is the message for the operator: the
/// sanitised error, where the helper's own error is (the journal) and, for
/// a group, which units were already done.
pub async fn perform(
    r: &dyn Runner,
    entry: &ServiceEntry,
    action: Action,
    rows: Option<&Row>,
) -> Result<(), String> {
    let mut units = entry.canonical_units();
    units.sort_by_key(|u| is_own_unit(u));
    let mut done: Vec<String> = Vec::new();
    for unit in units {
        let is_static = rows.is_some_and(|row| {
            row.units
                .iter()
                .any(|u| u.unit == unit && matches!(u.enabled, Enabled::Static))
        });
        if is_static && action != Action::Restart {
            continue;
        }
        let args = action.systemctl_args(&unit);
        let failed = match r.run(SYSTEMCTL, &args).await {
            Ok(o) if o.success() => None,
            Ok(o) => {
                let why = sanitize(&o.stderr);
                Some(if why.is_empty() {
                    format!("exit code {}", code_text(o.code))
                } else {
                    why
                })
            }
            Err(e) => Some(e.to_string()),
        };
        if let Some(why) = failed {
            // The oneshot's own error is in the journal, not in the
            // `systemctl start` output.
            let mut msg = format!(
                "systemctl {} {unit}: {why} (details: journalctl -u {})",
                action.name(),
                args.last().map_or("", String::as_str)
            );
            if !done.is_empty() {
                msg.push_str(&format!(" (done before it: {})", done.join(", ")));
            }
            return Err(bounded(msg));
        }
        done.push(unit);
    }
    Ok(())
}

/// At most [`STDERR_MAX`] characters, then `…`.
fn bounded(mut s: String) -> String {
    if s.chars().count() > STDERR_MAX {
        s = s.chars().take(STDERR_MAX).collect();
        s.push('…');
    }
    s
}

/// Standard error made fit for a page: terminal escape sequences (CSI and
/// OSC) and control characters dropped, lines joined with `; `, at most
/// [`STDERR_MAX`] characters (then `…`).
#[must_use]
pub fn sanitize(stderr: &str) -> String {
    let mut clean = String::with_capacity(stderr.len());
    let mut chars = stderr.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                // CSI: `ESC [` up to a letter.
                Some('[') => {
                    for n in chars.by_ref() {
                        if n.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                // OSC: `ESC ]` up to BEL or `ESC \`.
                Some(']') => {
                    let mut prev = ' ';
                    for n in chars.by_ref() {
                        if n == '\x07' || (prev == '\x1b' && n == '\\') {
                            break;
                        }
                        prev = n;
                    }
                }
                // Other escapes take one character.
                _ => {
                    chars.next();
                }
            }
        } else if c == '\n' {
            clean.push('\n');
        } else if !c.is_control() {
            clean.push(c);
        }
    }
    let joined = clean
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    bounded(joined)
}

// ---- polkit ------------------------------------------------------------------------------

/// Name of the rules file under `/etc/polkit-1/rules.d/`.
pub const POLKIT_RULES_NAME: &str = "49-tessera-web-appliance.rules";
/// The one polkit action the rule grants: PID 1 passes it the unit and the
/// verb, so the rule can name the helper instances exactly.
/// `org.freedesktop.systemd1.manage-unit-files` (enable, disable) carries no
/// details (systemd 257, `src/core/dbus-util.c`), so a rule could not tell
/// units apart; it is never granted, and neither is `manage-units` on a
/// service itself (see [`UNITFILE_TEMPLATE`]).
pub const ACTION_MANAGE_UNITS: &str = "org.freedesktop.systemd1.manage-units";
/// The one verb the rule grants: starting a helper instance.
pub const VERB_START: &str = "start";

/// A JavaScript string literal; unit names contain nothing to escape, but
/// the quoting is explicit all the same.
fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Every helper instance unit of the configuration, in rule order: for each
/// listed service, enable, disable, restart.
#[must_use]
pub fn helper_units(cfg: &ApplianceConfig) -> Vec<String> {
    let mut units: Vec<String> = Vec::new();
    for e in &cfg.services {
        for u in e.canonical_units() {
            if !units.contains(&u) {
                units.push(u);
            }
        }
    }
    units
        .iter()
        .flat_map(|u| HelperVerb::ALL.iter().map(|v| helper_unit(*v, u)))
        .collect()
}

/// The polkit rules file for the configured units: host configuration
/// generated from the allowlist. One action (`manage-units`), one verb
/// (`start`), one array lookup: the listed helper instances. Installed by
/// the operator (the header says how).
#[must_use]
pub fn polkit_rules(cfg: &ApplianceConfig) -> String {
    let helpers = helper_units(cfg)
        .iter()
        .map(|s| js_string(s))
        .collect::<Vec<_>>()
        .join(", ");
    let mut s = String::new();
    s.push_str("// polkit rules for tessera-web appliance mode: what the portal's user may\n");
    s.push_str("// ask systemd for. Generated by `tessera-web appliance polkit` from the\n");
    s.push_str("// [[appliance.service]] entries of /etc/tessera/web.toml; regenerate\n");
    s.push_str("// after changing them. Install:\n");
    s.push_str("//   f=$(mktemp) && tessera-web appliance polkit > \"$f\" \\\n");
    s.push_str(&format!(
        "//     && sudo install -m 0644 -o root -g root \"$f\" /etc/polkit-1/rules.d/{POLKIT_RULES_NAME} \\\n"
    ));
    s.push_str("//     && rm -f \"$f\"\n");
    s.push_str("polkit.addRule(function(action, subject) {\n");
    s.push_str(&format!(
        "    if (subject.user != {} || action.id != {}) {{\n        return polkit.Result.NOT_HANDLED;\n    }}\n",
        js_string(SERVICE_USER),
        js_string(ACTION_MANAGE_UNITS)
    ));
    s.push_str(&format!("    var helpers = [{helpers}];\n"));
    s.push_str(&format!(
        "    if (helpers.indexOf(action.lookup(\"unit\")) >= 0 && action.lookup(\"verb\") == {}) {{\n        return polkit.Result.YES;\n    }}\n",
        js_string(VERB_START)
    ));
    s.push_str("    return polkit.Result.NOT_HANDLED;\n});\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(units: &[&str], essential: bool) -> ServiceEntry {
        ServiceEntry {
            label: "x".into(),
            description: String::new(),
            units: units.iter().map(|u| (*u).to_owned()).collect(),
            essential,
        }
    }

    fn cfg(entries: Vec<ServiceEntry>) -> ApplianceConfig {
        ApplianceConfig {
            enabled: true,
            services: entries,
            ..ApplianceConfig::default()
        }
    }

    fn st(u: &str, e: Enabled, a: bool) -> UnitState {
        UnitState {
            unit: canonical_unit(u),
            enabled: e,
            active: a,
        }
    }

    #[test]
    fn systemctl_is_the_absolute_path() {
        assert_eq!(SYSTEMCTL, "/usr/bin/systemctl");
    }

    #[test]
    fn unit_file_states_map_to_switch_states() {
        let p = |w: &str| Enabled::of("loaded", w);
        for on in [
            "enabled",
            "enabled-runtime",
            "alias",
            "generated",
            "transient",
            "linked",
            "linked-runtime",
        ] {
            assert_eq!(p(on), Enabled::On, "{on}");
        }
        for off in ["disabled", "indirect", "bad", "", "something-new"] {
            assert_eq!(p(off), Enabled::Off, "{off:?}");
        }
        assert_eq!(p("static"), Enabled::Static);
        assert_eq!(p("masked"), Enabled::Masked);
        assert_eq!(p("masked-runtime"), Enabled::Masked);
        assert_eq!(Enabled::of("not-found", ""), Enabled::NotFound);
        assert_eq!(Enabled::of("masked", "masked"), Enabled::Masked);
    }

    #[test]
    fn show_output_is_parsed_per_unit_by_id() {
        let out = "Id=tesserad.service\nLoadState=loaded\nActiveState=active\nUnitFileState=enabled\n\n\
                   Id=nope.service\nLoadState=not-found\nActiveState=inactive\nUnitFileState=\n\n\
                   Id=dbus.service\nLoadState=loaded\nActiveState=reloading\nUnitFileState=static\n";
        let m = parse_show(out);
        assert_eq!(m["tesserad.service"], (Enabled::On, true));
        assert_eq!(m["nope.service"], (Enabled::NotFound, false));
        assert_eq!(m["dbus.service"], (Enabled::Static, true));
        assert_eq!(m.len(), 3);
        assert!(parse_show("").is_empty());
        assert_eq!(
            show_args(&["a.service".into(), "b.service".into()]),
            [
                "show",
                "-p",
                "Id,LoadState,UnitFileState,ActiveState",
                "a.service",
                "b.service"
            ]
        );
    }

    #[test]
    fn group_rows_fold_their_units() {
        let row = Row {
            entry: entry(&["a", "b"], false),
            units: vec![st("a", Enabled::On, true), st("b", Enabled::On, false)],
        };
        assert!(row.on() && !row.active() && row.blocked().is_none() && row.restartable());
        assert!(row.shows_status());
        let row = Row {
            entry: entry(&["a", "b"], false),
            units: vec![st("a", Enabled::On, true), st("b", Enabled::Off, false)],
        };
        assert!(!row.on() && !row.shows_status());
        let row = Row {
            entry: entry(&["a", "b"], false),
            units: vec![
                st("a", Enabled::On, true),
                st("b", Enabled::NotFound, false),
            ],
        };
        assert_eq!(row.blocked().as_deref(), Some("b.service is not installed"));
        assert!(!row.restartable());
        let row = Row {
            entry: entry(&["a"], false),
            units: vec![st("a", Enabled::Masked, false)],
        };
        assert_eq!(row.blocked().as_deref(), Some("a.service is masked"));
        assert!(!row.restartable());
        // static: no switch, a status, restartable; in a group it does not
        // hold the switch back.
        let row = Row {
            entry: entry(&["a"], false),
            units: vec![st("a", Enabled::Static, true)],
        };
        assert!(row.static_only() && !row.on() && row.shows_status() && row.restartable());
        let row = Row {
            entry: entry(&["a", "b"], false),
            units: vec![st("a", Enabled::Static, true), st("b", Enabled::On, true)],
        };
        assert!(!row.static_only() && row.on() && row.active());
    }

    #[tokio::test]
    async fn query_all_makes_one_call_and_a_failure_greys_every_row() {
        let r = FakeRunner::default();
        let c = cfg(vec![
            entry(&["a"], false),
            entry(&["b", "c.service"], false),
        ]);
        r.script(
            &[SYSTEMCTL, "show", "-p", "Id,LoadState,UnitFileState,ActiveState", "a.service", "b.service", "c.service"],
            Output::ok("Id=a.service\nLoadState=loaded\nActiveState=active\nUnitFileState=enabled\n\nId=b.service\nLoadState=loaded\nActiveState=inactive\nUnitFileState=disabled\n"),
        );
        let rows = query_all(&r, &c).await;
        assert_eq!(r.calls().len(), 1);
        assert!(rows[0].on() && rows[0].active());
        assert_eq!(rows[1].units[0].enabled, Enabled::Off);
        // A unit the answer does not mention is treated as missing.
        assert_eq!(rows[1].units[1].enabled, Enabled::NotFound);
        let r = FakeRunner::default();
        r.script_error(
            &[
                SYSTEMCTL,
                "show",
                "-p",
                "Id,LoadState,UnitFileState,ActiveState",
                "a.service",
                "b.service",
                "c.service",
            ],
            RunError::Timeout,
        );
        let rows = query_all(&r, &c).await;
        assert!(rows.iter().all(|row| row.blocked().is_some()));
        assert!(
            rows[0]
                .blocked()
                .unwrap()
                .contains("stopped waiting after 10 s")
        );
        assert!(query_all(&r, &cfg(vec![])).await.is_empty());
    }

    const H_ENABLE: &str = "tessera-web-unitfile@enable-tesserad.service";
    const H_DISABLE: &str = "tessera-web-unitfile@disable-tesserad.service";
    const H_RESTART: &str = "tessera-web-unitfile@restart-tesserad.service";

    #[test]
    fn actions_build_exact_argument_vectors() {
        // Every action starts the root helper; the portal never names a
        // service to PID 1.
        assert_eq!(
            Action::Enable.systemctl_args("tesserad.service"),
            ["start", H_ENABLE]
        );
        assert_eq!(
            Action::Disable.systemctl_args("tesserad.service"),
            ["start", H_DISABLE]
        );
        assert_eq!(
            Action::Restart.systemctl_args("tesserad.service"),
            ["start", H_RESTART]
        );
        // The portal's own unit: do not wait for the stop.
        assert_eq!(
            Action::Restart.systemctl_args("tessera-web.service"),
            [
                "start",
                "--no-block",
                "tessera-web-unitfile@restart-tessera-web.service"
            ]
        );
        assert_eq!(
            Action::Disable.systemctl_args("tessera-web.service"),
            [
                "start",
                "--no-block",
                "tessera-web-unitfile@disable-tessera-web.service"
            ]
        );
        assert_eq!(
            Action::Enable.systemctl_args("tessera-web.service"),
            ["start", "tessera-web-unitfile@enable-tessera-web.service"]
        );
        assert!(Action::Restart.stops_own_unit("tessera-web.service"));
        assert!(!Action::Enable.stops_own_unit("tessera-web.service"));
        assert!(!Action::Restart.stops_own_unit("tesserad.service"));
        assert_eq!(Action::parse("enable"), Some(Action::Enable));
        assert_eq!(Action::parse("stop"), None);
        assert_eq!(Action::parse(""), None);
        assert!(Action::Disable.asks_when_essential() && Action::Restart.asks_when_essential());
        assert!(!Action::Enable.asks_when_essential());
    }

    #[test]
    fn instances_escape_and_round_trip() {
        assert_eq!(instance(HelperVerb::Enable, "tesserad"), "enable-tesserad");
        assert_eq!(
            instance(HelperVerb::Disable, "bluealsa-aplay.service"),
            "disable-bluealsa-aplay"
        );
        assert_eq!(
            instance(HelperVerb::Restart, "tesserad"),
            "restart-tesserad"
        );
        assert_eq!(
            instance(HelperVerb::Enable, "getty@tty1.service"),
            "enable-getty\\x40tty1"
        );
        assert_eq!(
            helper_unit(HelperVerb::Enable, "tesserad.service"),
            H_ENABLE
        );
        // Every character an allowlisted name may contain survives the trip.
        let alphabet: String = ('a'..='z')
            .chain('A'..='Z')
            .chain('0'..='9')
            .chain(['_', '-'])
            .collect();
        for name in [
            alphabet.as_str(),
            "a-b_c",
            "getty@tty1",
            "x@y-z_9",
            "a--b",
            "A1@B2",
        ] {
            assert!(crate::config::valid_unit_name(name), "{name}");
            for v in HelperVerb::ALL {
                let i = instance(v, name);
                assert!(
                    i.bytes().all(|b| b.is_ascii_alphanumeric()
                        || matches!(b, b'_' | b'.' | b'-' | b'\\' | b'x')),
                    "{i}"
                );
                assert_eq!(
                    parse_instance(&i).unwrap(),
                    (v, format!("{name}.service")),
                    "{name}"
                );
            }
        }
        // `%i` after systemd's unescaping of the command line: a literal `@`
        // is the same instance as its `\x40` spelling.
        assert_eq!(
            parse_instance("enable-getty@tty1").unwrap(),
            (HelperVerb::Enable, "getty@tty1.service".into())
        );
        assert_eq!(
            parse_instance("enable-getty\\x40tty1").unwrap(),
            (HelperVerb::Enable, "getty@tty1.service".into())
        );
        for bad in [
            "",
            "enable",
            "enable-",
            "-tesserad",
            "--now",
            "start-tesserad",
            "stop-tesserad",
            "mask-tesserad",
            "enable-tesserad.service",
            "enable-tesserad.service.service",
            "enable-tessera d",
            "enable-tessera/d",
            "enable-tesserad@",
            "enable-@tty1",
            "enable-a@b@c",
            "enable-a@b\\x40c",
            "enable-tesserad\\x",
            "enable-tesserad\\x4",
            "enable-tesserad\\x4g",
            "enable-tesserad\\x61",
            "enable-tesserad\\x00",
            "enable-tesserad\\x2f",
            "enable-.hidden",
            "enable--rf",
            "enable-a.timer",
            "enable-tesserad\n",
            "enable-ünit",
        ] {
            assert!(parse_instance(bad).is_err(), "{bad:?}");
        }
        // The helper unit name must fit a unit name (255 bytes).
        let long = "a".repeat(218);
        assert!(crate::config::valid_unit_name(&long));
        assert_eq!(helper_unit(HelperVerb::Disable, &long).len(), 255);
        assert!(!crate::config::valid_unit_name(&"a".repeat(219)));
        assert!(!crate::config::valid_unit_name(&format!(
            "{}@x",
            "a".repeat(216)
        )));
    }

    #[tokio::test]
    async fn the_helper_checks_the_allowlist_again_and_runs_exactly_one_command() {
        let c = cfg(vec![entry(&["tesserad"], true), entry(&["a", "b"], false)]);
        let r = FakeRunner::default();
        // The three root commands, argv-exact.
        r.script(
            &[
                SYSTEMCTL,
                "enable",
                "--now",
                "--no-reload",
                "tesserad.service",
            ],
            Output::ok(""),
        );
        r.script(
            &[SYSTEMCTL, "disable", "--now", "--no-reload", "a.service"],
            Output::ok(""),
        );
        r.script(&[SYSTEMCTL, "restart", "b.service"], Output::ok(""));
        assert_eq!(run_unitfile(&r, &c, "enable-tesserad").await, Ok(0));
        assert_eq!(run_unitfile(&r, &c, "disable-a").await, Ok(0));
        assert_eq!(run_unitfile(&r, &c, "restart-b").await, Ok(0));
        assert_eq!(
            r.calls(),
            [
                argv(&[
                    SYSTEMCTL,
                    "enable",
                    "--now",
                    "--no-reload",
                    "tesserad.service"
                ]),
                argv(&[SYSTEMCTL, "disable", "--now", "--no-reload", "a.service"]),
                argv(&[SYSTEMCTL, "restart", "b.service"]),
            ]
        );
        // The exit code of systemctl is handed back.
        r.clear_calls();
        r.script(
            &[SYSTEMCTL, "disable", "--now", "--no-reload", "b.service"],
            Output::fail(5, "Failed to disable unit: ...\n"),
        );
        assert_eq!(run_unitfile(&r, &c, "disable-b").await, Ok(5));
        assert_eq!(r.calls().len(), 1);
        // Refusals run nothing: not allowlisted, unknown verb, malformed,
        // empty name, double `@`, bad escape, double suffix, an option.
        r.clear_calls();
        for bad in [
            "enable-sshd",
            "enable-tessera-web",
            "disable-debug-shell",
            "restart-sshd",
            "start-tesserad",
            "mask-tesserad",
            "enable-",
            "enable-a@b@c",
            "enable-tesserad\\x4g",
            "enable-tesserad\\x2fx",
            "enable-tesserad.service",
            "enable-tesserad.service.service",
            "--now",
            "enable---now",
            "",
        ] {
            let e = run_unitfile(&r, &c, bad).await.unwrap_err();
            assert!(!e.is_empty(), "{bad:?}");
        }
        assert_eq!(
            run_unitfile(&r, &c, "enable-sshd").await.unwrap_err(),
            "`sshd.service` is not an [[appliance.service]] unit"
        );
        assert_eq!(
            run_unitfile(&r, &c, "mask-tesserad").await.unwrap_err(),
            "instance `mask-tesserad`: the verb must be `enable`, `disable` or `restart`"
        );
        let off = ApplianceConfig {
            enabled: false,
            ..c.clone()
        };
        assert_eq!(
            run_unitfile(&r, &off, "enable-tesserad").await.unwrap_err(),
            "appliance mode is off ([appliance] enabled = false)"
        );
        assert!(r.calls().is_empty(), "{:?}", r.calls());
        // A systemctl that cannot run is an error, not an exit code.
        r.script_error(
            &[SYSTEMCTL, "enable", "--now", "--no-reload", "a.service"],
            RunError::Timeout,
        );
        assert!(run_unitfile(&r, &c, "enable-a").await.is_err());
        assert_eq!(
            unitfile_args(HelperVerb::Disable, "a.service"),
            ["disable", "--now", "--no-reload", "a.service"]
        );
        assert_eq!(
            unitfile_args(HelperVerb::Restart, "a.service"),
            ["restart", "a.service"]
        );
    }

    #[test]
    fn allowlist_matches_keys_only() {
        let c = cfg(vec![entry(&["tesserad"], true), entry(&["a", "b"], false)]);
        assert!(allowlisted(&c, "tesserad").is_some());
        assert!(allowlisted(&c, "a+b").is_some());
        for bad in [
            "a",
            "b+a",
            "tesserad.service",
            "tesserad; rm -rf /",
            "",
            "--now",
            "-x",
            "tesserad\n",
            "a+b+c",
        ] {
            assert!(allowlisted(&c, bad).is_none(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn perform_runs_each_unit_and_stops_at_the_first_failure() {
        let r = FakeRunner::default();
        let e = entry(&["a", "b", "c"], false);
        let ha = "tessera-web-unitfile@enable-a.service";
        let hb = "tessera-web-unitfile@enable-b.service";
        r.script(&[SYSTEMCTL, "start", ha], Output::ok(""));
        r.script(
            &[SYSTEMCTL, "start", hb],
            Output::fail(1, "Job for tessera-web-unitfile@enable-b.service failed because the control process exited with error code.\n"),
        );
        let err = perform(&r, &e, Action::Enable, None).await.unwrap_err();
        assert_eq!(
            err,
            "systemctl enable b.service: Job for tessera-web-unitfile@enable-b.service failed because the control process exited with error code. (details: journalctl -u tessera-web-unitfile@enable-b.service) (done before it: a.service)"
        );
        assert_eq!(
            r.calls(),
            [
                argv(&[SYSTEMCTL, "start", ha]),
                argv(&[SYSTEMCTL, "start", hb])
            ]
        );
        r.clear_calls();
        let ra = "tessera-web-unitfile@restart-a.service";
        r.script(&[SYSTEMCTL, "start", ra], Output::ok(""));
        assert_eq!(
            perform(&r, &entry(&["a"], false), Action::Restart, None).await,
            Ok(())
        );
        let e = perform(&r, &entry(&["zzz"], false), Action::Restart, None)
            .await
            .unwrap_err();
        assert_eq!(
            e,
            "systemctl restart zzz.service: unscripted command (details: journalctl -u tessera-web-unitfile@restart-zzz.service)"
        );
        // A static unit is skipped by enable and disable, not by restart.
        let r = FakeRunner::default();
        let e = entry(&["s", "a"], false);
        let row = Row {
            entry: e.clone(),
            units: vec![st("s", Enabled::Static, true), st("a", Enabled::On, true)],
        };
        let da = "tessera-web-unitfile@disable-a.service";
        r.script(&[SYSTEMCTL, "start", da], Output::ok(""));
        assert_eq!(perform(&r, &e, Action::Disable, Some(&row)).await, Ok(()));
        assert_eq!(r.calls(), [argv(&[SYSTEMCTL, "start", da])]);
        // The portal's own unit goes last in a group, without waiting.
        let r = FakeRunner::default();
        let e = entry(&["tessera-web", "tesserad"], false);
        let rw = "tessera-web-unitfile@restart-tessera-web.service";
        r.script(&[SYSTEMCTL, "start", H_RESTART], Output::ok(""));
        r.script(&[SYSTEMCTL, "start", "--no-block", rw], Output::ok(""));
        assert_eq!(perform(&r, &e, Action::Restart, None).await, Ok(()));
        assert_eq!(
            r.calls(),
            [
                argv(&[SYSTEMCTL, "start", H_RESTART]),
                argv(&[SYSTEMCTL, "start", "--no-block", rw]),
            ]
        );
        // A timeout is explained as such.
        let r = FakeRunner::default();
        r.script_error(&[SYSTEMCTL, "start", ra], RunError::Timeout);
        let e = perform(&r, &entry(&["a"], false), Action::Restart, None)
            .await
            .unwrap_err();
        assert_eq!(
            e,
            "systemctl restart a.service: the portal stopped waiting after 10 s; the action may still complete (details: journalctl -u tessera-web-unitfile@restart-a.service)"
        );
    }

    #[test]
    fn stderr_is_sanitised_and_bounded() {
        assert_eq!(sanitize("a\x1b[31mb\n\n  c  \n"), "ab; c");
        assert_eq!(sanitize("x\x1b]0;title\x07y\x1b]8;;http://e\x1b\\z"), "xyz");
        assert_eq!(sanitize("\x1bMq\tr\r"), "qr");
        let long = "x".repeat(400);
        let s = sanitize(&long);
        assert_eq!(s.chars().count(), STDERR_MAX + 1);
        assert!(s.ends_with('…'));
        assert_eq!(sanitize(""), "");
        assert!(bounded("y".repeat(1000)).chars().count() <= STDERR_MAX + 1);
    }

    /// The whole file for a two-unit configuration.
    pub(crate) const RULES_TWO_UNITS: &str = "\
// polkit rules for tessera-web appliance mode: what the portal's user may
// ask systemd for. Generated by `tessera-web appliance polkit` from the
// [[appliance.service]] entries of /etc/tessera/web.toml; regenerate
// after changing them. Install:
//   f=$(mktemp) && tessera-web appliance polkit > \"$f\" \\
//     && sudo install -m 0644 -o root -g root \"$f\" /etc/polkit-1/rules.d/49-tessera-web-appliance.rules \\
//     && rm -f \"$f\"
polkit.addRule(function(action, subject) {
    if (subject.user != \"tessera-web\" || action.id != \"org.freedesktop.systemd1.manage-units\") {
        return polkit.Result.NOT_HANDLED;
    }
    var helpers = [\"tessera-web-unitfile@enable-tesserad.service\", \"tessera-web-unitfile@disable-tesserad.service\", \"tessera-web-unitfile@restart-tesserad.service\", \"tessera-web-unitfile@enable-shairport-sync.service\", \"tessera-web-unitfile@disable-shairport-sync.service\", \"tessera-web-unitfile@restart-shairport-sync.service\"];
    if (helpers.indexOf(action.lookup(\"unit\")) >= 0 && action.lookup(\"verb\") == \"start\") {
        return polkit.Result.YES;
    }
    return polkit.Result.NOT_HANDLED;
});
";

    #[test]
    fn polkit_rules_are_generated_exactly() {
        let c = cfg(vec![
            entry(&["tesserad"], true),
            entry(&["shairport-sync.service"], false),
        ]);
        assert_eq!(polkit_rules(&c), RULES_TWO_UNITS);
        // Nothing but helper starts: no service name, no unit-file action.
        assert!(!RULES_TWO_UNITS.contains("manage-unit-files"));
        assert!(!RULES_TWO_UNITS.contains("\"tesserad.service\""));
        assert!(!RULES_TWO_UNITS.contains("\"restart\""));
        // A template instance with an escaped name is quoted as JavaScript.
        let g = polkit_rules(&cfg(vec![entry(&["getty@tty1"], false)]));
        assert_eq!(
            helper_units(&cfg(vec![entry(&["getty@tty1"], false)])),
            [
                "tessera-web-unitfile@enable-getty\\x40tty1.service",
                "tessera-web-unitfile@disable-getty\\x40tty1.service",
                "tessera-web-unitfile@restart-getty\\x40tty1.service",
            ]
        );
        assert!(
            g.contains("var helpers = [\"tessera-web-unitfile@enable-getty\\\\x40tty1.service\", \"tessera-web-unitfile@disable-getty\\\\x40tty1.service\", \"tessera-web-unitfile@restart-getty\\\\x40tty1.service\"];"),
            "{g}"
        );
        // No entries: nothing is granted.
        let empty = polkit_rules(&cfg(vec![]));
        assert!(empty.contains("var helpers = [];"), "{empty}");
        assert_eq!(js_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\u000a\"");
    }
}
