//! The injectable environment: everything the crate would otherwise have to
//! read from the host or the clock.
//!
//! Hostname, interface existence, the platform latency floor, identity
//! generation and time all come in through this module so that validation and
//! persistence are deterministic in tests. Nothing here hard-codes the ADR 0011
//! timing estimates: [`Environment::latency_floor_us`] is a caller-supplied
//! value (the defaults are merely plausible starting points).

use std::collections::BTreeMap;
use std::fmt;
use std::hash::{BuildHasher, Hasher, RandomState};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Monotonic time source. The value is the time since an arbitrary, fixed
/// origin; only differences are meaningful.
pub trait Clock: Send + Sync + fmt::Debug {
    /// Current monotonic time.
    fn now(&self) -> Duration;
}

/// [`Clock`] backed by [`Instant`].
#[derive(Debug)]
pub struct MonotonicClock {
    origin: Instant,
}

impl MonotonicClock {
    /// A clock whose origin is "now".
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// A [`Clock`] that only moves when told to. Clones share the same time.
#[derive(Debug, Clone, Default)]
pub struct ManualClock {
    nanos: Arc<AtomicU64>,
}

impl ManualClock {
    /// A clock starting at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Move time forward.
    pub fn advance(&self, by: Duration) {
        let n = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
        self.nanos.fetch_add(n, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(Ordering::SeqCst))
    }
}

/// What the host knows about a network interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterfaceInfo {
    /// The interface is a wireless (Wi-Fi) interface.
    pub wireless: bool,
}

/// Answers "does this interface exist, and is it wireless?".
pub trait InterfaceProbe: Send + Sync + fmt::Debug {
    /// `None` if the interface does not exist.
    fn lookup(&self, name: &str) -> Option<InterfaceInfo>;
}

/// A fixed set of interfaces (tests, or hosts configured by the caller).
#[derive(Debug, Clone, Default)]
pub struct StaticInterfaces {
    map: BTreeMap<String, InterfaceInfo>,
}

impl StaticInterfaces {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a wired interface.
    #[must_use]
    pub fn with_wired(mut self, name: &str) -> Self {
        self.map
            .insert(name.to_owned(), InterfaceInfo { wireless: false });
        self
    }

    /// Add a wireless interface.
    #[must_use]
    pub fn with_wireless(mut self, name: &str) -> Self {
        self.map
            .insert(name.to_owned(), InterfaceInfo { wireless: true });
        self
    }
}

impl InterfaceProbe for StaticInterfaces {
    fn lookup(&self, name: &str) -> Option<InterfaceInfo> {
        self.map.get(name).copied()
    }
}

/// Looks interfaces up under `/sys/class/net`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemInterfaces;

impl InterfaceProbe for SystemInterfaces {
    fn lookup(&self, name: &str) -> Option<InterfaceInfo> {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return None;
        }
        let base = std::path::Path::new("/sys/class/net").join(name);
        if !base.exists() {
            return None;
        }
        let wireless = base.join("wireless").exists() || base.join("phy80211").exists();
        Some(InterfaceInfo { wireless })
    }
}

/// Source of the 8-byte device identities generated once per logical device.
pub trait DeviceIdSource: Send + Sync + fmt::Debug {
    /// A fresh identity. Must not repeat across calls.
    fn new_wire_device_id(&self) -> [u8; 8];
}

/// Random identities from the standard library's randomly seeded hasher, with
/// the locally-administered bit set and the multicast bit clear in the first
/// byte (the way an EUI-64 would mark a non-vendor address). Whether
/// real-world peers need a particular structure is not specified (open
/// question for the protocol specs); the value is opaque to this crate.
#[derive(Debug, Clone, Copy, Default)]
pub struct RandomIds;

impl DeviceIdSource for RandomIds {
    fn new_wire_device_id(&self) -> [u8; 8] {
        let mut h = RandomState::new().build_hasher();
        h.write_u8(1);
        let mut id = h.finish().to_be_bytes();
        id[0] = (id[0] | 0x02) & !0x01;
        id
    }
}

/// Deterministic identities `02 00 00 00 00 00 00 NN`, for tests.
#[derive(Debug, Default)]
pub struct CountingIds {
    next: Mutex<u8>,
}

impl DeviceIdSource for CountingIds {
    fn new_wire_device_id(&self) -> [u8; 8] {
        let mut n = self
            .next
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *n = n.wrapping_add(1);
        [0x02, 0, 0, 0, 0, 0, 0, *n]
    }
}

/// Per-peer rate limit for commands arriving from the network
/// (ADR 0010). A token bucket per peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    /// Bucket size: how many commands a peer may send back-to-back.
    pub burst: u32,
    /// Sustained commands per second.
    pub per_second: u32,
    /// Maximum number of peers tracked at once. When exceeded the peer that
    /// was seen longest ago is forgotten (it then starts with a full bucket).
    pub max_peers: usize,
}

impl Default for RateLimit {
    fn default() -> Self {
        Self {
            burst: 256,
            per_second: 128,
            max_peers: 1024,
        }
    }
}

/// Everything the loader, validator and engine take from the outside world.
#[derive(Debug, Clone)]
pub struct Environment {
    /// Host name, used to derive default device names.
    pub hostname: String,
    /// Default `latency_us` for devices that do not set one.
    pub default_latency_us: u32,
    /// Default `latency_bounds_us` for devices that do not set them.
    pub default_latency_bounds_us: [u32; 2],
    /// Platform timing floor (ADR 0011): latencies below it are an error
    /// unless the device sets `allow_unsafe_latency`, in which case they are
    /// a warning. Supplied by the caller from measurement or profile.
    pub latency_floor_us: u32,
    /// Sample rates a device may use.
    pub supported_sample_rates: Vec<u32>,
    /// Wire encodings (bits per sample) a device may use.
    pub supported_encodings: Vec<u8>,
    /// Maximum length of device names and channel labels in bytes.
    pub max_name_bytes: usize,
    /// Interface lookup.
    pub interfaces: Arc<dyn InterfaceProbe>,
    /// Identity generation.
    pub ids: Arc<dyn DeviceIdSource>,
    /// Rate limit for protocol-originated commands; `None` disables it.
    pub rate_limit: Option<RateLimit>,
    /// Coalescing window for state-file writes.
    pub persist_delay: Duration,
    /// The program can run `[[bridge]]` entries (in `tesserad`: built with
    /// the `alsa-bridge` feature). When `false`, a configuration with any
    /// `[[bridge]]` entry is refused, so a build without the bridge never
    /// silently ignores one.
    pub bridges_supported: bool,
    /// The process's `RLIMIT_MEMLOCK` soft limit in bytes, if the caller
    /// looked it up (`None`: unknown or unlimited). With `[engine] mlock`,
    /// an `[audio] ring_budget_mib` above it is reported as a warning: the
    /// rings could not all be locked in memory.
    pub memlock_limit_bytes: Option<u64>,
}

/// Default sample rates from `docs/configuration.md`.
pub const DEFAULT_SAMPLE_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// Name length limit in bytes (`spec/protocol/settings.md`, draft).
pub const DEFAULT_MAX_NAME_BYTES: usize = 31;

impl Environment {
    /// A deterministic environment with the documented defaults and no known
    /// network interfaces. Suitable for tests and as a base for
    /// builder-style overrides.
    #[must_use]
    pub fn new(hostname: &str) -> Self {
        let arm = cfg!(any(target_arch = "arm", target_arch = "aarch64"));
        Self {
            hostname: hostname.to_owned(),
            default_latency_us: if arm { 10_000 } else { 4_000 },
            default_latency_bounds_us: [1_000, 40_000],
            latency_floor_us: if arm { 2_000 } else { 1_000 },
            supported_sample_rates: DEFAULT_SAMPLE_RATES.to_vec(),
            supported_encodings: vec![16, 24, 32],
            max_name_bytes: DEFAULT_MAX_NAME_BYTES,
            interfaces: Arc::new(StaticInterfaces::new()),
            ids: Arc::new(RandomIds),
            rate_limit: Some(RateLimit::default()),
            persist_delay: Duration::from_millis(500),
            // Safe default: refuse `[[bridge]]` unless the program says it
            // can run them (`tesserad` built with `alsa-bridge`).
            bridges_supported: false,
            memlock_limit_bytes: None,
        }
    }

    /// The real host: its hostname, `/sys/class/net`, random identities.
    #[must_use]
    pub fn detect() -> Self {
        let mut e = Self::new(&system_hostname());
        e.interfaces = Arc::new(SystemInterfaces);
        e
    }

    /// Replace the interface probe.
    #[must_use]
    pub fn with_interfaces(mut self, probe: impl InterfaceProbe + 'static) -> Self {
        self.interfaces = Arc::new(probe);
        self
    }

    /// Replace the identity source.
    #[must_use]
    pub fn with_ids(mut self, ids: impl DeviceIdSource + 'static) -> Self {
        self.ids = Arc::new(ids);
        self
    }
}

fn system_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "tessera".to_owned())
}

/// Check a device name or channel label against the wire rules: non-empty,
/// at most `max_bytes` bytes of UTF-8, no NUL or other control characters
/// (wire strings are NUL-terminated; `spec/protocol/overview.md`,
/// `spec/protocol/settings.md`).
///
/// # Errors
/// A human-readable reason when the name is not acceptable.
pub fn validate_name(name: &str, max_bytes: usize) -> Result<(), String> {
    if name.is_empty() {
        return Err("must not be empty".to_owned());
    }
    if name.len() > max_bytes {
        return Err(format!(
            "is {} bytes long, the limit is {max_bytes} bytes",
            name.len()
        ));
    }
    if let Some(c) = name.chars().find(|c| c.is_control()) {
        return Err(format!(
            "contains the control character U+{:04X}",
            u32::from(c)
        ));
    }
    Ok(())
}

/// Shorten `s` to at most `max_bytes` bytes at a character boundary and drop
/// control characters. Used for hostname-derived default names.
#[must_use]
pub fn truncate_name(s: &str, max_bytes: usize) -> String {
    let mut out = String::new();
    for c in s.chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > max_bytes {
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_rules() {
        assert!(validate_name("studio-pc", 31).is_ok());
        assert!(validate_name("", 31).is_err());
        assert!(validate_name(&"a".repeat(32), 31).is_err());
        assert!(validate_name(&"a".repeat(31), 31).is_ok());
        assert!(validate_name("a\0b", 31).is_err());
        // Multi-byte characters count in bytes.
        assert!(validate_name(&"é".repeat(16), 31).is_err());
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let t = truncate_name(&"é".repeat(40), 31);
        assert_eq!(t.len(), 30);
        assert!(validate_name(&t, 31).is_ok());
        assert_eq!(truncate_name("a\0b", 31), "ab");
    }

    #[test]
    fn manual_clock_is_shared() {
        let c = ManualClock::new();
        let d = c.clone();
        c.advance(Duration::from_millis(5));
        assert_eq!(d.now(), Duration::from_millis(5));
    }

    #[test]
    fn counting_ids_are_distinct() {
        let ids = CountingIds::default();
        assert_ne!(ids.new_wire_device_id(), ids.new_wire_device_id());
    }

    #[test]
    fn random_ids_have_local_bit() {
        let id = RandomIds.new_wire_device_id();
        assert_eq!(id[0] & 0x03, 0x02);
    }
}
