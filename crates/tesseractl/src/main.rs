//! `tesseractl`: command-line control of the tessera daemon.

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;
use tesseractl::cli::Cli;

fn main() -> ExitCode {
    if let Some(code) = tesseractl::runtime_build_probe("cli") {
        return code;
    }
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        // Prints help/version to stdout (exit 0) or the usage error to stderr
        // (exit 2).
        Err(e) => e.exit(),
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let stderr = std::io::stderr();
    let mut err = stderr.lock();
    let result = tesseractl::run(&cli, &mut out, &mut err);
    let _ = out.flush();
    match result {
        Ok(()) => ExitCode::from(tesseractl::EXIT_OK),
        Err(e) => {
            let msg = tesseractl::describe(&e);
            if !msg.is_empty() {
                if cli.json {
                    let _ = writeln!(err, "{}", serde_json::json!({ "error": msg }));
                } else {
                    let _ = writeln!(err, "tesseractl: {msg}");
                }
            }
            ExitCode::from(tesseractl::exit_code(&e))
        }
    }
}
