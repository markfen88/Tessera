//! Adapter for the existing compatibility runtime's two host commands.
//! Installed as `sudo` only in that service's private PATH. It never elevates
//! privileges: the service can write one managed flow file and request a typed
//! daemon configuration reload through its existing admin-group membership.
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, ExitCode};

const FLOW_FILE: &str = "/etc/tessera/config.d/90-probe-flows.toml";

#[derive(Debug, PartialEq, Eq)]
enum Action {
    SaveFlows,
    Reload,
    LocalControl(Vec<String>),
}

fn action(args: &[String]) -> Result<Action, &'static str> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["-n", "tee", FLOW_FILE] => Ok(Action::SaveFlows),
        ["-n", "systemctl", "reload", "tesserad" | "tesserad.service"] => Ok(Action::Reload),
        ["-n", "tesseractl" | "/usr/bin/tesseractl", rest @ ..] => Ok(Action::LocalControl(
            rest.iter().map(|s| (*s).to_owned()).collect(),
        )),
        _ => Err("unsupported compatibility host command"),
    }
}

fn save_flows(path: &Path, input: &str) -> Result<(), Box<dyn std::error::Error>> {
    let document: toml::Table = toml::from_str(input)?;
    if document.keys().any(|k| k != "flow")
        || document
            .get("flow")
            .is_some_and(|f| f.as_array().is_none_or(|a| a.iter().any(|f| !f.is_table())))
    {
        return Err("compatibility runtime may save only [[flow]] entries".into());
    }
    // The parent directory is root-owned; the service only owns this existing
    // file, so it cannot replace the directory entry or create arbitrary files.
    if !std::fs::symlink_metadata(path)?.file_type().is_file() {
        return Err("managed flow path is not a regular file".into());
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(input.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    match action(&std::env::args().skip(1).collect::<Vec<_>>())? {
        Action::SaveFlows => {
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input)?;
            save_flows(Path::new(FLOW_FILE), &input)?;
            Ok(ExitCode::SUCCESS)
        }
        Action::Reload => {
            let status = Command::new("/usr/bin/tesseractl")
                .args(["config", "reload"])
                .status()?;
            Ok(ExitCode::from(
                u8::try_from(status.code().unwrap_or(1)).unwrap_or(1),
            ))
        }
        Action::LocalControl(args) => {
            let status = Command::new("/usr/bin/tesseractl").args(args).status()?;
            Ok(ExitCode::from(
                u8::try_from(status.code().unwrap_or(1)).unwrap_or(1),
            ))
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("tessera-controller: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).into()).collect()
    }
    #[test]
    fn host_commands_are_bounded_and_local_control_never_elevates() {
        assert_eq!(
            action(&args(&["-n", "tee", FLOW_FILE])),
            Ok(Action::SaveFlows)
        );
        assert_eq!(
            action(&args(&["-n", "systemctl", "reload", "tesserad"])),
            Ok(Action::Reload)
        );
        assert_eq!(
            action(&args(&["-n", "tesseractl", "status"])),
            Ok(Action::LocalControl(vec!["status".into()]))
        );
        for bad in [
            vec!["-n", "tee", "/etc/passwd"],
            vec!["-n", "systemctl", "restart", "tesserad"],
            vec!["-n", "systemctl", "reload", "ssh"],
            vec!["sh", "-c", "true"],
        ] {
            assert!(action(&args(&bad)).is_err());
        }
    }
    #[test]
    fn flow_only_updates_preserve_the_existing_file_on_invalid_input() {
        let dir = std::env::temp_dir().join(format!("tessera-flow-adapter-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("flows.toml");
        std::fs::write(&path, "# original\n").unwrap();
        for bad in [
            "[daemon]\nstate_dir='/tmp'",
            "flow=1",
            "flow=[1]",
            "broken[",
        ] {
            assert!(save_flows(&path, bad).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "# original\n");
        }
        let good = "[[flow]]\nid='receiver'\ndirection='rx'\n";
        save_flows(&path, good).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), good);
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(dir.join("missing"), &path).unwrap();
        assert!(save_flows(&path, good).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
