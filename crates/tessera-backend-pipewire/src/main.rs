//! `tessera-pw`: the PipeWire client of `tesserad`.

use std::process::ExitCode;

use tessera_backend_pipewire::config::{Cli, Config, USAGE};
use tessera_backend_pipewire::supervisor;

#[cfg(feature = "alloc-check")]
#[global_allocator]
static ALLOC: tessera_backend_pipewire::alloc_check::CountingAlloc =
    tessera_backend_pipewire::alloc_check::CountingAlloc;

fn main() -> ExitCode {
    let cfg = match Config::from_args(std::env::args().skip(1)) {
        Ok(Cli::Run(c)) => *c,
        Ok(Cli::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("tessera-pw: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let level = cfg.log_level.parse().unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(false)
        .init();
    tessera_backend_pipewire::sys::abort_on_panic();
    match supervisor::run(cfg) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::from(1)
        }
    }
}
