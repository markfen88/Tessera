//! Backend lifecycle boundary. Adapters share a private immutable plan and the
//! host's relocatable filesystem/runner; transaction decisions belong to Engine.

use crate::host::{Host, Plan, parse_netplan_addressing, parse_networkd_addressing};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tessera_state::network_profiles::{
    Backend, ConfirmationError, Ipv4, MAX_DNS_SEARCH, Profiles, valid_dns_search_domain,
};

pub type Files = BTreeMap<String, Option<String>>;
pub type Result<T> = std::result::Result<T, Failure>;

/// Structured failure at the privileged adoption/planning boundary.
#[derive(Debug)]
pub struct Failure {
    pub kind: Option<ConfirmationError>,
    pub message: String,
}
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self {
            kind: None,
            message,
        }
    }
}
impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
pub fn unsupported(what: &str) -> Failure {
    Failure {
        kind: Some(ConfirmationError::Unsupported { what: what.into() }),
        message: format!("Network configuration refused: {what}."),
    }
}

/// A refusal whose atom is unchanged and whose message says what to do about it.
pub fn unsupported_with(what: &str, remedy: &str) -> Failure {
    let mut failure = unsupported(what);
    failure.message = format!("Network configuration refused: {what}. {remedy}");
    failure
}

/// `unrepresentable_configuration` whose detail names where the intent was
/// read and which key held it (NE-2.12a), without echoing its value.
pub fn unrepresentable(source: &str, key: &str) -> Failure {
    let mut failure = unsupported("unrepresentable_configuration");
    failure.message =
        format!("Network configuration refused: unrepresentable_configuration ({source}: {key}).");
    failure
}

/// What adoption reads from the management definition: the addressing plus the
/// configured (never lease-provided) DNS search domains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Addressing {
    pub ipv4: Ipv4,
    pub dns_search: Vec<String>,
}

/// NE-2.12a adoption bounds: more than six entries or an entry that is not a
/// plain host name (for example a `~` routing domain) cannot round-trip.
pub fn adopt_search(domains: &[&str], source: &str, key: &str) -> Result<Vec<String>> {
    if domains.len() > MAX_DNS_SEARCH || !domains.iter().all(|d| valid_dns_search_domain(d)) {
        return Err(unrepresentable(source, key));
    }
    Ok(domains.iter().map(|d| (*d).to_owned()).collect())
}

/// Backend intent captured before mutation, separate from the API-1 document.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub metric: u32,
    pub metrics: BTreeMap<String, u32>,
    #[serde(default)]
    pub pins: BTreeMap<String, String>,
    pub selected: BTreeMap<String, String>,
    #[serde(default)]
    pub connections: Vec<crate::networkmanager::Change>,
}
impl Metadata {
    pub fn metric(&self, interface: &str) -> u32 {
        self.metrics.get(interface).copied().unwrap_or(self.metric)
    }
}

/// Complete OS lifecycle, exercised through the same boundary in fixtures.
pub trait Adapter: std::fmt::Debug + Send + Sync {
    fn detect(&self, host: &Host, interface: &str) -> Result<bool>;
    fn adopt(&self, host: &Host, interface: &str) -> Result<Addressing>;
    fn snapshot(&self, host: &Host, previous: &Profiles, candidate: &Profiles) -> Result<Metadata>;
    fn render(
        &self,
        host: &Host,
        previous: &Profiles,
        candidate: &Profiles,
        metadata: &Metadata,
    ) -> Result<Files>;
    fn recheck(&self, host: &Host, plan: &Plan) -> crate::Result<()> {
        let current = self
            .snapshot(host, &plan.previous, &plan.candidate)
            .map_err(|e| e.to_string())?;
        if current.selected != plan.metadata.selected || current.metrics != plan.metadata.metrics {
            return Err("Backend intent changed after staging".into());
        }
        Ok(())
    }
    fn prepare(&self, _host: &Host, _plan: &Plan) -> crate::Result<()> {
        Ok(())
    }
    /// Whether `found` is the content `saved` of a recorded file, as far as this backend's
    /// own tools leave it alone. Byte equality, unless the tools rewrite files by themselves.
    fn same_file(&self, _path: &str, saved: Option<&str>, found: Option<&str>) -> bool {
        saved == found
    }
    fn activate(&self, host: &Host, plan: &Plan, restore: bool) -> crate::Result<()>;
    fn verify(&self, host: &Host, plan: &Plan, restore: bool) -> crate::Result<()>;
    fn restore(&self, host: &Host, plan: &Plan) -> crate::Result<()> {
        host.write_files(plan, true)?;
        self.activate(host, plan, true)
    }
    fn commit(&self, _host: &Host, _plan: &Plan) -> crate::Result<()> {
        Ok(())
    }
}
#[derive(Debug)]
struct Networkd;
#[derive(Debug)]
struct Netplan;
pub fn adapter(backend: Backend) -> &'static dyn Adapter {
    match backend {
        Backend::Networkd => &Networkd,
        Backend::Netplan => &Netplan,
        Backend::NetworkManager => &crate::networkmanager::NetworkManager,
        Backend::Ifupdown => &crate::ifupdown::Ifupdown,
    }
}
fn activate_networkd(host: &Host, p: &Profiles) -> crate::Result<()> {
    host.run("/usr/bin/networkctl", &["reload"])?;
    let mut args = vec!["reconfigure", "--", &p.interface];
    let names: Vec<_> = p
        .networks
        .iter()
        .filter(|n| n.vlan_id.is_some())
        .map(|n| p.ifname(n))
        .collect();
    args.extend(names.iter().map(String::as_str));
    host.run("/usr/bin/networkctl", &args)?;
    Ok(())
}
impl Adapter for Networkd {
    fn detect(&self, host: &Host, interface: &str) -> Result<bool> {
        Ok(!host
            .active_network_file(interface)?
            .starts_with("/run/systemd/network/10-netplan-"))
    }
    fn adopt(&self, host: &Host, interface: &str) -> Result<Addressing> {
        let path = host.active_network_file(interface)?;
        parse_networkd_addressing(&host.effective_networkd(&path)?, &path)
    }
    fn snapshot(
        &self,
        host: &Host,
        previous: &Profiles,
        _candidate: &Profiles,
    ) -> Result<Metadata> {
        host.networkd_metadata(previous, false)
    }
    fn render(
        &self,
        host: &Host,
        previous: &Profiles,
        candidate: &Profiles,
        metadata: &Metadata,
    ) -> Result<Files> {
        host.networkd_files(previous, candidate, metadata, true)
    }
    fn activate(&self, host: &Host, plan: &Plan, restore: bool) -> crate::Result<()> {
        activate_networkd(
            host,
            if restore {
                &plan.previous
            } else {
                &plan.candidate
            },
        )
    }
    fn verify(&self, host: &Host, plan: &Plan, restore: bool) -> crate::Result<()> {
        host.verify_networkd(plan, restore)
    }
}
impl Adapter for Netplan {
    fn detect(&self, host: &Host, interface: &str) -> Result<bool> {
        Ok(host
            .active_network_file(interface)?
            .starts_with("/run/systemd/network/10-netplan-"))
    }
    fn adopt(&self, host: &Host, interface: &str) -> Result<Addressing> {
        let (id, entry) = host.netplan_entry(interface)?;
        parse_netplan_addressing(&entry, &format!("netplan network.ethernets.{id}"))
    }
    fn snapshot(
        &self,
        host: &Host,
        previous: &Profiles,
        _candidate: &Profiles,
    ) -> Result<Metadata> {
        host.networkd_metadata(previous, true)
    }
    fn render(
        &self,
        host: &Host,
        previous: &Profiles,
        candidate: &Profiles,
        metadata: &Metadata,
    ) -> Result<Files> {
        let mut files = host.netplan_files(previous, candidate, metadata)?;
        files.extend(host.networkd_files(previous, candidate, metadata, false)?);
        Ok(files)
    }
    fn activate(&self, host: &Host, plan: &Plan, restore: bool) -> crate::Result<()> {
        host.run("/usr/sbin/netplan", &["generate"])?;
        host.run("/usr/sbin/netplan", &["apply"])?;
        activate_networkd(
            host,
            if restore {
                &plan.previous
            } else {
                &plan.candidate
            },
        )
    }
    fn verify(&self, host: &Host, plan: &Plan, restore: bool) -> crate::Result<()> {
        host.verify_networkd(plan, restore)
    }
}
