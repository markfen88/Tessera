//! Standalone host networking contract. Separate from the audio daemon's
//! runtime interface selection: a privileged helper owns OS configuration.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr};

use serde::{Deserialize, Serialize};

/// Increment when the host-networking wire contract becomes incompatible.
pub const NETWORK_API: u32 = 1;
/// Semantic documents use decimal-string revisions; API 1 remains unchanged.
pub const NETWORK_DOCUMENT_API: u32 = 2;
/// Largest revision that API 1 can pass through a JavaScript number losslessly.
pub const LEGACY_MAX_REVISION: u64 = 9_007_199_254_740_991;
/// An explicit keep is required this many seconds after activation succeeds.
pub const CONFIRM_SECONDS: u64 = 60;
/// Give the server-rendered confirmation page time to load before switching.
pub const APPLY_DELAY_SECONDS: u64 = 3;
/// Initial guard includes the bounded apply job and the full keep window.
pub const GUARD_SECONDS: u64 = 180;
/// Most DNS search domains a network row can carry (NE-2.12a).
pub const MAX_DNS_SEARCH: usize = 6;

/// Host-controlled confirmation policy; the transaction token is always required.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ConfirmPolicy {
    /// Verify the actual HTTP destination is the new management address.
    #[default]
    RequireManagementAddress,
    /// The pending transaction link can be used from any address.
    TokenOnly,
}

/// Administrator-owned `[network]` settings in web.toml, enforced by the helper.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkPolicy {
    /// How the transaction token and HTTP destination authorize Keep.
    pub confirm: ConfirmPolicy,
    /// Explicit wired port for first adoption when default routes are ambiguous.
    pub interface: Option<String>,
}

/// Machine-readable confirmation refusals (counterpart contract §3.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ConfirmationError {
    /// The candidate was based on a different committed revision.
    RevisionConflict { current_revision: String },
    /// An unsupported backend or adoption intent, identified without prose parsing.
    Unsupported { what: String },
    /// Another owner or protected path prevents this mutation.
    Locked { field: String, by: String },
    /// A classified backend or recovery failure.
    ApplyFailed {
        domain: String,
        step: String,
        reason: String,
    },
    /// No change is waiting for confirmation.
    NotPending,
    /// The confirmation window has passed.
    Expired,
    /// Activation or recovery is still in progress.
    StillApplying,
    /// The supplied token does not authorize the pending change.
    TokenMismatch,
    /// Confirmation must arrive on the new management address.
    WrongAddress,
    /// Too many token mismatches from this client.
    RateLimited,
    /// The transaction token is not 64 lowercase hexadecimal characters.
    Invalid {
        /// Invalid input field.
        field: String,
        /// Human-readable validation reason.
        reason: String,
    },
}

/// Supported persistent configuration backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// systemd-networkd's native files.
    Networkd,
    /// Netplan with its networkd renderer.
    Netplan,
    /// NetworkManager, including Netplan's NetworkManager renderer.
    #[serde(rename = "networkmanager", alias = "network_manager")]
    NetworkManager,
    /// Administrator-handed-off ifupdown drop-ins.
    Ifupdown,
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
    /// DNS search domains (management only, NE-2.12a). Valid for DHCP and
    /// static addressing; omitted from documents while empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dns_search: Vec<String>,
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

/// Effective administrator policy captured with a semantic candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Policies {
    pub confirm: ConfirmPolicy,
}

/// API-2 representation of NE-2.1. The schema remains version 1, while the
/// enclosing IPC version makes the lossless revision encoding explicit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Document {
    pub version: u32,
    #[serde(with = "decimal_revision")]
    pub revision: u64,
    pub interface: String,
    pub backend: Backend,
    pub networks: Vec<Network>,
    pub management: String,
    pub audio: String,
    pub policies: Policies,
}

impl Document {
    /// Explicit adapter for existing API-1 and private journal documents.
    #[must_use]
    pub fn from_profiles(profiles: Profiles, confirm: ConfirmPolicy) -> Self {
        Self {
            version: profiles.version,
            revision: profiles.revision,
            interface: profiles.interface,
            backend: profiles.backend,
            networks: profiles.networks,
            management: profiles.management,
            audio: profiles.audio,
            policies: Policies { confirm },
        }
    }

    /// Keep the engine's full u64 and the legacy document's existing shape.
    #[must_use]
    pub fn into_profiles(self) -> Profiles {
        Profiles {
            version: self.version,
            revision: self.revision,
            interface: self.interface,
            backend: self.backend,
            networks: self.networks,
            management: self.management,
            audio: self.audio,
        }
    }
}

mod decimal_revision {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.is_empty()
            || (value.len() > 1 && value.starts_with('0'))
            || !value.bytes().all(|c| c.is_ascii_digit())
        {
            return Err(D::Error::custom(
                "revision must be a canonical unsigned decimal string",
            ));
        }
        value.parse().map_err(D::Error::custom)
    }
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

/// NE-2.12a: a host name of 1..253 ASCII characters made of dot-separated
/// labels of 1..63 letters, digits and hyphens that neither start nor end
/// with a hyphen. A trailing dot, `~` routing prefix or wildcard is invalid.
#[must_use]
pub fn valid_dns_search_domain(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
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
            // NE-2.6 / U4: retain existing API-1 IDs; new IDs start with a letter.
            if (!previous.networks.iter().any(|old| old.id == n.id)
                && !n.id.as_bytes().first().is_some_and(u8::is_ascii_lowercase))
                || n.id.is_empty()
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
                || n.name.chars().count() > 64
                || n.name.chars().any(char::is_control)
                || n.vlan_id.is_some_and(|v| !(1..=4094).contains(&v))
            {
                return Err(
                    "Use a name and a VLAN ID from 1 to 4094, or leave the tag empty.".into(),
                );
            }
            // NE-2.12a / U16: bounded, management-only, valid for DHCP too.
            if n.dns_search.len() > MAX_DNS_SEARCH {
                return Err("Use at most six DNS search domains.".into());
            }
            if !n.dns_search.iter().all(|d| valid_dns_search_domain(d)) {
                return Err(
                    "Use DNS search domains made of letters, digits, hyphens and dots.".into(),
                );
            }
            if n.id != self.management && !n.dns_search.is_empty() {
                return Err(
                    "Default gateway and DNS belong to the management network only.".into(),
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
        && !ip.is_link_local()
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

/// Actual portal socket endpoints, forwarded only over the protected helper IPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPath {
    pub peer: IpAddr,
    pub local: Option<IpAddr>,
}

/// Mutations require a complete validated profile or a pending transaction token.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    /// API 2: preview a semantic candidate without changing administrator policy.
    PreviewDocument {
        document: Document,
        session: Option<SessionPath>,
    },
    /// API 2: stage a complete semantic candidate and its effective policy.
    ApplyDocument {
        document: Document,
        session: Option<SessionPath>,
    },
    /// Current profiles and pending transaction, without its secret token.
    Status {},
    /// Validate without changing anything.
    Preview { profiles: Profiles },
    /// Journal and schedule a protected change.
    Apply { profiles: Profiles },
    /// Preview with trusted portal socket context (API-1 additive command).
    PreviewFrom {
        profiles: Profiles,
        session: SessionPath,
    },
    /// Stage a protected move carrying the requesting session's real path.
    ApplyFrom {
        profiles: Profiles,
        session: SessionPath,
    },
    /// Inspect only the matching pending change after a management move.
    Pending { token: String },
    /// Keep a ready change, subject to the host's confirmation policy.
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
    /// staged, applying, applied or failed (with recovery_pending).
    pub phase: String,
    /// Seconds remaining on the device-side monotonic timer.
    pub seconds: u64,
    /// Configured static address, or the actual acquired DHCP address when ready.
    pub management_address: Option<Ipv4Addr>,
    /// Direct portal port, even when the original page used a reverse proxy.
    pub management_port: u16,
    /// Latest activation/recovery error.
    pub error: Option<String>,
    #[serde(default)]
    pub recovery_pending: bool,
    #[serde(default)]
    pub recovery_error: Option<String>,
}

/// Foreign configuration evidence, without unrelated contents or secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Drift {
    pub path: String,
    pub selector: String,
    pub interface: String,
    pub foreign: bool,
    pub blocking: bool,
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
    /// API 2 only; API-1 numeric revisions are never silently re-encoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<Document>,
    /// Pending activation/recovery state.
    pub pending: Option<Pending>,
    /// Secret, only in a successful Apply response.
    pub token: Option<String>,
    /// Refusal or unsupported-host explanation.
    pub error: Option<String>,
    /// Additive typed refusal; absent in older helper responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ConfirmationError>,
    /// Effective host policy; older helpers enforce the address requirement.
    #[serde(default)]
    pub confirm_policy: ConfirmPolicy,
    /// Foreign matches observed during a read-only scan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drift: Vec<Drift>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profiles() -> Profiles {
        Profiles {
            version: NETWORK_API,
            revision: 0,
            interface: "eth0".into(),
            backend: Backend::Networkd,
            networks: vec![
                Network {
                    id: "management".into(),
                    name: "Management".into(),
                    vlan_id: None,
                    ipv4: Ipv4::Static {
                        address: Ipv4Addr::new(192, 0, 2, 10),
                        prefix: 24,
                        gateway: Some(Ipv4Addr::new(192, 0, 2, 1)),
                        dns: vec![Ipv4Addr::new(192, 0, 2, 53)],
                    },
                    dns_search: Vec::new(),
                },
                Network {
                    id: "audio".into(),
                    name: "Audio".into(),
                    vlan_id: Some(20),
                    ipv4: Ipv4::Dhcp,
                    dns_search: Vec::new(),
                },
            ],
            management: "management".into(),
            audio: "audio".into(),
        }
    }

    fn search(domains: &[&str]) -> Vec<String> {
        domains.iter().map(|d| (*d).to_owned()).collect()
    }

    #[test]
    fn search_domain_syntax_follows_host_name_labels() {
        let label = "a".repeat(63);
        let longest = format!("{label}.{label}.{label}.{}", "b".repeat(61));
        let too_long = format!("{label}.{label}.{label}.{}", "b".repeat(62));
        let long_label = "a".repeat(64);
        for valid in [
            "lan",
            "example.test",
            "Corp-1.Example.TEST",
            "a.b.c.d.e.f",
            "1.2.3",
            label.as_str(),
            longest.as_str(),
        ] {
            assert!(valid_dns_search_domain(valid), "{valid}");
        }
        assert_eq!(longest.len(), 253);
        for invalid in [
            "",
            ".",
            "example.test.",
            ".example.test",
            "a..b",
            "-a.test",
            "a-.test",
            "a.-b.test",
            "a b.test",
            "a_b.test",
            "~example.test",
            "*.example.test",
            "bücher.test",
            "a,b",
            long_label.as_str(),
            too_long.as_str(),
        ] {
            assert!(!valid_dns_search_domain(invalid), "{invalid}");
        }
    }

    #[test]
    fn management_search_domains_are_valid_for_dhcp_and_static() {
        let previous = profiles();
        let mut candidate = previous.clone();
        candidate.networks[0].dns_search = search(&["lan.example", "Example.TEST"]);
        assert_eq!(candidate.validate(&previous), Ok(()));
        candidate.networks[0].ipv4 = Ipv4::Dhcp;
        assert_eq!(candidate.validate(&previous), Ok(()));
        // Order and case are kept as entered; duplicates are not rejected.
        candidate.networks[0].dns_search = search(&["b.test", "A.test", "b.test"]);
        assert_eq!(candidate.validate(&previous), Ok(()));
        assert_eq!(
            candidate.networks[0].dns_search,
            search(&["b.test", "A.test", "b.test"])
        );
    }

    #[test]
    fn search_domains_are_bounded_to_six() {
        let previous = profiles();
        let mut candidate = previous.clone();
        candidate.networks[0].dns_search = (1..=6).map(|i| format!("d{i}.test")).collect();
        assert_eq!(candidate.validate(&previous), Ok(()));
        candidate.networks[0].dns_search.push("d7.test".into());
        assert_eq!(
            candidate.validate(&previous),
            Err("Use at most six DNS search domains.".into())
        );
    }

    #[test]
    fn an_invalid_search_domain_is_refused() {
        let previous = profiles();
        for bad in ["example.test.", "~example.test", "a b", "-x.test", ""] {
            let mut candidate = previous.clone();
            candidate.networks[0].dns_search = search(&["ok.test", bad]);
            assert!(candidate.validate(&previous).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn only_management_may_carry_search_domains() {
        let previous = profiles();
        for dhcp in [true, false] {
            let mut candidate = previous.clone();
            if !dhcp {
                candidate.networks[1].ipv4 = Ipv4::Static {
                    address: Ipv4Addr::new(198, 51, 100, 10),
                    prefix: 24,
                    gateway: None,
                    dns: Vec::new(),
                };
            }
            candidate.networks[1].dns_search = search(&["lan.example"]);
            assert_eq!(
                candidate.validate(&previous),
                Err("Default gateway and DNS belong to the management network only.".into())
            );
            // Moving management clears the old row's list in the same candidate.
            candidate.management = "audio".into();
            candidate.networks[0].ipv4 = Ipv4::Dhcp;
            assert_eq!(candidate.validate(&previous), Ok(()));
        }
    }
}
