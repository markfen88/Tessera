//! Launch the existing Controller runtime with the daemon's current identity
//! and wired interface. Protocol handling stays in the compatibility runtime.
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

#[derive(Debug)]
struct UnsupportedDevice(&'static str);

impl std::fmt::Display for UnsupportedDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for UnsupportedDevice {}

fn failure_exit(error: &(dyn std::error::Error + 'static)) -> ExitCode {
    if error.is::<UnsupportedDevice>() {
        ExitCode::from(78)
    } else {
        ExitCode::FAILURE
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let key = std::env::args().nth(1).unwrap_or_else(|| "main".into());
    let output = Command::new("/usr/bin/tesseractl")
        .args(["--json", "status"])
        .output()?;
    if !output.status.success() {
        return Err("cannot read daemon status".into());
    }
    let status: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let config = Command::new("/usr/bin/tesseractl")
        .args(["--json", "config", "show", "--effective"])
        .output()?;
    if !config.status.success() {
        return Err("cannot read effective daemon configuration".into());
    }
    let config: serde_json::Value = serde_json::from_slice(&config.stdout)?;
    let config: toml::Value = toml::from_str(
        config["effective_config"]
            .as_str()
            .ok_or("missing effective configuration")?,
    )?;
    let state_dir = config
        .get("daemon")
        .and_then(|d| d.get("state_dir"))
        .and_then(toml::Value::as_str)
        .ok_or("missing daemon state directory")?;
    let state_path = std::path::Path::new(state_dir).join("state.toml");
    let args = runtime_args(&status, &std::fs::read_to_string(state_path)?, &key)?;
    let preset = std::fs::read_to_string("/usr/lib/tessera/controller.args")?;
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
        .ok_or("daemon has not persisted its device identity yet")?;
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
