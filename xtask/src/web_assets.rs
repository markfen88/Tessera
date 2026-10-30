//! Compile the optional browser enhancement and check its committed output.

use crate::{Failure, Result, run};
use std::fs;
use std::path::Path;
use std::process::Command;

pub(crate) fn build(root: &Path, check: bool) -> Result<()> {
    let web = root.join("crates/tessera-web");
    let asset = web.join("assets/generated/live.js");
    let before = if check { Some(fs::read(&asset)?) } else { None };
    run(Command::new("npm").current_dir(&web).args([
        "ci",
        "--ignore-scripts",
        "--no-audit",
        "--no-fund",
    ]))?;
    // The browser tests are development material: published source releases
    // carry neither them nor their build configuration.
    let scripts: &[&str] = if cfg!(feature = "internal") {
        &["build", "build-tests", "test"]
    } else {
        &["build"]
    };
    for &script in scripts {
        run(Command::new("npm").current_dir(&web).args(["run", script]))?;
    }
    if let Some(before) = before {
        if before != fs::read(&asset)? {
            return Err(Failure::new(
                "browser asset is stale: run cargo xtask web-assets and include assets/generated/live.js",
            ));
        }
    }
    Ok(())
}
