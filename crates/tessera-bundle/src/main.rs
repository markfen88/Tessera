use std::process::ExitCode;

fn main() -> ExitCode {
    nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o022));
    let args: Vec<_> = std::env::args().skip(1).collect();
    let action = args.first().map(String::as_str).unwrap_or("bundle");
    let result = tessera_bundle::parse(&args)
        .map_err(|error| tessera_bundle::Error(format!("{action}: parse: {error}")))
        .and_then(|options| {
            tessera_bundle::Paths::environment().and_then(|paths| {
                tessera_bundle::run(&options, &paths, &mut tessera_bundle::Native)
            })
        });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
