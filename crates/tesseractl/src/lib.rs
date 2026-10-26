//! `tesseractl`: command-line control of the tessera daemon.
//!
//! Talks to `control.sock` with length-delimited JSON frames (ADR 0008) using
//! the types of `tessera-control-types`. Exit codes: 0 ok, 1 error, 2 usage,
//! 3 cannot connect, 4 permission denied.
//!
//! The library exists so the binary and its tests share code; it is not a
//! public API.
#![warn(missing_docs)]

pub mod cli;
pub mod client;
pub mod output;
pub mod sanitize;

use std::io::{self, BufRead, IsTerminal, Write};
use std::time::{Duration, Instant};

use clap::CommandFactory;
use tessera_control_types::{
    Command, CommandError, DeviceKey, Impact, Query, Reply, RequestBody, Status,
};
use tessera_state::{ConfigPaths, Environment, load_config};

use crate::cli::{
    Cli, ClockCmd, Cmd, ConfigCmd, DirArgs, FlowCmd, NetworkCmd, ShowArgs, WatchArgs,
};
use crate::client::{Client, ClientError};

/// Exit code: success.
pub const EXIT_OK: u8 = 0;
/// Exit code: the command failed.
pub const EXIT_ERROR: u8 = 1;
/// Exit code: bad command line (the code `clap` uses).
pub const EXIT_USAGE: u8 = 2;
/// Exit code: the daemon could not be reached.
pub const EXIT_CONNECT: u8 = 3;
/// Exit code: not allowed.
pub const EXIT_DENIED: u8 = 4;

/// Why a command did not succeed.
#[derive(Debug)]
pub enum CliError {
    /// The arguments make no sense in this situation.
    Usage(String),
    /// Talking to the daemon failed or was refused.
    Client(ClientError),
    /// Reading the configuration files failed.
    Config(String),
    /// A change that restarts something was not confirmed (the answer was
    /// no, there was nobody to ask, or the impact kept changing). Nothing
    /// was changed.
    NotConfirmed(String),
    /// Standard output was closed (for example `| head`): not an error.
    BrokenPipe,
}

impl From<ClientError> for CliError {
    fn from(e: ClientError) -> Self {
        Self::Client(e)
    }
}

impl From<io::Error> for CliError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::BrokenPipe {
            Self::BrokenPipe
        } else {
            Self::Config(e.to_string())
        }
    }
}

/// The process exit code for an error.
#[must_use]
pub fn exit_code(e: &CliError) -> u8 {
    match e {
        CliError::Usage(_) => EXIT_USAGE,
        CliError::BrokenPipe => EXIT_OK,
        CliError::Config(_) | CliError::NotConfirmed(_) => EXIT_ERROR,
        CliError::Client(ClientError::Connect { source, .. }) => {
            if source.kind() == io::ErrorKind::PermissionDenied {
                EXIT_DENIED
            } else {
                EXIT_CONNECT
            }
        }
        CliError::Client(ClientError::Command(CommandError::PermissionDenied)) => EXIT_DENIED,
        CliError::Client(_) => EXIT_ERROR,
    }
}

/// The message to print for an error, with a hint where one helps.
#[must_use]
pub fn describe(e: &CliError) -> String {
    match e {
        CliError::Usage(m) | CliError::Config(m) | CliError::NotConfirmed(m) => m.clone(),
        CliError::BrokenPipe => String::new(),
        CliError::Client(ClientError::Connect { path, source }) => match source.kind() {
            io::ErrorKind::PermissionDenied => format!(
                "permission denied opening {}: your user needs to be in the daemon's socket \
                 group (`tessera` by default), and also in the admin group \
                 (`tessera-admin` by default) to change settings",
                path.display()
            ),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => format!(
                "cannot connect to {}: {source}\nis tesserad running? use --socket or \
                 TESSERA_SOCKET if it listens elsewhere",
                path.display()
            ),
            _ => format!("cannot connect to {}: {source}", path.display()),
        },
        CliError::Client(ClientError::Command(CommandError::PermissionDenied)) => {
            "permission denied: changing settings needs root or membership of the daemon's \
             admin group (`tessera-admin` by default); reading status does not"
                .into()
        }
        CliError::Client(
            e @ ClientError::ResponseTooLarge {
                limit,
                daemon_limit: Some(daemon),
                ..
            },
        ) if *daemon > *limit as u64 => format!(
            "{e}\nthe daemon sends frames of up to {daemon} bytes; raise --max-response-bytes"
        ),
        CliError::Client(e) => e.to_string(),
    }
}

/// Where answers to confirmation questions come from.
pub struct Input<'a> {
    /// The lines the user types.
    pub reader: &'a mut dyn BufRead,
    /// Is there a person to ask (standard input and standard error are
    /// terminals)? When not, a change that needs confirmation is refused
    /// unless `--yes` was given.
    pub interactive: bool,
}

/// Standard input, locked only when a question is actually asked.
struct LazyStdin(Option<io::StdinLock<'static>>);

impl LazyStdin {
    fn lock(&mut self) -> &mut io::StdinLock<'static> {
        self.0.get_or_insert_with(|| io::stdin().lock())
    }
}

impl io::Read for LazyStdin {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.lock().read(buf)
    }
}

impl BufRead for LazyStdin {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.lock().fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        self.lock().consume(amt);
    }
}

/// How often a changing impact is shown again before giving up.
const CONFIRM_ROUNDS: u32 = 3;

/// Execute a parsed command line, asking on the terminal when a change needs
/// confirmation. Normal output goes to `out`, notes, warnings and questions
/// to `err`.
///
/// # Errors
/// [`CliError`]; map it with [`exit_code`] and [`describe`].
pub fn run(cli: &Cli, out: &mut dyn Write, err: &mut dyn Write) -> Result<(), CliError> {
    let interactive = io::stdin().is_terminal() && io::stderr().is_terminal();
    run_with(
        cli,
        out,
        err,
        &mut Input {
            reader: &mut LazyStdin(None),
            interactive,
        },
    )
}

/// [`run`] with the confirmation input given (tests, other front ends).
///
/// # Errors
/// [`CliError`]; map it with [`exit_code`] and [`describe`].
pub fn run_with(
    cli: &Cli,
    out: &mut dyn Write,
    err: &mut dyn Write,
    input: &mut Input<'_>,
) -> Result<(), CliError> {
    match &cli.command {
        Cmd::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(*shell, &mut cmd, "tesseractl", out);
            return Ok(());
        }
        Cmd::Config(ConfigCmd::Show(a)) if !a.effective => {
            return config_files(a, cli.json, out, err);
        }
        Cmd::Config(ConfigCmd::Check(d)) => return config_check(d, cli.json, out, err),
        _ => {}
    }
    let connect = || {
        Client::connect(
            &cli.socket,
            Duration::from_secs(cli.timeout),
            cli.max_response_bytes,
        )
    };
    let mut c = connect()?;
    match &cli.command {
        Cmd::Status => {
            if let Some(dev) = &cli.device {
                let reply = c.request(RequestBody::Query(Query::Device {
                    device: DeviceKey(dev.clone()),
                }))?;
                let Reply::Device(d) = reply else {
                    return Err(unexpected(&reply));
                };
                if cli.json {
                    json(out, &d)?;
                } else {
                    write!(out, "{}", output::render_device(&d))?;
                }
            } else {
                let s = c.status()?;
                if cli.json {
                    json(out, &s)?;
                } else {
                    write!(out, "{}", output::render_status(&s))?;
                }
            }
        }
        Cmd::Metrics => {
            let reply = c.request(RequestBody::Query(Query::Metrics))?;
            let Reply::Metrics(m) = reply else {
                return Err(unexpected(&reply));
            };
            if cli.json {
                json(out, &m)?;
            } else {
                write!(out, "{}", output::render_metrics(&m))?;
            }
        }
        Cmd::Flows(FlowCmd::Show) => {
            let reply = c.request(RequestBody::Query(Query::Metrics))?;
            let Reply::Metrics(metrics) = reply else {
                return Err(unexpected(&reply));
            };
            if cli.json {
                json(out, &metrics.flows)?;
            } else if metrics.flows.is_empty() {
                writeln!(out, "No flows.")?;
            } else {
                write!(out, "{}", output::render_flows(&metrics.flows))?;
            }
        }
        Cmd::Bridges => {
            let reply = c.request(RequestBody::Query(Query::Bridges))?;
            let Reply::Bridges(b) = reply else {
                return Err(unexpected(&reply));
            };
            if cli.json {
                json(out, &b)?;
            } else if b.is_empty() {
                writeln!(out, "No bridges.")?;
            } else {
                write!(out, "{}", output::render_bridges(&b))?;
            }
        }
        Cmd::Watch(a) => watch(&mut c, a, cli.json, out, &|| connect()?.status())?,
        Cmd::Clock(ClockCmd::Show) => {
            let s = c.status()?;
            if cli.json {
                json(
                    out,
                    &serde_json::json!({ "settings": s.clock_settings, "clocks": s.clocks }),
                )?;
            } else {
                if let Some(settings) = &s.clock_settings {
                    writeln!(
                        out,
                        "Clock settings: {}",
                        output::render_clock_settings(settings)
                    )?;
                }
                if s.clocks.is_empty() {
                    writeln!(out, "No clocks.")?;
                } else {
                    write!(out, "{}", output::render_clocks(&s.clocks))?;
                }
            }
        }
        Cmd::Network(NetworkCmd::Show) => {
            let s = c.status()?;
            match (&s.network, cli.json) {
                (n, true) => json(out, n)?,
                (Some(n), false) => write!(out, "{}", output::render_network(n))?,
                (None, false) => writeln!(out, "The daemon has not reported its interfaces yet.")?,
            }
        }
        Cmd::Config(ConfigCmd::Show(_)) => {
            let reply = c.request(RequestBody::Query(Query::EffectiveConfig))?;
            let Reply::EffectiveConfig(text) = reply else {
                return Err(unexpected(&reply));
            };
            if cli.json {
                json(out, &serde_json::json!({ "effective_config": text }))?;
            } else {
                write!(out, "{}", sanitize::clean_text(&text))?;
            }
        }
        Cmd::Completions { .. } | Cmd::Config(ConfigCmd::Check(_)) => {}
        cmd => {
            let device = if cli::needs_device(cmd) {
                resolve_device(&mut c, cli.device.as_deref())?
            } else {
                DeviceKey(String::new())
            };
            let command = if let Cmd::Flows(FlowCmd::Set { file }) = cmd {
                let text = std::fs::read_to_string(file)?;
                let flows =
                    tessera_state::flows::parse_flow_specs(&text).map_err(CliError::Config)?;
                Command::SetProtocolFlows { flows }
            } else {
                cli::to_command(cmd, &device)
                    .ok_or_else(|| CliError::Usage("this command is not implemented".into()))?
            };
            let mut seen = None;
            let mut command = command;
            if let Cmd::SetChannels { rx, tx, dry_run } = cmd {
                // Look first (unless told to go ahead blindly): the dry run
                // shows what the change does and gives the token to confirm.
                command = with_dry_run(&command, false);
                if *dry_run {
                    // Only ever a dry run, whatever else is set (`--yes`
                    // included): nothing below this block is reached.
                    let impact = probe_impact(&mut c, &command)?;
                    dry_run_output(cli.json, &device, (*rx, *tx), &impact, out)?;
                    out.flush()?;
                    return Ok(());
                }
                if !cli.yes {
                    seen = Some(probe_impact(&mut c, &command)?);
                }
            }
            let reply = apply(&mut c, cli, &command, seen, err, input)?;
            match reply {
                Reply::Done | Reply::Impact(_) => {
                    let impact = match &reply {
                        Reply::Impact(i) => Some(i),
                        _ => None,
                    };
                    if cli.json {
                        let v = match impact {
                            Some(i) => serde_json::json!({ "ok": true, "impact": i }),
                            None => serde_json::json!({ "ok": true }),
                        };
                        writeln!(out, "{v}")?;
                    } else {
                        writeln!(out, "{}", confirmation(cmd, &device))?;
                        if let Some(i) = impact {
                            let after = sanitize::clean(&i.restart.afterwards);
                            if !after.is_empty() {
                                writeln!(out, "{after}")?;
                            }
                        }
                    }
                }
                other => return Err(unexpected(&other)),
            }
        }
    }
    out.flush()?;
    Ok(())
}

/// The same `SetDeviceChannels` with another `dry_run`.
fn with_dry_run(c: &Command, dry_run: bool) -> Command {
    match c {
        Command::SetDeviceChannels { device, rx, tx, .. } => Command::SetDeviceChannels {
            device: device.clone(),
            rx: *rx,
            tx: *tx,
            dry_run,
        },
        other => other.clone(),
    }
}

/// Ask the daemon what `command` (a `SetDeviceChannels`) would do. Sends a
/// dry run, never the command itself.
fn probe_impact(c: &mut Client, command: &Command) -> Result<Impact, CliError> {
    match c.request(RequestBody::Command(with_dry_run(command, true)))? {
        Reply::Impact(impact) => Ok(impact),
        _ => Err(CliError::Client(ClientError::Protocol(
            "a dry run was not answered with an impact".into(),
        ))),
    }
}

/// What `set-channels --dry-run` prints.
fn dry_run_output(
    as_json: bool,
    device: &DeviceKey,
    (rx, tx): (u16, u16),
    impact: &Impact,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    if as_json {
        json(
            out,
            &serde_json::json!({ "dry_run": true, "impact": impact }),
        )?;
    } else {
        writeln!(
            out,
            "{}: dry run of rx {rx}, tx {tx}; nothing was changed.",
            sanitize::clean(&device.0)
        )?;
        writeln!(out, "{}", sanitize::cleaned(impact).describe())?;
    }
    Ok(())
}

/// Send `command`. If the daemon says it restarts something (`seen`: the
/// impact a dry run already showed), show the impact and ask the user;
/// send it again as confirmed with the impact's token, so that a changed
/// situation is refused and shown again instead of applied unseen. With
/// `--yes` it is sent confirmed at once, for whatever the impact is.
fn apply(
    c: &mut Client,
    cli: &Cli,
    command: &Command,
    seen: Option<Impact>,
    err: &mut dyn Write,
    input: &mut Input<'_>,
) -> Result<Reply, CliError> {
    if cli.yes {
        return Ok(c.request(RequestBody::confirmed(command.clone(), None))?);
    }
    let mut impact = match seen {
        Some(i) if i.needs_confirmation() => i,
        // Nothing to confirm as far as the dry run saw (or no dry run was
        // made): send it; the daemon has the last word.
        _ => match c.request(RequestBody::Command(command.clone())) {
            Err(ClientError::Command(CommandError::ConfirmationRequired { impact })) => *impact,
            other => return Ok(other?),
        },
    };
    // A broken standard error means the user cannot see what they would
    // confirm: that is "not confirmed", never a success.
    let unseen = |e: io::Error| {
        CliError::NotConfirmed(format!(
            "cannot show the question ({e}); nothing was changed"
        ))
    };
    for round in 1..=CONFIRM_ROUNDS {
        writeln!(err, "{}", sanitize::cleaned(&impact).describe()).map_err(unseen)?;
        if impact.token.is_none() {
            // Without a digest "what was shown" cannot be pinned down.
            return Err(CliError::NotConfirmed(
                "the daemon sent no impact token, so this change cannot be confirmed safely; \
                 nothing was changed. Use --yes to apply it anyway."
                    .into(),
            ));
        }
        if !input.interactive {
            return Err(CliError::NotConfirmed(
                "this change restarts something and needs your confirmation, but there is no \
                 terminal to ask on; nothing was changed. Run it again with --yes to apply it \
                 without asking."
                    .into(),
            ));
        }
        write!(err, "Apply this change? [y/N] ").map_err(unseen)?;
        err.flush().map_err(unseen)?;
        let mut line = String::new();
        let n = input.reader.read_line(&mut line).map_err(unseen)?;
        if n == 0 || !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            return Err(CliError::NotConfirmed(
                "not confirmed; nothing was changed".into(),
            ));
        }
        match c.request(RequestBody::confirmed(
            command.clone(),
            impact.token.clone(),
        )) {
            Err(ClientError::Command(CommandError::ConfirmationRequired { impact: now })) => {
                if round == CONFIRM_ROUNDS {
                    break;
                }
                writeln!(
                    err,
                    "The situation changed since the impact above was computed; nothing was \
                     applied. It is now:"
                )
                .map_err(unseen)?;
                impact = *now;
            }
            other => return Ok(other?),
        }
    }
    Err(CliError::NotConfirmed(
        "the impact of the change kept changing; nothing was applied".into(),
    ))
}

fn unexpected(r: &Reply) -> CliError {
    CliError::Client(ClientError::Protocol(format!("unexpected reply {r:?}")))
}

fn json<T: serde::Serialize>(out: &mut dyn Write, v: &T) -> Result<(), CliError> {
    let s = serde_json::to_string_pretty(v).map_err(|e| CliError::Config(e.to_string()))?;
    writeln!(out, "{s}")?;
    Ok(())
}

/// Pick the device a command applies to: `--device`, else the only one.
fn resolve_device(c: &mut Client, wanted: Option<&str>) -> Result<DeviceKey, CliError> {
    if let Some(d) = wanted {
        return Ok(DeviceKey(d.to_owned()));
    }
    let Status { devices, .. } = c.status()?;
    match devices.as_slice() {
        [only] => Ok(only.key.clone()),
        [] => Err(CliError::Usage("the daemon has no devices".into())),
        many => Err(CliError::Usage(format!(
            "the daemon has several devices ({}); pick one with --device",
            many.iter()
                .map(|d| d.key.0.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn confirmation(cmd: &Cmd, device: &DeviceKey) -> String {
    let d = &device.0;
    match cmd {
        Cmd::Flows(FlowCmd::Set { .. }) => "Controller flow set replaced.".into(),
        Cmd::Flows(FlowCmd::Clear) => "Controller flow set cleared.".into(),
        Cmd::SetName { name } => format!("{d}: name set to \"{name}\""),
        Cmd::SetRate { hz } => format!("{d}: sample rate set to {hz} Hz"),
        Cmd::SetEncoding { bits } => format!("{d}: encoding set to {bits} bit"),
        Cmd::SetLatency { micros } => format!("{d}: latency set to {micros} us"),
        Cmd::SetChannels { rx, tx, .. } => format!("{d}: channels set to rx {rx}, tx {tx}"),
        Cmd::SetLabel {
            reset: true,
            direction,
            channel,
            ..
        } => format!("{d}: {direction:?} channel {channel} label reset").to_lowercase(),
        Cmd::SetLabel {
            direction,
            channel,
            label,
            ..
        } => format!(
            "{d}: {} channel {channel} label set to \"{}\"",
            format!("{direction:?}").to_lowercase(),
            label.as_deref().unwrap_or("")
        ),
        Cmd::Subscribe {
            rx_channel,
            tx_channel,
            tx_device,
        } => {
            format!("{d}: rx {rx_channel} subscribed to {tx_channel} @ {tx_device}")
        }
        Cmd::Unsubscribe { rx_channel } => format!("{d}: rx {rx_channel} unsubscribed"),
        Cmd::Clock(ClockCmd::SetRole { role }) => {
            format!("clock role set to {role:?}").to_lowercase()
        }
        Cmd::Clock(ClockCmd::SetPriority { priority }) => {
            format!("clock priority set to {priority}")
        }
        Cmd::Network(NetworkCmd::Primary { interface }) => match crate::cli::primary_arg(interface)
        {
            Some(i) => format!("primary interface selected: {i}"),
            None => "primary interface selection cleared".into(),
        },
        Cmd::Config(ConfigCmd::Reload) => "configuration reloaded".into(),
        _ => "ok".into(),
    }
}

fn watch(
    c: &mut Client,
    a: &WatchArgs,
    as_json: bool,
    out: &mut dyn Write,
    resync: &dyn Fn() -> Result<Status, ClientError>,
) -> Result<(), CliError> {
    c.send(RequestBody::SubscribeEvents)?;
    c.read_response()?.result.map_err(ClientError::Command)?;
    let deadline = a.for_secs.map(|s| Instant::now() + Duration::from_secs(s));
    if deadline.is_none() {
        c.set_read_timeout(None)?;
    }
    let mut seen = 0u64;
    while a.count.is_none_or(|max| seen < max) {
        if let Some(d) = deadline {
            let left = d.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            c.set_read_timeout(Some(left))?;
        }
        let resp = match c.read_response() {
            Ok(r) => r,
            Err(ClientError::Timeout) if deadline.is_some() => break,
            Err(e) => return Err(e.into()),
        };
        match resp.result.map_err(ClientError::Command)? {
            Reply::Event(e) => {
                seen += 1;
                if as_json {
                    writeln!(out, "{}", to_line(&Reply::Event(e))?)?;
                } else {
                    writeln!(out, "{}", output::render_event(&e))?;
                }
            }
            Reply::Lagged { dropped } => {
                // This client fell behind. Say so; in the human view also show
                // where things stand now (read on a second connection: this
                // one is an event stream).
                if as_json {
                    writeln!(out, "{}", to_line(&Reply::Lagged { dropped })?)?;
                } else {
                    writeln!(
                        out,
                        "-- {dropped} events were dropped; current state follows --"
                    )?;
                    match resync() {
                        Ok(s) => write!(out, "{}", output::render_status(&s))?,
                        Err(e) => writeln!(out, "(cannot read the current state: {e})")?,
                    }
                }
            }
            _ => {}
        }
        out.flush()?;
    }
    Ok(())
}

fn to_line(r: &Reply) -> Result<String, CliError> {
    serde_json::to_string(r).map_err(|e| CliError::Config(e.to_string()))
}

fn load_files(d: &DirArgs) -> Result<tessera_state::LoadedConfig, CliError> {
    let paths = ConfigPaths::new(
        d.config_dir.join("config.toml"),
        d.config_dir.join("config.d"),
    );
    // The schema is checked here; whether the installed daemon was built
    // with the hardware bridge only the daemon knows (it refuses
    // `[[bridge]]` at reload if not), so the offline check accepts them.
    let mut env = Environment::detect();
    env.bridges_supported = true;
    load_config(&paths, &env).map_err(|e| CliError::Config(e.to_string()))
}

fn config_files(
    a: &ShowArgs,
    as_json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CliError> {
    let loaded = load_files(&a.dir)?;
    for w in &loaded.warnings {
        writeln!(err, "{w}")?;
    }
    if as_json {
        json(out, &loaded.config)?;
        return Ok(());
    }
    let text = loaded.config.to_toml().map_err(CliError::Config)?;
    writeln!(
        out,
        "# merged configuration files (the daemon may use other values stored in its state; see `config show --effective`)"
    )?;
    write!(out, "{text}")?;
    if a.provenance {
        writeln!(out, "\n# provenance: which file set each key")?;
        for (key, file) in loaded.provenance.iter() {
            writeln!(out, "# {key} <- {}", file.display())?;
        }
    }
    Ok(())
}

fn config_check(
    d: &DirArgs,
    as_json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CliError> {
    let loaded = load_files(d)?;
    for w in &loaded.warnings {
        writeln!(err, "{w}")?;
    }
    if as_json {
        writeln!(
            out,
            "{{\"ok\":true,\"files\":{},\"warnings\":{}}}",
            loaded.files.len(),
            loaded.warnings.len()
        )?;
    } else {
        writeln!(
            out,
            "configuration OK ({} file(s), {} warning(s))",
            loaded.files.len(),
            loaded.warnings.len()
        )?;
    }
    Ok(())
}
