//! Configuration of the discovery layer.

use core::fmt;

use crate::provisional::DEFAULT_MAX_HOSTNAME_LEN;
use crate::txt::TxtLimits;

/// Which mDNS implementation answers on the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Use Avahi if `avahi-daemon` owns its name on the system bus, otherwise
    /// the built-in responder. Never both.
    #[default]
    Auto,
    /// Always use the built-in responder (`mdns-sd`). If Avahi is also
    /// running, two responders share port 5353: a warning is logged.
    Builtin,
    /// Always use Avahi over D-Bus; fail if it is not on the system bus.
    Avahi,
}

/// The backend that was actually selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// The built-in `mdns-sd` responder/browser.
    Builtin,
    /// Avahi over D-Bus.
    Avahi,
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BackendKind::Builtin => "builtin",
            BackendKind::Avahi => "avahi",
        })
    }
}

/// IP address families used for mDNS traffic and published addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IpFamilies {
    /// IPv4 only (224.0.0.251). The spec says the reference never uses IPv6
    /// (`likely`), so this is the default.
    #[default]
    V4,
    /// IPv6 only (ff02::fb).
    V6,
    /// Both.
    Both,
}

impl IpFamilies {
    /// True if IPv4 is used.
    #[must_use]
    pub fn v4(self) -> bool {
        matches!(self, IpFamilies::V4 | IpFamilies::Both)
    }
    /// True if IPv6 is used.
    #[must_use]
    pub fn v6(self) -> bool {
        matches!(self, IpFamilies::V6 | IpFamilies::Both)
    }
}

/// Size and naming limits applied to everything handed to a responder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// TXT record limits.
    pub txt: TxtLimits,
    /// Maximum bytes of a service instance name (a DNS label, RFC 6763 4.1.1).
    pub max_instance_len: usize,
    /// Maximum bytes of a friendly hostname. Provisional, see
    /// [`crate::provisional::DEFAULT_MAX_HOSTNAME_LEN`].
    pub max_hostname_len: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            txt: TxtLimits::default(),
            max_instance_len: 63,
            max_hostname_len: DEFAULT_MAX_HOSTNAME_LEN,
        }
    }
}

/// Settings for [`crate::Discovery::start`].
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    /// Backend selection.
    pub mode: Mode,
    /// Interfaces (by name) to answer and browse on, used as given. Empty means
    /// the first wired interface that has carrier (ADR 0010): not loopback,
    /// wireless or virtual (bridge, veth, docker, tun...), and not down; if
    /// there is none, starting fails with `Error::NoDefaultInterface`.
    pub interfaces: Vec<String>,
    /// Address families.
    pub ip_families: IpFamilies,
    /// Deliver our own multicast packets to other sockets on this host, so
    /// several instances on one machine see each other (spec section 1,
    /// `likely`). Builtin backend only.
    pub multicast_loopback: bool,
    /// Limits on registered data.
    pub limits: Limits,
    /// Per service kind, how many distinct instances a browser tracks. Further
    /// instances are dropped and counted (untrusted LAN input).
    pub max_instances_per_kind: usize,
    /// Capacity of the event channels. Backends never wait for a reader: when
    /// a channel is full the event is dropped and counted (and, for browsers,
    /// an `Overflow` marker is delivered; see `Browser::snapshot`).
    pub event_capacity: usize,
    /// What to do when another process already has UDP port 5353 bound on one
    /// of our interfaces and the built-in responder is used (mDNS sockets can
    /// share the port with `SO_REUSEADDR`, but two responders answer twice).
    pub port_conflict: PortConflict,
    /// How often the Avahi backend re-checks the addresses of its interfaces
    /// (there is no netlink watcher). Zero disables the check.
    pub address_recheck: std::time::Duration,
    /// Test hook, not part of the API: lets a test stop the built-in
    /// `update` at a known point (see [`TestHooks`]).
    #[doc(hidden)]
    pub test_hooks: Option<std::sync::Arc<TestHooks>>,
}

/// A checkpoint inside the built-in `update`, after the withdrawn services
/// have been unregistered and before the changed ones are registered. When
/// armed, `update` parks there until [`TestHooks::resume`], so a test can drop
/// the caller's future at exactly that point without depending on timing.
#[doc(hidden)]
#[derive(Debug)]
pub struct TestHooks {
    armed: std::sync::atomic::AtomicBool,
    paused: tokio::sync::Notify,
    resume: tokio::sync::Semaphore,
}

impl TestHooks {
    /// New, not armed.
    #[must_use]
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            armed: std::sync::atomic::AtomicBool::new(false),
            paused: tokio::sync::Notify::new(),
            resume: tokio::sync::Semaphore::new(0),
        })
    }

    /// The next `update` parks at the checkpoint.
    pub fn arm(&self) {
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Completes when an armed `update` has reached the checkpoint.
    pub async fn paused(&self) {
        self.paused.notified().await;
    }

    /// Lets the parked `update` go on.
    pub fn resume(&self) {
        self.resume.add_permits(1);
    }

    pub(crate) async fn checkpoint(&self) {
        if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.paused.notify_one();
            if let Ok(p) = self.resume.acquire().await {
                p.forget();
            }
        }
    }
}

/// Policy for [`DiscoveryConfig::port_conflict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PortConflict {
    /// Log a warning and carry on.
    #[default]
    Warn,
    /// Refuse to start the built-in responder.
    Refuse,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Auto,
            interfaces: Vec::new(),
            ip_families: IpFamilies::V4,
            multicast_loopback: true,
            limits: Limits::default(),
            max_instances_per_kind: 4096,
            event_capacity: 1024,
            port_conflict: PortConflict::Warn,
            address_recheck: std::time::Duration::from_secs(5),
            test_hooks: None,
        }
    }
}
