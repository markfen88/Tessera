mod build_bundle;
mod build_deb;
mod check_deb;
mod dsp;
mod files;
mod news;
mod no_scripts;
mod release;

use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Output};

type Result<T> = std::result::Result<T, Failure>;

#[derive(Debug)]
struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Task {
    Ci,
    BuildBundle(bool),
    BuildDeb(bool),
    CheckDeb(Vec<PathBuf>),
    Release(release::Action, Vec<String>),
    CheckNoScripts,
    WriteNews {
        directory: PathBuf,
        package: String,
        source: PathBuf,
        version: String,
    },
    Help,
}

fn parse(args: &[String]) -> Result<Task> {
    let Some(name) = args.first() else {
        return Ok(Task::Help);
    };
    if let Some(action) = match name.as_str() {
        "release" => Some(release::Action::Sign),
        "verify-release" => Some(release::Action::Verify),
        "release-keygen" => Some(release::Action::Keygen),
        _ => None,
    } {
        return Ok(Task::Release(action, args[1..].to_vec()));
    }
    if name == "build-deb" {
        return match &args[1..] {
            [] => Ok(Task::BuildDeb(false)),
            [flag] if flag == "--print-version" => Ok(Task::BuildDeb(true)),
            _ => Err(Failure::new("unexpected build arguments")),
        };
    }
    if name == "build-bundle" {
        return match &args[1..] {
            [] => Ok(Task::BuildBundle(false)),
            [flag] if flag == "--allow-ubuntu-24.04" => Ok(Task::BuildBundle(true)),
            _ => Err(Failure::new("unexpected build arguments")),
        };
    }
    if name == "check-deb" {
        if args[1..].iter().any(|arg| arg.starts_with('-')) {
            return Err(Failure::new("check-deb expects package paths"));
        }
        return Ok(Task::CheckDeb(
            args[1..].iter().map(PathBuf::from).collect(),
        ));
    }
    if name == "__write-news" {
        if args.len() != 5 || args[1..].iter().any(|arg| arg.starts_with('-')) {
            return Err(Failure::new(
                "__write-news expects directory, package, source and version",
            ));
        }
        return Ok(Task::WriteNews {
            directory: PathBuf::from(&args[1]),
            package: args[2].clone(),
            source: PathBuf::from(&args[3]),
            version: args[4].clone(),
        });
    }
    let task = match name.as_str() {
        "ci" => Task::Ci,
        "check-no-scripts" => Task::CheckNoScripts,
        "help" | "--help" | "-h" => Task::Help,
        _ => return Err(Failure::new(format!("unknown task: {name}"))),
    };
    if args.len() != 1 {
        return Err(Failure::new(format!("unexpected arguments for {name}")));
    }
    Ok(task)
}

fn status_code(status: ExitStatus) -> u8 {
    if let Some(code) = status.code() {
        return u8::try_from(code).unwrap_or(1);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return u8::try_from(128 + signal).unwrap_or(1);
        }
    }
    1
}

fn run(command: &mut Command) -> Result<()> {
    let status = command.status().map_err(command_error)?;
    if status.success() {
        Ok(())
    } else {
        Err(Failure {
            code: status_code(status),
            message: format!(
                "{} exited with {status}",
                command.get_program().to_string_lossy()
            ),
        })
    }
}

fn output(command: &mut Command) -> Result<Output> {
    let result = command.output().map_err(command_error)?;
    if result.status.success() {
        Ok(result)
    } else {
        use std::io::Write;
        std::io::stderr().write_all(&result.stderr)?;
        Err(Failure {
            code: status_code(result.status),
            message: format!(
                "{} exited with {}",
                command.get_program().to_string_lossy(),
                result.status
            ),
        })
    }
}

fn command_error(error: std::io::Error) -> Failure {
    let code = match error.kind() {
        std::io::ErrorKind::NotFound => 127,
        std::io::ErrorKind::PermissionDenied => 126,
        _ => 1,
    };
    Failure {
        code,
        message: error.to_string(),
    }
}

fn env_or(name: &str, default: &str) -> String {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

fn ci_commands() -> Vec<Vec<String>> {
    let mut commands: Vec<Vec<String>> = [
        vec!["fmt", "--all", "--check"],
        vec![
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
        vec!["test", "--workspace"],
        vec!["deny", "check"],
    ]
    .into_iter()
    .map(|args| args.into_iter().map(str::to_owned).collect())
    .collect();
    for name in [
        "proto",
        "control",
        "clock",
        "media",
        "aes67",
        "control-types",
    ] {
        commands.push(vec![
            "check".into(),
            "--target".into(),
            "thumbv8m.main-none-eabihf".into(),
            "-p".into(),
            format!("tessera-{name}"),
            "--no-default-features".into(),
        ]);
    }
    commands
}

fn execute(task: Task, root: &Path) -> Result<()> {
    match task {
        Task::Ci => {
            no_scripts::check(root)?;
            for args in ci_commands() {
                run(Command::new("cargo").current_dir(root).args(args))?;
            }
            Ok(())
        }
        Task::BuildDeb(print_version) => build_deb::build(root, print_version),
        Task::BuildBundle(ubuntu) => build_bundle::build(root, ubuntu).map(|_| ()),
        Task::CheckDeb(paths) => check_deb::check(root, &paths),
        Task::Release(action, args) => release::execute(action, &args, root),
        Task::CheckNoScripts => no_scripts::check(root),
        Task::WriteNews {
            directory,
            package,
            source,
            version,
        } => news::write_news(&directory, &package, &source, &version),
        Task::Help => {
            println!(
                "usage: cargo xtask <ci|build-deb|build-bundle|check-deb|check-no-scripts|release|verify-release|release-keygen>"
            );
            Ok(())
        }
    }
}

fn task_root(override_root: Option<&str>) -> Result<PathBuf> {
    if let Some(root) = override_root {
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            return Err(Failure::new("TESSERA_XTASK_ROOT must be absolute"));
        }
        return Ok(root);
    }
    Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask is inside the workspace")
        .to_owned())
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let override_root = env::var("TESSERA_XTASK_ROOT").ok();
    let result = task_root(override_root.as_deref())
        .and_then(|root| parse(&args).and_then(|task| execute(task, &root)));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if !error.message.is_empty() {
                eprintln!("xtask: {error}");
            }
            ExitCode::from(error.code)
        }
    }
}
