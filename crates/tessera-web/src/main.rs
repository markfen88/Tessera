//! `tessera-web`: serve the portal, or set its password.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tessera_web::appliance::Appliance;
use tessera_web::appliance::services::{ProcessRunner, polkit_rules, run_unitfile};
use tessera_web::auth::{MIN_PASSWORD_LEN, PasswordFile, hash_password};
use tessera_web::backend::SocketBackend;
use tessera_web::config::{DEFAULT_PATH, WebConfig};
use tessera_web::{AppState, router};

/// The web portal of a tessera device.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    /// Configuration file.
    #[arg(long, default_value = DEFAULT_PATH)]
    config: PathBuf,
    /// What to do (default: serve).
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Serve the portal (the default).
    Serve,
    /// Set the operator password (read from standard input, never from the
    /// command line). Run as root.
    SetPassword,
    /// Check the configuration file and exit.
    Check,
    /// Appliance mode helpers.
    Appliance {
        #[command(subcommand)]
        cmd: ApplianceCmd,
    },
}

#[derive(Debug, Subcommand)]
enum ApplianceCmd {
    /// Print the polkit rules file for the configured `[[appliance.service]]`
    /// units (the output says where to install it).
    Polkit,
    /// Enable or disable one allowlisted service at boot. Run as root by
    /// `tessera-web-unitfile@<instance>.service` (started by the portal), never
    /// by hand: `<instance>` is `enable-<service>` or `disable-<service>`.
    Unitfile {
        /// The instance of the helper unit.
        instance: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .without_time()
        .init();
    let cfg = match WebConfig::load(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tessera-web: {e}");
            return ExitCode::from(2);
        }
    };
    match cli.cmd.unwrap_or(Cmd::Serve) {
        Cmd::Serve => serve(cfg),
        Cmd::SetPassword => set_password(&cfg),
        Cmd::Check => {
            println!(
                "listen {}; control socket {}; password file {}; appliance mode {}",
                cfg.listen,
                cfg.control_socket.display(),
                cfg.password_file.display(),
                if cfg.appliance.enabled {
                    format!("on ({} service entries)", cfg.appliance.services.len())
                } else {
                    "off".to_owned()
                }
            );
            ExitCode::SUCCESS
        }
        Cmd::Appliance {
            cmd: ApplianceCmd::Polkit,
        } => {
            print!("{}", polkit_rules(&cfg.appliance));
            ExitCode::SUCCESS
        }
        Cmd::Appliance {
            cmd: ApplianceCmd::Unitfile { instance },
        } => unitfile(&cfg, &instance),
    }
}

/// The root helper: re-checks the allowlist from the configuration file it
/// was given, then runs one `systemctl` and exits with its code.
fn unitfile(cfg: &WebConfig, instance: &str) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tessera-web: cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run_unitfile(&ProcessRunner, &cfg.appliance, instance)) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            eprintln!("tessera-web appliance unitfile: refused: {e}");
            ExitCode::FAILURE
        }
    }
}

fn serve(cfg: WebConfig) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tessera-web: cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async move {
        let backend = Arc::new(SocketBackend::new(cfg.control_socket.clone(), cfg.daemon_timeout));
        let mut state = AppState::new(
            backend.clone(),
            PasswordFile(cfg.password_file.clone()),
            cfg.session_idle,
            cfg.cookie_secure,
        );
        if cfg.appliance.enabled {
            tracing::info!(
                services = cfg.appliance.services.len(),
                "appliance mode: the Speaker page is the landing page"
            );
            state = state.with_appliance(Appliance::new(
                cfg.appliance.clone(),
                Arc::new(ProcessRunner),
            ));
        }
        let state = Arc::new(state);
        state.hub.follow(backend, Duration::from_secs(2));
        let listener = match tokio::net::TcpListener::bind(cfg.listen).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("tessera-web: cannot listen on {}: {e}", cfg.listen);
                return ExitCode::FAILURE;
            }
        };
        if !cfg.listen.ip().is_loopback() {
            tracing::warn!(
                "listening on {} (plain HTTP): put a TLS reverse proxy in front on a shared network",
                cfg.listen
            );
        }
        tracing::info!("tessera-web on http://{}", cfg.listen);
        let app = router(state).into_make_service_with_connect_info::<tessera_web::network_profiles::Connection>();
        let r = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown())
            .await;
        match r {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("tessera-web: {e}");
                ExitCode::FAILURE
            }
        }
    })
}

async fn shutdown() {
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = term => {}
    }
}

/// Read one line from standard input without echo when it is a terminal.
fn read_secret(prompt: &str) -> std::io::Result<String> {
    let stdin = std::io::stdin();
    let tty = stdin.is_terminal();
    let mut saved = None;
    if tty {
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
        if let Ok(t) = nix::sys::termios::tcgetattr(&stdin) {
            let mut quiet = t.clone();
            quiet
                .local_flags
                .remove(nix::sys::termios::LocalFlags::ECHO);
            if nix::sys::termios::tcsetattr(&stdin, nix::sys::termios::SetArg::TCSANOW, &quiet)
                .is_ok()
            {
                saved = Some(t);
            }
        }
    }
    let mut line = String::new();
    let r = stdin.lock().read_line(&mut line);
    if let Some(t) = saved {
        let _ = nix::sys::termios::tcsetattr(&stdin, nix::sys::termios::SetArg::TCSANOW, &t);
        eprintln!();
    }
    r?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

fn set_password(cfg: &WebConfig) -> ExitCode {
    let first = match read_secret("New portal password: ") {
        Ok(p) => p,
        Err(e) => {
            eprintln!("tessera-web: {e}");
            return ExitCode::FAILURE;
        }
    };
    if first.chars().count() < MIN_PASSWORD_LEN {
        eprintln!("tessera-web: the password needs at least {MIN_PASSWORD_LEN} characters");
        return ExitCode::from(2);
    }
    if std::io::stdin().is_terminal() {
        match read_secret("Again: ") {
            Ok(again) if again == first => {}
            Ok(_) => {
                eprintln!("tessera-web: the passwords differ");
                return ExitCode::from(2);
            }
            Err(e) => {
                eprintln!("tessera-web: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    let hash = match hash_password(&first) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("tessera-web: {e}");
            return ExitCode::FAILURE;
        }
    };
    match write_password_file(&cfg.password_file, &hash) {
        Ok(()) => {
            println!("password set in {}", cfg.password_file.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("tessera-web: {}: {e}", cfg.password_file.display());
            ExitCode::FAILURE
        }
    }
}

/// Write atomically, mode 0640, group `tessera-web` when it exists (so the
/// service can read it and nobody else can).
fn write_password_file(path: &std::path::Path, hash: &str) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("web-password")
    ));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o640)
            .open(&tmp)?;
        f.write_all(hash.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o640))?;
    if let Ok(Some(g)) = nix::unistd::Group::from_name("tessera-web") {
        if let Err(e) = nix::unistd::chown(&tmp, None, Some(g.gid)) {
            eprintln!("tessera-web: warning: cannot give the file to group tessera-web: {e}");
        }
    }
    std::fs::rename(&tmp, path)?;
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}
