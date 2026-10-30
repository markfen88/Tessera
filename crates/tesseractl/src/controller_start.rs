//! Launch the existing Controller runtime with the daemon's current identity
//! and wired interface. Protocol handling stays in the compatibility runtime.
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;
use tessera_control_types::{Query, Reply, RequestBody};
use tesseractl::client::{Client, DEFAULT_MAX_RESPONSE_BYTES};

#[derive(Debug)]
struct DaemonContext {
    status: serde_json::Value,
    state_path: PathBuf,
}

/// Read through the existing daemon API in THIS launcher process. Future
/// pre-exec association must not register a short-lived CLI child instead.
/// These queries themselves supply no launch or holder proof.
/// They are separate observations, not an atomic status/configuration revision;
/// any eventual registration must revalidate current authority context.
fn daemon_context(socket: &Path) -> Result<DaemonContext, Box<dyn std::error::Error>> {
    let mut client = Client::connect(socket, Duration::from_secs(10), DEFAULT_MAX_RESPONSE_BYTES)?;
    let status = serde_json::to_value(client.status()?)?;
    let Reply::EffectiveConfig(text) =
        client.request(RequestBody::Query(Query::EffectiveConfig))?
    else {
        return Err("expected effective daemon configuration".into());
    };
    let config: toml::Value = toml::from_str(&text)?;
    let state_dir = config
        .get("daemon")
        .and_then(|d| d.get("state_dir"))
        .and_then(toml::Value::as_str)
        .ok_or("missing daemon state directory")?;
    Ok(DaemonContext {
        status,
        state_path: Path::new(state_dir).join("state.toml"),
    })
    // Client is dropped here. No query socket is inherited into the runtime
    // or held idle while waiting for the daemon's first identity flush.
}

#[derive(Debug)]
struct UnsupportedDevice(&'static str);

impl std::fmt::Display for UnsupportedDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for UnsupportedDevice {}

#[derive(Debug)]
struct PendingIdentity;

impl std::fmt::Display for PendingIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("waiting for the daemon to persist its device identity")
    }
}

impl std::error::Error for PendingIdentity {}

fn failure_exit(error: &(dyn std::error::Error + 'static)) -> ExitCode {
    if error.is::<UnsupportedDevice>() {
        ExitCode::from(78)
    } else {
        ExitCode::FAILURE
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let key = std::env::args().nth(1).unwrap_or_else(|| "main".into());
    let socket = std::env::var_os("TESSERA_SOCKET").map_or_else(
        || PathBuf::from(tesseractl::cli::DEFAULT_SOCKET),
        PathBuf::from,
    );
    let DaemonContext { status, state_path } = daemon_context(&socket)?;
    let mut announced = false;
    let args = wait_for_identity(&status, &state_path, &key, || {
        if !announced {
            eprintln!("tessera-controller: {PendingIdentity}");
            announced = true;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    })?;
    let preset = std::fs::read_to_string("/usr/lib/tessera/controller.args")?;
    register_runtime(
        &socket,
        &status,
        args.get(3).ok_or("missing device identity")?,
    )?;
    let error = Command::new("/usr/lib/tessera/cmc-probe")
        .args(args)
        .args(
            preset
                .lines()
                .filter(|line| !line.is_empty() && !line.starts_with('#')),
        )
        .exec();
    Err(error.into())
}

fn register_runtime(
    socket: &Path,
    status: &serde_json::Value,
    wire_device_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let status: tessera_control_types::Status = serde_json::from_value(status.clone())?;
    let device = status.devices.into_iter().next().ok_or("no first device")?;
    let network = status.network.ok_or("no network observation")?;
    let mut client = Client::connect(socket, Duration::from_secs(10), DEFAULT_MAX_RESPONSE_BYTES)?;
    match client.request(RequestBody::RuntimeStart {
        device,
        network,
        wire_device_id: wire_device_id.into(),
    })? {
        Reply::Done => Ok(()),
        _ => Err("runtime registration was not acknowledged".into()),
    }
    // No connected registration descriptor crosses exec or reaches a child.
}

fn wait_for_identity(
    status: &serde_json::Value,
    path: &std::path::Path,
    key: &str,
    mut wait: impl FnMut(),
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    loop {
        match std::fs::read_to_string(path) {
            Ok(state) => match runtime_args(status, &state, key) {
                Err(error) if error.is::<PendingIdentity>() => {}
                result => return result,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // Never invent an identity while the daemon's atomic first flush is
        // pending. Keep this process alive rather than restarting the unit.
        wait();
    }
}

fn runtime_args(
    status: &serde_json::Value,
    state: &str,
    key: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let network = &status["network"];
    let primary = network["primary"].as_str().ok_or("no primary interface")?;
    let interface = network["interfaces"]
        .as_array()
        .ok_or("no interfaces")?
        .iter()
        .find(|i| i["name"].as_str() == Some(primary))
        .ok_or("primary interface missing")?;
    if interface["wireless"].as_bool() != Some(false)
        || interface["carrier"].as_bool() != Some(true)
    {
        return Err("Controller compatibility requires an active wired media interface".into());
    }
    let ip = interface["ipv4"]
        .as_str()
        .ok_or("media interface has no IPv4 address")?;
    let _: std::net::Ipv4Addr = ip.parse()?;
    let mac = interface["mac"]
        .as_str()
        .ok_or("media interface has no MAC")?
        .replace(':', "");
    if mac.len() != 12 || !mac.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid MAC".into());
    }
    let devices = status["devices"].as_array().ok_or("no devices")?;
    let index = devices
        .iter()
        .position(|d| d["key"].as_str() == Some(key))
        .or_else(|| (key == "main" && !devices.is_empty()).then_some(0))
        .ok_or(UnsupportedDevice("configured device not found"))?;
    // The existing runtime serves the base ports, not offset device ports.
    if index != 0 {
        return Err(UnsupportedDevice(
            "the existing Controller runtime serves only the first logical device's ports",
        )
        .into());
    }
    let device = &devices[index];
    let key = device["key"].as_str().ok_or("device has no key")?;
    let name = device["name"].as_str().ok_or("device has no name")?;
    // Read, never regenerate, the daemon-owned persistent identity.
    let state: toml::Value = toml::from_str(state)?;
    let id = state
        .get("device")
        .and_then(|d| d.get(key))
        .and_then(|d| d.get("wire_device_id"))
        .and_then(toml::Value::as_str)
        .ok_or(PendingIdentity)?;
    if id.len() != 16 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid device identity".into());
    }
    Ok(
        ["--ip", ip, "--device-id", id, "--mac", &mac, "--name", name]
            .map(str::to_owned)
            .to_vec(),
    )
}

fn main() -> ExitCode {
    if let Some(code) = tesseractl::runtime_build_probe("launcher") {
        return code;
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tessera-controller: {e}");
            failure_exit(e.as_ref())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;
    use tessera_control_types::{CommandError, Request, Response, Status, WIRE_VERSION};

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn with_daemon(
        script: impl FnOnce(&mut UnixStream) + Send + 'static,
        query: impl FnOnce(&Path),
    ) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let scratch = Scratch(std::env::temp_dir().join(format!(
            "tessera-launcher-api-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        )));
        std::fs::create_dir(&scratch.0).unwrap();
        let path = scratch.0.join("control.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("launcher fixture accept: {error}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            script(&mut socket);
        });
        query(&path);
        server.join().unwrap();
    }
    fn expect_query(socket: &mut UnixStream, query: Query) -> u64 {
        let mut length = [0; 4];
        socket.read_exact(&mut length).unwrap();
        let length = u32::from_be_bytes(length) as usize;
        assert!(length < 4096);
        let mut body = vec![0; length];
        socket.read_exact(&mut body).unwrap();
        let request: Request = serde_json::from_slice(&body).unwrap();
        assert_eq!(request.v, WIRE_VERSION);
        assert_eq!(request.body, RequestBody::Query(query));
        request.id
    }
    fn respond(socket: &mut UnixStream, id: u64, result: Result<Reply, CommandError>) {
        let body = serde_json::to_vec(&Response {
            v: WIRE_VERSION,
            id,
            result,
        })
        .unwrap();
        socket
            .write_all(&u32::try_from(body.len()).unwrap().to_be_bytes())
            .unwrap();
        socket.write_all(&body).unwrap();
    }
    fn expect_closed(socket: &mut UnixStream) {
        assert_eq!(socket.read(&mut [0]).unwrap(), 0);
    }
    #[test]
    fn daemon_context_uses_one_existing_api_connection_then_closes_it() {
        with_daemon(
            |socket| {
                let id = expect_query(socket, Query::Status);
                respond(
                    socket,
                    id,
                    Ok(Reply::Status(Status::new("launcher-fixture"))),
                );
                let id = expect_query(socket, Query::EffectiveConfig);
                respond(
                    socket,
                    id,
                    Ok(Reply::EffectiveConfig(
                        "[daemon]\nstate_dir='/fixture/state'\n".into(),
                    )),
                );
                expect_closed(socket);
            },
            |path| {
                let context = daemon_context(path).unwrap();
                assert_eq!(context.status["version"], "launcher-fixture");
                assert_eq!(
                    context.state_path,
                    PathBuf::from("/fixture/state/state.toml")
                );
            },
        );
    }
    #[test]
    fn daemon_refusal_cannot_proceed_to_config_or_fall_back_to_file_queries() {
        with_daemon(
            |socket| {
                expect_query(socket, Query::Status);
                // Legitimate refusal before daemon parses the rate-limited frame.
                respond(socket, 0, Err(CommandError::RateLimited));
                expect_closed(socket);
            },
            |path| {
                let error = daemon_context(path).unwrap_err();
                assert!(matches!(
                    error.downcast_ref::<tesseractl::client::ClientError>(),
                    Some(tesseractl::client::ClientError::Command(
                        CommandError::RateLimited
                    ))
                ));
            },
        );
    }
    #[test]
    fn wrong_or_invalid_effective_config_stops_launcher_preparation() {
        for reply in [
            Reply::Done,
            Reply::EffectiveConfig("invalid=[".into()),
            Reply::EffectiveConfig("[daemon]\n".into()),
        ] {
            with_daemon(
                move |socket| {
                    let id = expect_query(socket, Query::Status);
                    respond(socket, id, Ok(Reply::Status(Status::new("test"))));
                    let id = expect_query(socket, Query::EffectiveConfig);
                    respond(socket, id, Ok(reply));
                    expect_closed(socket);
                },
                |path| assert!(daemon_context(path).is_err()),
            );
        }
    }

    #[test]
    fn first_flush_waits_in_process_and_invalid_state_is_not_hidden() {
        let path =
            std::env::temp_dir().join(format!("tessera-controller-state-{}", std::process::id()));
        let status = serde_json::json!({
            "network": {"primary":"eth0", "interfaces":[
                {"name":"eth0", "ipv4":"192.0.2.3", "mac":"00:11:22:33:44:55", "wireless":false,"carrier":true}]},
            "devices":[{"key":"main","name":"Studio"}]
        });
        let mut waits = 0;
        let args = wait_for_identity(&status, &path, "main", || {
            waits += 1;
            std::fs::write(
                &path,
                if waits == 1 {
                    ""
                } else {
                    "[device.main]\nwire_device_id = '0123456789abcdef'\n"
                },
            )
            .unwrap();
        })
        .unwrap();
        assert_eq!(waits, 2);
        assert_eq!(args[3], "0123456789abcdef");
        std::fs::write(&path, "invalid = [").unwrap();
        assert!(
            wait_for_identity(&status, &path, "main", || panic!("invalid state must fail"))
                .is_err()
        );
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn unsupported_device_stops_retries_but_transient_errors_retry() {
        assert_eq!(
            failure_exit(&UnsupportedDevice("retired key")),
            ExitCode::from(78)
        );
        assert_eq!(
            failure_exit(&std::io::Error::other("not ready")),
            ExitCode::FAILURE
        );
    }
    #[test]
    fn uses_current_wired_address_and_preserves_identity_and_name() {
        let mut status = serde_json::json!({
            "network": {"primary":"eth0", "interfaces":[
                {"name":"wlan0", "ipv4":"192.0.2.2", "wireless":true,"carrier":true},
                {"name":"eth0", "ipv4":"192.0.2.3", "mac":"00:11:22:33:44:55", "wireless":false,"carrier":true}]},
            "devices":[{"key":"main","name":"Studio"}]
        });
        let state = "[device.main]\nwire_device_id = '0123456789abcdef'\n";
        let args = runtime_args(&status, state, "main").unwrap();
        assert_eq!(
            &args[..8],
            [
                "--ip",
                "192.0.2.3",
                "--device-id",
                "0123456789abcdef",
                "--mac",
                "001122334455",
                "--name",
                "Studio"
            ]
        );
        status["network"]["interfaces"][1]["ipv4"] = "192.0.2.4".into();
        assert_eq!(
            runtime_args(&status, state, "main").unwrap()[1],
            "192.0.2.4"
        );
        status["devices"][0]["key"] = "custom".into();
        let custom_state = "[device.custom]\nwire_device_id = '0123456789abcdef'\n";
        assert!(runtime_args(&status, custom_state, "main").is_ok());
        status["devices"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"key":"second", "name":"Other"}));
        for key in ["second", "retired-user"] {
            let error = runtime_args(&status, custom_state, key).unwrap_err();
            assert_eq!(failure_exit(error.as_ref()), ExitCode::from(78));
        }
        status["network"]["primary"] = "wlan0".into();
        assert!(runtime_args(&status, state, "main").is_err());
        assert!(runtime_args(&status, "", "main").is_err());
    }
}
