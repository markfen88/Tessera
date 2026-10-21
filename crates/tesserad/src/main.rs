//! `tesserad`: the tessera audio-over-IP daemon.

use clap::Parser;

fn main() -> std::process::ExitCode {
    let opts = tesserad::Options::parse();
    std::process::ExitCode::from(tesserad::run(&opts))
}
