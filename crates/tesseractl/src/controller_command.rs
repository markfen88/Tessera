//! Adapter for the existing compatibility runtime's two host commands.
//! Installed as `sudo` only in that service's private PATH. It never elevates
//! privileges: it submits a typed controller flow replacement through the
//! service's existing admin-group membership, without writing configuration.
//!
//! Every complete report, including startup-empty, goes to the daemon's
//! distinct guarded report transition. The adapter never claims ownership,
//! reads a marker or falls back to the intentional whole-set setter.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use tessera_control_types::{FlowSpec, Reply, RequestBody};
use tesseractl::client::{Client, DEFAULT_MAX_RESPONSE_BYTES};

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

fn save_flows(socket: &Path, input: &str) -> Result<(), Box<dyn std::error::Error>> {
    submit_flows(socket, tessera_state::flows::parse_flow_specs(input)?)
}

fn submit_flows(path: &Path, flows: Vec<FlowSpec>) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = Client::connect(path, Duration::from_secs(10), DEFAULT_MAX_RESPONSE_BYTES)?;
    let reply = client.request(RequestBody::RuntimeReport { flows })?;
    match reply {
        Reply::Done => Ok(()),
        other => Err(format!("unexpected reply to runtime report: {other:?}").into()),
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    match action(&std::env::args().skip(1).collect::<Vec<_>>())? {
        Action::SaveFlows => {
            let mut input = String::new();
            std::io::stdin()
                .take(1024 * 1024 + 1)
                .read_to_string(&mut input)?;
            if input.len() > 1024 * 1024 {
                return Err("complete runtime report exceeds 1 MiB".into());
            }
            let socket = std::env::var_os("TESSERA_SOCKET")
                .map(PathBuf::from)
                .unwrap_or_else(|| tesseractl::cli::DEFAULT_SOCKET.into());
            save_flows(&socket, &input)?;
            Ok(ExitCode::SUCCESS)
        }
        Action::Reload => {
            eprintln!(
                "tessera-controller: deprecated reload action ignored; controller flows are applied by the state command"
            );
            Ok(ExitCode::SUCCESS)
        }
        Action::LocalControl(args) => {
            let status = Command::new("/usr/bin/tesseractl")
                .args(confirmed(args))
                .status()?;
            Ok(ExitCode::from(
                u8::try_from(status.code().unwrap_or(1)).unwrap_or(1),
            ))
        }
    }
}

/// `args` for `tesseractl`, with `--yes` in front: the runtime has no terminal to ask on, and its
/// operator already chose the change. Left alone when they carry `--yes`/`-y` (a repeated flag is an
/// error) or `--dry-run` (which conflicts with `--yes` and changes nothing).
fn confirmed(args: Vec<String>) -> Vec<String> {
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "--yes" | "-y" | "--dry-run"))
    {
        return args;
    }
    std::iter::once("--yes".to_owned()).chain(args).collect()
}

fn main() -> ExitCode {
    if let Some(code) = tesseractl::runtime_build_probe("report-adapter") {
        return code;
    }
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
    fn local_control_is_confirmed_unless_that_would_be_an_error() {
        assert_eq!(
            confirmed(args(&["set-rate", "96000"])),
            args(&["--yes", "set-rate", "96000"])
        );
        assert_eq!(confirmed(args(&["status"])), args(&["--yes", "status"]));
        // Already confirmed: not repeated (clap rejects a repeated flag).
        assert_eq!(
            confirmed(args(&["--yes", "set-rate", "48000"])),
            args(&["--yes", "set-rate", "48000"])
        );
        assert_eq!(
            confirmed(args(&["-y", "set-rate", "48000"])),
            args(&["-y", "set-rate", "48000"])
        );
        // A dry run conflicts with --yes and changes nothing.
        let dry = args(&["set-channels", "2", "2", "--dry-run"]);
        assert_eq!(confirmed(dry.clone()), dry);
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
    fn flow_conversion_is_typed_and_preserves_wire_channel_order_and_defaults() {
        let flows = tessera_state::flows::parse_flow_specs(
            "[[flow]]\nid='receiver'\ndirection='rx'\ndevice='main'\npeer='192.0.2.10'\nport=5004\nchannels=[2,1]\nrx_keepalive_payload=1337\n"
        ).unwrap();
        assert_eq!(flows.len(), 1);
        assert_eq!(
            flows[0].device,
            tessera_control_types::DeviceKey("main".into())
        );
        assert_eq!(flows[0].direction, tessera_control_types::Direction::Rx);
        assert_eq!(flows[0].channels, vec![2, 1]);
        assert_eq!(flows[0].peer.as_deref(), Some("192.0.2.10"));
        assert_eq!(flows[0].format, "reference");
        assert_eq!(flows[0].dscp, 46);
        assert_eq!(flows[0].rx_keepalive_payload.as_deref(), Some("1337"));
    }

    #[test]
    fn scope_and_unknown_flow_keys_are_refused_and_empty_sets_clear() {
        for bad in [
            "[daemon]\nstate_dir='/tmp'",
            "flow=1",
            "flow=[1]",
            "[flow]\nid='x'",
            "broken[",
            "[[flow]]\nid='x'\ndirection='rx'\ndevice='main'\nport=5004\nunknown=true",
        ] {
            assert!(
                tessera_state::flows::parse_flow_specs(bad).is_err(),
                "{bad}"
            );
        }
        for clear in ["", "# cleared\n", "flow=[]"] {
            assert!(
                tessera_state::flows::parse_flow_specs(clear)
                    .unwrap()
                    .is_empty()
            );
        }
    }

    /// A directory of its own for a test's socket and run marker.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "tessera-flow-adapter-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }

        fn socket(&self) -> PathBuf {
            self.0.join("control.sock")
        }

        fn daemon(&self, refuse: bool) -> std::thread::JoinHandle<Vec<FlowSpec>> {
            use std::io::Write;
            use std::os::unix::net::UnixListener;
            use tessera_control_types::{Request, Response, WIRE_VERSION};
            let _ = std::fs::remove_file(self.socket());
            let listener = UnixListener::bind(self.socket()).unwrap();
            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut header = [0; 4];
                stream.read_exact(&mut header).unwrap();
                let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
                stream.read_exact(&mut bytes).unwrap();
                let request: Request = serde_json::from_slice(&bytes).unwrap();
                let RequestBody::RuntimeReport { flows } = request.body else {
                    panic!("not a distinct complete runtime report");
                };
                if !refuse {
                    let response = Response {
                        v: WIRE_VERSION,
                        id: request.id,
                        result: Ok(Reply::Done),
                    };
                    let bytes = serde_json::to_vec(&response).unwrap();
                    stream
                        .write_all(&(bytes.len() as u32).to_be_bytes())
                        .unwrap();
                    stream.write_all(&bytes).unwrap();
                }
                flows
            })
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const ONE_FLOW: &str =
        "[[flow]]\nid='receiver'\ndirection='rx'\ndevice='main'\npeer='192.0.2.10'\nport=5004\n";

    #[test]
    fn every_complete_report_including_startup_empty_uses_the_guarded_request() {
        let dir = Scratch::new("complete");
        for input in [ONE_FLOW, "", "flow=[]", ONE_FLOW, "# empty\n"] {
            let server = dir.daemon(false);
            save_flows(&dir.socket(), input).unwrap();
            let flows = server.join().unwrap();
            assert_eq!(flows.len(), usize::from(input == ONE_FLOW));
            // No configuration or ownership marker is written by the adapter.
            assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
        }
    }

    #[test]
    fn refused_report_has_no_fallback_or_marker_and_invalid_input_is_local() {
        let dir = Scratch::new("refused");
        let server = dir.daemon(true);
        assert!(save_flows(&dir.socket(), ONE_FLOW).is_err());
        assert_eq!(server.join().unwrap().len(), 1);
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
        assert!(save_flows(&dir.socket(), "[daemon]\nstate_dir='/tmp'").is_err());
    }
}
