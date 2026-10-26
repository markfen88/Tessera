//! Trusted bundle lifecycle. Production paths are constant; `Paths` and `Runner`
//! make the same lifecycle testable without root or host mutations.
pub mod data;
pub mod dsp;
mod lifecycle;
pub mod manifest;
pub mod pcm;
pub mod polkit;
pub mod portal;
pub mod record;
pub mod release;

use std::fmt;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self(error.to_string())
    }
}

pub(crate) fn ensure(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(Error(message.into()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub action: String,
    pub profile: String,
    pub card: String,
    pub no_dsp: bool,
    pub admins: Vec<String>,
    pub restart: bool,
    pub reinstall: bool,
    pub allow_ubuntu: bool,
    pub allow_unsigned: bool,
    pub offline: bool,
    pub dry: bool,
    pub yes: bool,
    pub purge: bool,
    pub keep: bool,
    pub remove_dependents: bool,
}

pub fn parse(args: &[String]) -> Result<Options> {
    let action = args.first().map(String::as_str).unwrap_or("");
    ensure(
        matches!(action, "install" | "uninstall" | "verify"),
        "usage: tessera-bundle install|uninstall|verify [options]",
    )?;
    let mut options = Options {
        action: action.into(),
        profile: "none".into(),
        card: "default".into(),
        no_dsp: false,
        admins: Vec::new(),
        restart: false,
        reinstall: false,
        allow_ubuntu: false,
        allow_unsigned: false,
        offline: false,
        dry: false,
        yes: false,
        purge: false,
        keep: false,
        remove_dependents: false,
    };
    let mut index = 1;
    while index < args.len() {
        let arg = args[index].as_str();
        match (action, arg) {
            ("install", "--profile" | "--admin-user" | "--audio-card") => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| Error(format!("missing value for {arg}")))?;
                match arg {
                    "--profile" => options.profile = value.clone(),
                    "--audio-card" => options.card = value.clone(),
                    _ => options.admins.push(value.clone()),
                }
            }
            ("install", "--no-dsp") => options.no_dsp = true,
            ("install", "--restart") => options.restart = true,
            ("install", "--reinstall") => options.reinstall = true,
            ("install", "--allow-ubuntu-24.04") => options.allow_ubuntu = true,
            ("install", "--allow-unsigned") => options.allow_unsigned = true,
            ("install", "--offline") => options.offline = true,
            ("install" | "uninstall", "--dry-run") => options.dry = true,
            ("install" | "uninstall", "--yes") => options.yes = true,
            ("uninstall", "--purge") => options.purge = true,
            ("uninstall", "--keep-users") => options.keep = true,
            ("uninstall", "--remove-dependents") => options.remove_dependents = true,
            _ => return Err(Error(format!("unknown option {arg}"))),
        }
        index += 1;
    }
    ensure(
        matches!(options.profile.as_str(), "none" | "speaker" | "ndimon"),
        "invalid profile",
    )?;
    ensure(
        !options.card.is_empty()
            && options
                .card
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_:.,=+-".contains(&c)),
        "invalid audio card",
    )?;
    ensure(
        options.admins.iter().all(|user| account(user)),
        "invalid admin user",
    )?;
    Ok(options)
}

pub(crate) fn account(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|c| c.is_ascii_lowercase() || *c == b'_')
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_-".contains(&c))
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub root: PathBuf,
    pub bundle: PathBuf,
    pub os_release: PathBuf,
    pub uid: u32,
}

impl Paths {
    pub fn environment() -> Result<Self> {
        let root = std::env::var_os("BUNDLE_ROOT")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| "/".into());
        ensure(root.is_absolute(), "BUNDLE_ROOT must be absolute")?;
        Ok(Self {
            root,
            bundle: std::env::current_exe()?
                .parent()
                .ok_or_else(|| Error("missing executable directory".into()))?
                .to_owned(),
            os_release: std::env::var_os("BUNDLE_OS_RELEASE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| "/etc/os-release".into()),
            uid: nix::unistd::geteuid().as_raw(),
        })
    }

    pub fn at(&self, absolute: &str) -> PathBuf {
        self.root.join(absolute.trim_start_matches('/'))
    }
}

#[derive(Debug, Default)]
pub struct Reply {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub trait Runner {
    fn command(&mut self, program: &str, args: &[String]) -> Result<Reply>;
    fn confirm(&mut self, prompt: &str) -> Result<bool>;
}

#[derive(Debug)]
pub struct Native;

impl Runner for Native {
    fn command(&mut self, program: &str, args: &[String]) -> Result<Reply> {
        let output = Command::new(program)
            .args(args)
            .env("DEBIAN_FRONTEND", "noninteractive")
            .stdin(Stdio::null())
            .output()?;
        Ok(Reply {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn confirm(&mut self, prompt: &str) -> Result<bool> {
        ensure(
            std::io::stdin().is_terminal(),
            "use --yes without a terminal",
        )?;
        print!("{prompt} [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        Ok(matches!(answer.trim(), "y" | "Y"))
    }
}

pub(crate) fn command(runner: &mut impl Runner, program: &str, args: &[&str]) -> Result<Reply> {
    runner.command(
        program,
        &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
    )
}

pub(crate) fn checked(runner: &mut impl Runner, program: &str, args: &[&str]) -> Result<String> {
    let reply = command(runner, program, args)?;
    if reply.success && !reply.stderr.is_empty() {
        eprint!("{}", reply.stderr);
    }
    ensure(
        reply.success,
        &format!("{program} failed: {}", visible(reply.stderr.trim())),
    )?;
    Ok(reply.stdout.trim_end().into())
}

/// Equivalent to cat -v for terminal control bytes used in status and residue.
pub fn visible(text: &str) -> String {
    let mut escaped = String::new();
    for c in text.chars() {
        match c {
            '\n' | '\t' => escaped.push(c),
            '\u{7f}' => escaped.push_str("^?"),
            c if c.is_control() && (c as u32) < 32 => {
                escaped.push('^');
                escaped.push(char::from(c as u8 + 64));
            }
            c if c.is_control() => escaped.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => escaped.push(c),
        }
    }
    escaped
}

impl From<release::Error> for Error {
    fn from(error: release::Error) -> Self {
        Self(error.to_string())
    }
}

/// The refusal of a release that `keys` do not vouch for. The typed code comes
/// first, as everywhere else; then why, where the trusted keys are read from
/// and that `--allow-unsigned` is for development builds only.
fn authentication_error(error: release::Error, keys: &[String]) -> Error {
    if error != release::Error::UntrustedKey {
        return error.into();
    }
    let why = if keys.is_empty() {
        "no trusted release key is installed"
    } else {
        "the release is not signed by any trusted release key"
    };
    Error(format!(
        "{}: {why}. Trusted keys are read from {} (one minisign public key per line; none is built in). --allow-unsigned installs without authentication and is for development builds only.",
        release::Error::UntrustedKey.code(),
        release::OVERRIDE_FILE
    ))
}

pub fn run(options: &Options, paths: &Paths, runner: &mut impl Runner) -> Result<()> {
    let keys =
        if options.action == "verify" || (options.action == "install" && !options.allow_unsigned) {
            release::load_keys(&paths.at(release::OVERRIDE_FILE))?
        } else {
            Vec::new()
        };
    run_with_keys(options, paths, runner, &keys)
}

/// Embedders supply public keys from their own trusted provisioning boundary.
/// The command-line entry point always loads compiled/root-owned trust above.
pub fn run_with_keys(
    options: &Options,
    paths: &Paths,
    runner: &mut impl Runner,
    keys: &[String],
) -> Result<()> {
    if matches!(options.action.as_str(), "install" | "verify") {
        let signed = if options.action == "install" && options.allow_unsigned {
            eprintln!(
                "WARNING: --allow-unsigned is for development only; release authentication bypassed"
            );
            let sums = release::read_small(&paths.bundle, "SHA256SUMS", release::MAX_SUMS)?;
            release::Checksums::parse(&sums)?.verify_files(&paths.bundle)?;
            None
        } else {
            Some(
                manifest::verify_signature(&paths.bundle, keys, None)
                    .map_err(|error| authentication_error(error, keys))?,
            )
        };
        let malformed = |error: Error| Error::from(release::Error::Malformed { reason: error.0 });
        let manifest = manifest::load(&paths.bundle, runner).map_err(malformed)?;
        if signed
            .as_ref()
            .is_some_and(|signed| signed.version != manifest.version)
        {
            return Err(release::Error::WrongVersion.into());
        }
        manifest::verify(&paths.bundle, &manifest, runner).map_err(malformed)?;
        if options.action == "verify" {
            println!("signature: verified (tessera {})", manifest.version);
            println!("verify: complete");
            return Ok(());
        }
    }
    lifecycle::run(options, paths, runner)
}

pub(crate) fn no_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure(
            !metadata.file_type().is_symlink(),
            "record directory or file is a symlink",
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
