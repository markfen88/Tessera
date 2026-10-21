//! What the Speaker page knows about the host itself: temperature, uptime,
//! host name (read from the kernel, no commands) and the connected
//! Bluetooth devices (`bluetoothctl`, through the [`Runner`], only while the
//! `bluetooth` unit runs: with it stopped the command blocks for seconds).
//! Interfaces come from the daemon's status, not from here.

use std::path::Path;
use std::time::Duration;

use crate::appliance::services::Runner;

/// The thermal zone read for the temperature chip (absent on many hosts:
/// then no chip).
pub const THERMAL_ZONE: &str = "/sys/class/thermal/thermal_zone0/temp";
/// Seconds since boot.
pub const UPTIME: &str = "/proc/uptime";
/// The Bluetooth command (looked up on `PATH`).
pub const BLUETOOTHCTL: &str = "bluetoothctl";
/// The unit whose activity decides whether Bluetooth is asked at all.
pub const BLUETOOTH_UNIT: &str = "bluetooth.service";

/// A snapshot of the host.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HostInfo {
    /// The kernel host name (`""` if it cannot be read).
    pub hostname: String,
    /// Degrees Celsius, if the host reports a temperature.
    pub temperature_c: Option<f32>,
    /// Time since boot, if `/proc/uptime` is readable.
    pub uptime: Option<Duration>,
}

impl HostInfo {
    /// Read the host's files.
    #[must_use]
    pub fn read() -> Self {
        Self::read_from(Path::new(THERMAL_ZONE), Path::new(UPTIME))
    }

    /// Read from the given files (for tests).
    #[must_use]
    pub fn read_from(thermal: &Path, uptime: &Path) -> Self {
        Self {
            hostname: nix::unistd::gethostname()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_default(),
            temperature_c: std::fs::read_to_string(thermal)
                .ok()
                .and_then(|t| parse_temperature(&t)),
            uptime: std::fs::read_to_string(uptime)
                .ok()
                .and_then(|t| parse_uptime(&t)),
        }
    }
}

/// `/sys/class/thermal/*/temp` is millidegrees.
#[must_use]
pub fn parse_temperature(text: &str) -> Option<f32> {
    let milli: i64 = text.trim().parse().ok()?;
    // A sensor that is absent or broken reports nonsense; show nothing then.
    (-50_000..=200_000)
        .contains(&milli)
        .then(|| milli as f32 / 1000.0)
}

/// `/proc/uptime`: `<seconds since boot> <idle seconds>`.
#[must_use]
pub fn parse_uptime(text: &str) -> Option<Duration> {
    let secs: f64 = text.split_whitespace().next()?.parse().ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs))
}

/// `1 h 12 m`, `3 d 2 h`, `5 m`, `20 s`.
#[must_use]
pub fn format_uptime(d: Duration) -> String {
    let s = d.as_secs();
    let (days, hours, mins) = (s / 86_400, (s / 3600) % 24, (s / 60) % 60);
    if days > 0 {
        format!("{days} d {hours} h")
    } else if hours > 0 {
        format!("{hours} h {mins} m")
    } else if mins > 0 {
        format!("{mins} m")
    } else {
        format!("{s} s")
    }
}

/// `53 °C` (rounded to a degree).
#[must_use]
pub fn format_temperature(c: f32) -> String {
    format!("{} °C", c.round() as i32)
}

/// What `bluetoothctl` said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bluetooth {
    /// The command failed or could not be run.
    Unavailable,
    /// The names of the connected devices (possibly none).
    Connected(Vec<String>),
}

/// The connected Bluetooth devices, as `bluetoothctl devices Connected`
/// lists them (BlueZ 5.65 or newer; older versions ignore the filter and
/// list every known device). Ask only while [`BLUETOOTH_UNIT`] is active;
/// the caller decides that.
pub async fn bluetooth_connected(r: &dyn Runner) -> Bluetooth {
    let args = [String::from("devices"), String::from("Connected")];
    match r.run(BLUETOOTHCTL, &args).await {
        Ok(o) if o.success() => Bluetooth::Connected(parse_connected(&o.stdout)),
        _ => Bluetooth::Unavailable,
    }
}

/// Lines of `bluetoothctl devices Connected`: `Device AA:BB:CC:DD:EE:FF Name
/// with spaces`. The name is shown; the address only if there is no name.
#[must_use]
pub fn parse_connected(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|l| {
            let rest = l.trim().strip_prefix("Device ")?;
            let (addr, name) = rest.split_once(' ').unwrap_or((rest, ""));
            let name = name.trim();
            if addr.is_empty() {
                return None;
            }
            Some(if name.is_empty() { addr } else { name }.to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appliance::services::{FakeRunner, Output, RunError};

    #[test]
    fn host_files_are_parsed_and_missing_ones_are_tolerated() {
        assert_eq!(parse_temperature("53456\n"), Some(53.456));
        assert_eq!(parse_temperature("x"), None);
        assert_eq!(parse_temperature("999999"), None);
        assert_eq!(
            parse_uptime("4321.55 8000.00\n"),
            Some(Duration::from_secs_f64(4321.55))
        );
        assert_eq!(parse_uptime(""), None);
        let h = HostInfo::read_from(Path::new("/nonexistent/temp"), Path::new("/nonexistent/up"));
        assert_eq!(h.temperature_c, None);
        assert_eq!(h.uptime, None);
    }

    #[test]
    fn formatting() {
        assert_eq!(format_uptime(Duration::from_secs(20)), "20 s");
        assert_eq!(format_uptime(Duration::from_secs(5 * 60 + 9)), "5 m");
        assert_eq!(
            format_uptime(Duration::from_secs(3600 + 12 * 60)),
            "1 h 12 m"
        );
        assert_eq!(
            format_uptime(Duration::from_secs(3 * 86_400 + 2 * 3600 + 59)),
            "3 d 2 h"
        );
        assert_eq!(format_temperature(53.456), "53 °C");
    }

    #[test]
    fn bluetooth_list_is_parsed_by_name() {
        let out = "Device 00:11:22:33:44:55 Phone 9\nDevice AA:BB:CC:DD:EE:FF \nnoise\n";
        assert_eq!(parse_connected(out), ["Phone 9", "AA:BB:CC:DD:EE:FF"]);
        assert!(parse_connected("").is_empty());
    }

    #[tokio::test]
    async fn bluetooth_failure_differs_from_nothing_connected() {
        let r = FakeRunner::default();
        r.script(&[BLUETOOTHCTL, "devices", "Connected"], Output::ok(""));
        assert_eq!(
            bluetooth_connected(&r).await,
            Bluetooth::Connected(Vec::new())
        );
        r.script(
            &[BLUETOOTHCTL, "devices", "Connected"],
            Output::fail(1, "No default controller available\n"),
        );
        assert_eq!(bluetooth_connected(&r).await, Bluetooth::Unavailable);
        r.script_error(&[BLUETOOTHCTL, "devices", "Connected"], RunError::Timeout);
        assert_eq!(bluetooth_connected(&r).await, Bluetooth::Unavailable);
    }
}
