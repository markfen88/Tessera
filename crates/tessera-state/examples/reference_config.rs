//! Print the reference configuration generated from the schema.
//!
//! This is how `dist/etc/tessera/config.toml` (installed as
//! `/etc/tessera/config.toml` and as
//! `/usr/share/doc/tessera/examples/config.toml`) is produced, and what
//! `packaging/build-deb.sh` and the `dist` test check it against. It is a
//! development tool, not a product binary. `tesserad --print-default-config`
//! prints the same text from the installed daemon; this example is the way to get it from the workspace without
//! building the daemon. To regenerate the shipped file, from the workspace
//! root:
//!
//! ```sh
//! cargo run -q -p tessera-state --example reference_config > dist/etc/tessera/config.toml
//! ```
//!
//! With `--tables` it prints the Markdown tables that sit between the
//! `schema:begin` and `schema:end` markers of `docs/configuration.md`.

use std::io::Write as _;

fn main() -> std::io::Result<()> {
    let text = match std::env::args().nth(1).as_deref() {
        None => tessera_state::reference_config(),
        Some("--tables") => tessera_state::schema::markdown_tables(),
        Some(other) => {
            eprintln!("usage: reference_config [--tables]   (unknown argument {other:?})");
            std::process::exit(2);
        }
    };
    std::io::stdout().write_all(text.as_bytes())
}
