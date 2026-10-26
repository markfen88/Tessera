//! Adapter for the existing compatibility runtime's two host commands.
//! Installed as `sudo` only in that service's private PATH. It never elevates
//! privileges: it submits a typed controller flow replacement through the
//! service's existing admin-group membership, without writing configuration.
//!
//! Flow ownership. The runtime reports its flow set with every change it makes,
//! and at start, when it knows none, it reports an empty set. That report is no
//! instruction to remove anything: the daemon may hold flows the operator
//! stored (`tesseractl flows set`, `set_protocol_flows`) that must survive the
//! daemon's and the runtime's restart. The controller owns the flows it has
//! submitted in its current run. A report is applied when it names flows, or
//! when the controller has already applied one in this run (an empty report
//! then removes what it set); otherwise it is answered with success and
//! ignored. A run is one systemd invocation of the service (`INVOCATION_ID`).
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use tessera_control_types::{Command as StateCommand, FlowSpec, Reply, RequestBody};
use tesseractl::client::{Client, DEFAULT_MAX_RESPONSE_BYTES};

const FLOW_FILE: &str = "/etc/tessera/config.d/90-probe-flows.toml";
/// The service's private writable directory (`ReadWritePaths` of the unit).
const STATE_DIR: &str = "/var/lib/tessera-controller";
/// In [`STATE_DIR`]: the run that last had a flow report applied.
const RUN_FILE: &str = "flows-run";

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

/// What became of a flow report.
#[derive(Debug, PartialEq, Eq)]
enum Report {
    /// The daemon's controller flow set was replaced.
    Applied,
    /// An empty report from a run that has applied none: the stored set stays.
    Kept,
}

/// Does a report replace the stored set? One that names flows does; an empty
/// one only from a run that has applied a report before (it removes its own
/// flows). `last_run` is the run recorded by the last applied report.
fn applies(empty: bool, last_run: Option<&str>, run: &str) -> bool {
    !empty || last_run == Some(run)
}

/// Apply the runtime's flow report in `input` under the ownership rule in the
/// module documentation, remembering in `dir` the run that applied one.
fn save_flows(
    socket: &Path,
    dir: &Path,
    run: &str,
    input: &str,
) -> Result<Report, Box<dyn std::error::Error>> {
    let flows = tessera_state::flows::parse_flow_specs(input)?;
    let last_run = std::fs::read_to_string(dir.join(RUN_FILE)).ok();
    if !applies(flows.is_empty(), last_run.as_deref(), run) {
        return Ok(Report::Kept);
    }
    submit_flows(socket, flows)?;
    // Only an applied report makes the run the owner; a failed write of the
    // marker costs the next empty report an unneeded refusal, nothing more.
    if let Err(error) = write_run(dir, run) {
        eprintln!("tessera-controller: cannot record the flow report: {error}");
    }
    Ok(Report::Applied)
}

/// Replace `RUN_FILE` atomically: a private temporary file, then a rename.
fn write_run(dir: &Path, run: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let temporary = dir.join(format!(".{RUN_FILE}.{}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(run.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, dir.join(RUN_FILE))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn submit_flows(path: &Path, flows: Vec<FlowSpec>) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = Client::connect(path, Duration::from_secs(10), DEFAULT_MAX_RESPONSE_BYTES)?;
    // The historical host command contains no controller address. Name the
    // forwarding runtime honestly; a media peer is not the controller peer.
    let reply = client.request(RequestBody::ProtocolCommand {
        peer: "controller-compatibility".into(),
        command: StateCommand::SetProtocolFlows { flows },
    })?;
    match reply {
        Reply::Done => Ok(()),
        other => Err(format!("unexpected reply to controller flow replacement: {other:?}").into()),
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    match action(&std::env::args().skip(1).collect::<Vec<_>>())? {
        Action::SaveFlows => {
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input)?;
            let socket = std::env::var_os("TESSERA_SOCKET")
                .map(PathBuf::from)
                .unwrap_or_else(|| tesseractl::cli::DEFAULT_SOCKET.into());
            let dir = std::env::var_os("TESSERA_CONTROLLER_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| STATE_DIR.into());
            let run = std::env::var("INVOCATION_ID").unwrap_or_default();
            if save_flows(&socket, &dir, &run, &input)? == Report::Kept {
                eprintln!(
                    "tessera-controller: no flows of its own to report yet; the stored flow set is kept"
                );
            }
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

        fn run_marker(&self) -> Option<String> {
            std::fs::read_to_string(self.0.join(RUN_FILE)).ok()
        }

        /// A daemon that answers one request: `Done`, or (`refuse`) a closed
        /// connection. It returns the flows the request carried.
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
                let RequestBody::ProtocolCommand {
                    peer,
                    command: StateCommand::SetProtocolFlows { flows },
                } = request.body
                else {
                    panic!("not a protocol flow replacement");
                };
                assert_eq!(peer, "controller-compatibility");
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
    fn one_socket_command_replaces_the_set_without_writing_a_flow_file() {
        let dir = Scratch::new("replace");
        let server = dir.daemon(false);
        assert_eq!(
            save_flows(&dir.socket(), &dir.0, "run-1", ONE_FLOW).unwrap(),
            Report::Applied
        );
        let flows = server.join().unwrap();
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].id, "receiver");
        // The socket and the run marker are all that is in the directory.
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 2);
        assert_eq!(dir.run_marker().as_deref(), Some("run-1"));
    }

    #[test]
    fn the_runtimes_empty_report_at_start_keeps_the_stored_flows() {
        // Nothing listens: the daemon is not even asked. The restart of the
        // daemon restarts the runtime, which reports no flows of its own.
        let dir = Scratch::new("start");
        for empty in ["", "# no flows\n", "flow=[]"] {
            assert_eq!(
                save_flows(&dir.socket(), &dir.0, "run-1", empty).unwrap(),
                Report::Kept,
                "{empty:?}"
            );
        }
        assert_eq!(dir.run_marker(), None);
        // A run that follows an earlier run which applied reports starts the same way.
        let server = dir.daemon(false);
        save_flows(&dir.socket(), &dir.0, "run-1", ONE_FLOW).unwrap();
        server.join().unwrap();
        let _ = std::fs::remove_file(dir.socket());
        assert_eq!(
            save_flows(&dir.socket(), &dir.0, "run-2", "flow=[]").unwrap(),
            Report::Kept
        );
        assert_eq!(dir.run_marker().as_deref(), Some("run-1"));
    }

    #[test]
    fn once_it_has_set_flows_the_runtime_may_remove_them() {
        let dir = Scratch::new("remove");
        let server = dir.daemon(false);
        save_flows(&dir.socket(), &dir.0, "run-1", ONE_FLOW).unwrap();
        assert_eq!(server.join().unwrap().len(), 1);
        // A later empty report of the same run clears the daemon's set.
        let server = dir.daemon(false);
        assert_eq!(
            save_flows(&dir.socket(), &dir.0, "run-1", "flow=[]").unwrap(),
            Report::Applied
        );
        assert!(server.join().unwrap().is_empty());
        // Flows named in a new run are applied at once and own that run.
        let server = dir.daemon(false);
        assert_eq!(
            save_flows(&dir.socket(), &dir.0, "run-2", ONE_FLOW).unwrap(),
            Report::Applied
        );
        server.join().unwrap();
        assert_eq!(dir.run_marker().as_deref(), Some("run-2"));
    }

    #[test]
    fn a_report_the_daemon_does_not_apply_does_not_make_the_run_the_owner() {
        let dir = Scratch::new("refused");
        let server = dir.daemon(true);
        assert!(save_flows(&dir.socket(), &dir.0, "run-1", ONE_FLOW).is_err());
        server.join().unwrap();
        assert_eq!(dir.run_marker(), None);
        // So its next empty report is still a start-up report.
        assert_eq!(
            save_flows(&dir.socket(), &dir.0, "run-1", "").unwrap(),
            Report::Kept
        );
        // A report that is not a flow document is refused before anything else.
        assert!(save_flows(&dir.socket(), &dir.0, "run-1", "[daemon]\nstate_dir='/tmp'").is_err());
    }

    #[test]
    fn only_a_run_that_applied_a_report_may_send_an_empty_one() {
        for (empty, last, run, expected) in [
            (false, None, "a", true),
            (false, Some("b"), "a", true),
            (true, None, "a", false),
            (true, Some("b"), "a", false),
            (true, Some("a"), "a", true),
            // Without systemd there is no run id: one unnamed run.
            (true, Some(""), "", true),
            (true, None, "", false),
        ] {
            assert_eq!(
                applies(empty, last, run),
                expected,
                "{empty} {last:?} {run:?}"
            );
        }
    }
}
