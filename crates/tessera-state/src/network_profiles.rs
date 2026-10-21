//! Standalone host networking contract. Separate from the audio daemon's
//! runtime interface selection: a privileged helper owns OS configuration.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

/// Increment when the host-networking wire contract becomes incompatible.
pub const NETWORK_API: u32 = 1;
/// An explicit keep is required this many seconds after activation succeeds.
pub const CONFIRM_SECONDS: u64 = 60;
/// Give the server-rendered confirmation page time to load before switching.
pub const APPLY_DELAY_SECONDS: u64 = 3;
/// Initial guard includes the bounded apply job and the full keep window.
pub const GUARD_SECONDS: u64 = 180;

/// Supported persistent configuration backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// systemd-networkd's native files.
    Networkd,
    /// Netplan with its networkd renderer.
    Netplan,
}

/// One IPv4 network on a wired port. None means the switch's untagged VLAN.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    /// Stable identifier used by assignments.
    pub id: String,
    /// Operator-visible name.
    pub name: String,
    /// IEEE 802.1Q tag, or untagged.
    pub vlan_id: Option<u16>,
    /// DHCP or one static IPv4 address.
    pub ipv4: Ipv4,
}

/// Addressing. Only management may install a default route or DNS servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Ipv4 {
    /// Obtain an address automatically.
    Dhcp,
    /// Configure a single IPv4 subnet.
    Static {
        /// Host address.
        address: Ipv4Addr,
        /// CIDR length.
        prefix: u8,
        /// Default router (management only).
        gateway: Option<Ipv4Addr>,
        /// DNS servers (management only).
        dns: Vec<Ipv4Addr>,
    },
}

/// Complete transaction; profiles and role assignments always commit together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profiles {
    /// Document schema version.
    pub version: u32,
    /// Optimistic concurrency guard.
    pub revision: u64,
    /// Physical wired interface; fixed once adopted.
    pub interface: String,
    /// Detected OS backend; callers cannot change it.
    pub backend: Backend,
    /// Configured VLANs and optional untagged network.
    pub networks: Vec<Network>,
    /// Web management network ID.
    pub management: String,
    /// Audio, discovery, control and PTP network ID.
    pub audio: String,
}

/// Strict interface names, safe in both OS configuration and nft expressions.
#[must_use]
pub fn valid_interface(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 15
        && s != "lo"
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

impl Profiles {
    /// The Linux interface belonging to a network.
    #[must_use]
    pub fn ifname(&self, n: &Network) -> String {
        n.vlan_id.map_or_else(
            || self.interface.clone(),
            |v| format!("{}.{v}", self.interface),
        )
    }

    /// Resolve an assigned ID, after validation.
    pub fn assigned(&self, id: &str) -> Result<&Network, String> {
        self.networks
            .iter()
            .find(|n| n.id == id)
            .ok_or_else(|| "An assigned network was removed. Select a replacement first.".into())
    }

    /// Validate the whole candidate, including deletions, routes and subnets.
    pub fn validate(&self, previous: &Self) -> Result<(), String> {
        if self.version != NETWORK_API
            || self.interface != previous.interface
            || self.backend != previous.backend
            || self.revision != previous.revision
        {
            return Err(
                "Network version, port, backend or revision changed; reload the page.".into(),
            );
        }
        if !valid_interface(&self.interface)
            || self.interface.contains('.')
            || self.networks.is_empty()
        {
            return Err("Choose a physical wired port and at least one network.".into());
        }
        self.assigned(&self.management)?;
        self.assigned(&self.audio)?;
        let mut ids = BTreeSet::new();
        let mut tags = BTreeSet::new();
        let mut subnets = Vec::new();
        for n in &self.networks {
            if n.id.is_empty()
                || n.id.len() > 32
                || !n
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                || !ids.insert(&n.id)
                || !tags.insert(n.vlan_id)
                || !valid_interface(&self.ifname(n))
            {
                return Err(
                    "Network IDs, VLAN tags and interface names must be unique and valid.".into(),
                );
            }
            if n.name.trim().is_empty()
                || n.name.len() > 64
                || n.name.chars().any(char::is_control)
                || n.vlan_id.is_some_and(|v| !(1..=4094).contains(&v))
            {
                return Err(
                    "Use a name and a VLAN ID from 1 to 4094, or leave the tag empty.".into(),
                );
            }
            if let Ipv4::Static {
                address,
                prefix,
                gateway,
                dns,
            } = &n.ipv4
            {
                if !(8..=30).contains(prefix) || dns.len() > 3 {
                    return Err(
                        "Use an IPv4 prefix from 8 to 30 and at most three DNS servers.".into(),
                    );
                }
                let mask = u32::MAX << (32 - prefix);
                let host = u32::from(*address);
                let start = host & mask;
                let end = start | !mask;
                if !unicast(*address)
                    || host == start
                    || host == end
                    || gateway.is_some_and(|g| {
                        !unicast(g)
                            || u32::from(g) & mask != start
                            || g == *address
                            || u32::from(g) == start
                            || u32::from(g) == end
                    })
                    || dns.iter().any(|d| !unicast(*d))
                {
                    return Err("Use valid host, router and DNS IPv4 addresses.".into());
                }
                if n.id != self.management && (gateway.is_some() || !dns.is_empty()) {
                    return Err(
                        "Default gateway and DNS belong to the management network only.".into(),
                    );
                }
                if subnets.iter().any(|(a, b)| start <= *b && *a <= end) {
                    return Err("The configured IPv4 subnets overlap.".into());
                }
                subnets.push((start, end));
            }
        }
        Ok(())
    }
}

fn unicast(ip: Ipv4Addr) -> bool {
    !ip.is_unspecified()
        && !ip.is_loopback()
        && !ip.is_multicast()
        && !ip.is_broadcast()
        && ip.octets()[0] < 224
        && ip.octets()[0] != 0
}

/// Helper request, framed as a single size-limited JSON line on a Unix socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Required protocol version; mismatches are refused before any action.
    pub api: u32,
    /// Typed host operation.
    pub command: Command,
}

/// Mutations require a complete validated profile or a pending transaction token.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    /// Current profiles and pending transaction, without its secret token.
    Status {},
    /// Validate without changing anything.
    Preview { profiles: Profiles },
    /// Journal and schedule a protected change.
    Apply { profiles: Profiles },
    /// Inspect only the matching pending change after a management move.
    Pending { token: String },
    /// Keep a ready change, from the actual new management socket address.
    Confirm {
        token: String,
        local_address: Ipv4Addr,
    },
    /// Restore the previous configuration.
    Revert { token: String },
}

/// Public pending information. Tokens are returned only to the apply initiator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    /// applying, ready or recovery_failed.
    pub phase: String,
    /// Seconds remaining on the device-side monotonic timer.
    pub seconds: u64,
    /// Configured static address, or the actual acquired DHCP address when ready.
    pub management_address: Option<Ipv4Addr>,
    /// Direct portal port, even when the original page used a reverse proxy.
    pub management_port: u16,
    /// Latest activation/recovery error.
    pub error: Option<String>,
}

/// Versioned helper response. Unsupported hosts remain readable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    /// Protocol version.
    pub api: u32,
    /// NDIMon owns the host networking; no standalone mutations are allowed.
    pub externally_managed: bool,
    /// Current committed or initially adopted profiles.
    pub profiles: Option<Profiles>,
    /// Pending activation/recovery state.
    pub pending: Option<Pending>,
    /// Secret, only in a successful Apply response.
    pub token: Option<String>,
    /// Refusal or unsupported-host explanation.
    pub error: Option<String>,
}
