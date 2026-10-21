//! A minimal `sd_notify(3)` client (no crate, no libsystemd).
//!
//! If `NOTIFY_SOCKET` is not set every call is a no-op, so the daemon runs
//! the same way under systemd, from a shell, and in tests.

use std::io;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::time::Duration;

/// Sends service-manager notifications.
#[derive(Debug)]
pub struct Notifier {
    target: Option<(UnixDatagram, SocketAddr)>,
}

impl Notifier {
    /// A notifier that discards everything.
    #[must_use]
    pub fn disabled() -> Self {
        Self { target: None }
    }

    /// Use `NOTIFY_SOCKET` from the environment if it is set.
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var("NOTIFY_SOCKET") {
            Ok(p) if !p.is_empty() => Self::to_socket(&p).unwrap_or_else(|_| Self::disabled()),
            _ => Self::disabled(),
        }
    }

    /// Send to `path` (a filesystem path, or `@name` for an abstract socket).
    ///
    /// # Errors
    /// The socket cannot be created or the address is invalid.
    pub fn to_socket(path: &str) -> io::Result<Self> {
        let addr = match path.strip_prefix('@') {
            Some(name) => SocketAddr::from_abstract_name(name.as_bytes())?,
            None => SocketAddr::from_pathname(path)?,
        };
        Ok(Self {
            target: Some((UnixDatagram::unbound()?, addr)),
        })
    }

    /// Is a service manager listening?
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.target.is_some()
    }

    /// Send a raw state string such as `READY=1`.
    ///
    /// # Errors
    /// The datagram could not be sent. Callers log and carry on: a missing
    /// notification must never stop the daemon.
    pub fn send(&self, state: &str) -> io::Result<()> {
        if let Some((sock, addr)) = &self.target {
            sock.send_to_addr(state.as_bytes(), addr)?;
        }
        Ok(())
    }

    /// `RELOADING=1` with the `MONOTONIC_USEC` that `Type=notify-reload`
    /// requires; follow with [`ready`](Self::ready) when the reload is done.
    pub fn reloading(&self) {
        let usec = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)
            .map(|t| {
                u64::try_from(t.tv_sec()).unwrap_or(0) * 1_000_000
                    + u64::try_from(t.tv_nsec()).unwrap_or(0) / 1000
            })
            .unwrap_or(0);
        let _ = self.send(&format!("RELOADING=1\nMONOTONIC_USEC={usec}"));
    }

    /// `READY=1`.
    pub fn ready(&self) {
        let _ = self.send("READY=1");
    }

    /// `STOPPING=1`.
    pub fn stopping(&self) {
        let _ = self.send("STOPPING=1");
    }

    /// `WATCHDOG=1`.
    pub fn watchdog(&self) {
        let _ = self.send("WATCHDOG=1");
    }

    /// `STATUS=<text>` (newlines are replaced).
    pub fn status(&self, text: &str) {
        let _ = self.send(&format!("STATUS={}", text.replace('\n', " ")));
    }
}

/// The watchdog period systemd expects, from `WATCHDOG_USEC` (honouring
/// `WATCHDOG_PID`), halved as the manual recommends for the ping interval.
#[must_use]
pub fn watchdog_interval_from(
    usec: Option<&str>,
    pid: Option<&str>,
    own_pid: u32,
) -> Option<Duration> {
    if let Some(p) = pid {
        if p.trim().parse::<u32>().ok()? != own_pid {
            return None;
        }
    }
    let usec: u64 = usec?.trim().parse().ok()?;
    if usec == 0 {
        return None;
    }
    Some(Duration::from_micros(usec / 2).max(Duration::from_millis(1)))
}

/// [`watchdog_interval_from`] reading the process environment.
#[must_use]
pub fn watchdog_interval() -> Option<Duration> {
    watchdog_interval_from(
        std::env::var("WATCHDOG_USEC").ok().as_deref(),
        std::env::var("WATCHDOG_PID").ok().as_deref(),
        std::process::id(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_notifier_is_a_no_op() {
        let n = Notifier::disabled();
        assert!(!n.is_enabled());
        n.ready();
        n.stopping();
        assert!(n.send("X=1").is_ok());
    }

    #[test]
    fn datagrams_reach_a_path_socket() {
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = std::env::temp_dir().join(format!("tesserad-notify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = Cleanup(dir.clone());
        let path = dir.join("notify");
        let server = UnixDatagram::bind(&path).unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let n = Notifier::to_socket(path.to_str().unwrap()).unwrap();
        n.ready();
        n.status("a\nb");
        n.watchdog();
        n.stopping();
        let mut got = Vec::new();
        for _ in 0..4 {
            let mut buf = [0u8; 64];
            let len = server.recv(&mut buf).unwrap();
            got.push(String::from_utf8_lossy(&buf[..len]).into_owned());
        }
        assert_eq!(got, ["READY=1", "STATUS=a b", "WATCHDOG=1", "STOPPING=1"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn datagrams_reach_an_abstract_socket() {
        let name = format!("tesserad-notify-abs-{}", std::process::id());
        let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let server = UnixDatagram::bind_addr(&addr).unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        Notifier::to_socket(&format!("@{name}")).unwrap().ready();
        let mut buf = [0u8; 16];
        let len = server.recv(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"READY=1");
    }

    #[test]
    fn watchdog_interval_follows_systemd_rules() {
        let half = Some(Duration::from_millis(500));
        assert_eq!(watchdog_interval_from(Some("1000000"), None, 5), half);
        assert_eq!(watchdog_interval_from(Some("1000000"), Some("5"), 5), half);
        assert_eq!(watchdog_interval_from(Some("1000000"), Some("6"), 5), None);
        assert_eq!(watchdog_interval_from(Some("0"), None, 5), None);
        assert_eq!(watchdog_interval_from(None, None, 5), None);
        assert_eq!(watchdog_interval_from(Some("x"), None, 5), None);
    }
}
