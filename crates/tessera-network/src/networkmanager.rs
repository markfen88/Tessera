//! UUID-scoped NetworkManager takeover. Only an explicit allowlist is read or
//! changed; credentials, keyfiles and unrelated connection contents stay out
//! of the journal and diagnostics.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tessera_state::network_profiles::{Ipv4, Network, Profiles};

use crate::backend::{
    Adapter, Addressing, Failure, Files, Metadata, Result, adopt_search, unsupported,
};
use crate::host::{FileChange, Host, Plan, interfaces, parse_netplan_ipv4};

const NMCLI: &str = "/usr/bin/nmcli";
// Where a connection is persisted: Netplan's definitions and the keyfiles
// NetworkManager reads (its Netplan plugin generates the ones under /run from
// the YAML). A file belongs to a connection when it names the connection's UUID.
const PROFILE_DIRECTORIES: &[&str] = &[
    "/etc/netplan",
    "/run/netplan",
    "/etc/NetworkManager/system-connections",
    "/run/NetworkManager/system-connections",
];
// NE-4.9/4.10: every modified field has its exact before-image, including
// metric and binding. Never ask nmcli for all properties or --show-secrets.
const FIELDS: &[&str] = &[
    "connection.id",
    "connection.interface-name",
    "connection.autoconnect",
    "connection.wait-device-timeout",
    "ipv4.method",
    "ipv4.addresses",
    "ipv4.gateway",
    "ipv4.dns",
    "ipv4.dns-search",
    "ipv4.never-default",
    "ipv4.ignore-auto-dns",
    "ipv4.ignore-auto-routes",
    "ipv4.route-metric",
    "ipv4.may-fail",
    "ipv6.method",
    // NE-3.9a: not IPv6 intent, so it never refuses adoption. It is recorded because
    // NetworkManager resets it when the profile's IPv6 goes to `disabled` (the Netplan
    // plugin then saves `-1` over the `0` it had), and Revert has to put it back.
    "ipv6.ip6-privacy",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub uuid: String,
    pub name: String,
    pub interface: String,
    pub tag: Option<u16>,
    pub before: Option<BTreeMap<String, String>>,
    pub active: bool,
    pub after: BTreeMap<String, String>,
    pub removed: bool,
}

#[derive(Debug)]
pub struct NetworkManager;

fn valid_uuid(uuid: &str) -> bool {
    uuid.len() == 36
        && uuid.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
fn new_uuid() -> crate::Result<String> {
    let bytes = crate::store::token()?;
    Ok(format!(
        "{}-{}-4{}-a{}-{}",
        &bytes[..8],
        &bytes[8..12],
        &bytes[13..16],
        &bytes[17..20],
        &bytes[20..32]
    ))
}
fn field(host: &Host, uuid: &str, key: &str) -> crate::Result<String> {
    if !valid_uuid(uuid) {
        return Err("Invalid NetworkManager connection UUID".into());
    }
    host.run(
        NMCLI,
        &[
            "--escape",
            "no",
            "-g",
            key,
            "connection",
            "show",
            "uuid",
            uuid,
        ],
    )
    .map(|s| s.trim().to_owned())
}
fn uuids(host: &Host, active: bool) -> crate::Result<BTreeSet<String>> {
    let mut args = vec!["--escape", "no", "-g", "UUID", "connection", "show"];
    if active {
        args.push("--active");
    }
    let output = host.run(NMCLI, &args)?;
    output
        .lines()
        .filter(|s| !s.is_empty())
        .map(|s| {
            if valid_uuid(s) {
                Ok(s.to_owned())
            } else {
                Err("Invalid NetworkManager UUID inventory".into())
            }
        })
        .collect()
}
fn active_parent(host: &Host, interface: &str) -> Result<String> {
    let uuid = host.run(
        NMCLI,
        &[
            "--escape",
            "no",
            "-g",
            "GENERAL.CON-UUID",
            "device",
            "show",
            interface,
        ],
    )?;
    if !valid_uuid(uuid.trim()) {
        return Err(unsupported("missing_parent_connection"));
    }
    Ok(uuid.trim().to_owned())
}
fn properties(host: &Host, uuid: &str) -> crate::Result<BTreeMap<String, String>> {
    FIELDS
        .iter()
        .map(|key| Ok(((*key).to_owned(), field(host, uuid, key)?)))
        .collect()
}
fn split(value: &str) -> Vec<&str> {
    value
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|s| !s.is_empty())
        .collect()
}
fn intent(host: &Host, uuid: &str) -> Result<Addressing> {
    for key in ["ipv4.routes", "ipv4.routing-rules"] {
        if !field(host, uuid, key)?.is_empty() {
            return Err(unsupported(if key.ends_with("rules") {
                "policy_routing"
            } else {
                "custom_routes"
            }));
        }
    }
    if !matches!(
        field(host, uuid, "ipv4.route-table")?.as_str(),
        "0" | "0 (unspec)" | ""
    ) {
        return Err(unsupported("unrepresentable_configuration"));
    }
    for key in [
        "ipv6.addresses",
        "ipv6.gateway",
        "ipv6.dns",
        "ipv6.routes",
        "ipv6.routing-rules",
    ] {
        if !field(host, uuid, key)?.is_empty() {
            return Err(unsupported("ipv6_configuration"));
        }
    }
    if !matches!(
        field(host, uuid, "ipv6.method")?.as_str(),
        "auto" | "disabled" | "ignore" | "link-local"
    ) {
        return Err(unsupported("ipv6_configuration"));
    }
    let method = field(host, uuid, "ipv4.method")?;
    if !matches!(method.as_str(), "auto" | "manual") {
        return Err(unsupported("unrepresentable_configuration"));
    }
    if method == "auto" {
        for key in [
            "ipv4.ignore-auto-dns",
            "ipv4.ignore-auto-routes",
            "ipv4.never-default",
        ] {
            if field(host, uuid, key)? != "no" {
                return Err(unsupported("unrepresentable_configuration"));
            }
        }
    }
    let addresses = field(host, uuid, "ipv4.addresses")?;
    let dns = field(host, uuid, "ipv4.dns")?;
    let gateway = field(host, uuid, "ipv4.gateway")?;
    // NE-2.12a: the configured property, never the lease's IP4.DOMAIN values.
    let search = field(host, uuid, "ipv4.dns-search")?;
    Ok(Addressing {
        ipv4: parse_netplan_ipv4(
            &json!({"dhcp4":method == "auto", "addresses":split(&addresses),
            "gateway4":gateway, "nameservers":{"addresses":split(&dns)}}),
        )?,
        dns_search: adopt_search(
            &split(&search),
            &format!("NetworkManager connection {uuid}"),
            "ipv4.dns-search",
        )?,
    })
}
/// NetworkManager's own default metric for an Ethernet port, which `ipv4.route-metric = -1`
/// stands for (nm-settings-nmcli(5)).
const NM_ETHERNET_METRIC: u32 = 100;

/// The refusal of a metric adoption cannot take: `unrepresentable_configuration` naming the
/// connection and the key (NE-4.4: when the effective metric cannot be established safely).
fn metric_refusal(host: &Host, uuid: &str, detail: &str) -> Failure {
    let name = field(host, uuid, "connection.id").map_or_else(|_| uuid.to_owned(), |id| label(&id));
    let mut failure = unsupported("unrepresentable_configuration");
    failure.message = format!(
        "Network configuration refused: unrepresentable_configuration (NetworkManager connection '{name}': ipv4.route-metric {detail})."
    );
    failure
}

/// The metric of the connection's default route. A number is the profile's own choice. `-1`
/// leaves it to NetworkManager, whose default for an Ethernet port is 100: the value every
/// stock Raspberry Pi OS and Netplan-generated profile has (NE-3.9b, U18). The port's
/// connection is the one active on it, so the default route NetworkManager really installed
/// has to agree: a connection default set in `conf.d` is refused by name, not guessed (NE-4.4).
/// A VLAN connection is the helper's own, with its metric written out.
fn metric(host: &Host, uuid: &str, interface: &str, vlan: bool) -> Result<u32> {
    let value = field(host, uuid, "ipv4.route-metric")?;
    if value != "-1" {
        return value
            .parse()
            .map_err(|_| metric_refusal(host, uuid, "is not a number or -1"));
    }
    if vlan {
        return Ok(400);
    }
    let routes: Value =
        serde_json::from_str(&host.run("/usr/sbin/ip", &["-j", "-4", "route", "show", "default"])?)
            .map_err(|e| e.to_string())?;
    for route in routes
        .as_array()
        .into_iter()
        .flatten()
        .filter(|route| route["dev"] == interface)
    {
        let installed = route["metric"].as_u64().unwrap_or(0);
        if installed != u64::from(NM_ETHERNET_METRIC) {
            return Err(metric_refusal(
                host,
                uuid,
                &format!(
                    "is -1 but the default route of {interface} has metric {installed}, not NetworkManager's Ethernet default {NM_ETHERNET_METRIC}"
                ),
            ));
        }
    }
    Ok(NM_ETHERNET_METRIC)
}

/// Does a profile's text hold a credential? The journal keeps the before-image
/// of the files it restores, so such a profile is refused rather than copied
/// (the Netplan backend refuses the same way). `*-flags` keys only say where a
/// secret is kept.
fn holds_secret(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        if line.starts_with('[') {
            return matches!(
                line,
                "[802-1x]" | "[wifi-security]" | "[vpn]" | "[wireguard]"
            );
        }
        let Some((key, value)) = line.split_once(['=', ':']) else {
            return false;
        };
        let key = key.trim().trim_matches(['"', '\'']).to_ascii_lowercase();
        let value = value.trim().trim_matches(['"', '\'']);
        matches!(key.as_str(), "auth" | "wifis" | "access-points")
            || (!value.is_empty()
                && !key.ends_with("-flags")
                && ["password", "psk", "secret", "private-key", "wep-key"]
                    .iter()
                    .any(|word| key.contains(word)))
    })
}

/// The files that persist one connection, as they are now, and whether
/// Netplan stores it.
struct Persisted {
    files: Files,
    netplan: bool,
}

/// A file name that can hold a profile.
fn profile_name(name: &str) -> bool {
    name.ends_with(".yaml") || name.ends_with(".yml") || name.ends_with(".nmconnection")
}

fn persisted(host: &Host, uuid: &str) -> Result<Persisted> {
    let needle = uuid.to_ascii_lowercase();
    let mut files = Files::new();
    let mut netplan = false;
    // The Netplan definitions that generated a keyfile for this UUID.
    let mut definitions = Vec::new();
    for directory in PROFILE_DIRECTORIES {
        for name in host.entries(directory)? {
            if !profile_name(&name) {
                continue;
            }
            let path = format!("{directory}/{name}");
            // A file that cannot be read safely (wrong owner, writable by
            // others, too large) is not a profile this helper may rely on.
            let Ok(Some(text)) = host.read(&path) else {
                continue;
            };
            if !text.to_ascii_lowercase().contains(&needle) {
                continue;
            }
            if holds_secret(&text) {
                return Err(unsupported("unsafe_configuration"));
            }
            netplan |= directory.ends_with("netplan") || name.starts_with("netplan-");
            if let Some(id) = name
                .strip_prefix("netplan-")
                .and_then(|rest| rest.strip_suffix(".nmconnection"))
            {
                definitions.push(id.to_owned());
            }
            files.insert(path, Some(text));
        }
    }
    // NetworkManager writes a changed Netplan profile back to the file that
    // defines it, which need not name the UUID (a hand-written definition).
    // Such a file is recorded when it holds no credential; one that does is
    // left alone, like the other files of the host.
    for directory in ["/etc/netplan", "/run/netplan"] {
        if definitions.is_empty() {
            break;
        }
        for name in host.entries(directory)? {
            let path = format!("{directory}/{name}");
            if files.contains_key(&path) || !profile_name(&name) {
                continue;
            }
            let Ok(Some(text)) = host.read(&path) else {
                continue;
            };
            let defines = crate::yaml::parse(&text).is_ok_and(|doc| {
                definitions
                    .iter()
                    .any(|id| doc["network"]["ethernets"].get(id).is_some())
            });
            if defines && !holds_secret(&text) {
                files.insert(path, Some(text));
            }
        }
    }
    // NetworkManager's Netplan plugin saves a changed profile as this file;
    // recording its absence lets a failed apply remove what the apply made.
    let saved = format!("/etc/netplan/90-NM-{uuid}.yaml");
    if netplan && !files.contains_key(&saved) && host.path("/etc/netplan").is_dir() {
        files.insert(saved, None);
    }
    Ok(Persisted { files, netplan })
}

/// Whether a path is directly in one of the directories that hold connection profiles.
fn in_profile_directory(path: &str) -> bool {
    PROFILE_DIRECTORIES.iter().any(|directory| {
        path.strip_prefix(directory)
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|name| !name.contains('/'))
    })
}

/// The files of a connection among a plan's recorded files.
fn profile_files(plan: &Plan) -> Vec<&FileChange> {
    plan.files
        .iter()
        .filter(|file| in_profile_directory(&file.path))
        .collect()
}

/// A saved profile without the lines NetworkManager's writers keep adding or refreshing by
/// themselves (HW-3, D14). After `nmcli connection reload` its Netplan writer puts
/// `ipv6.ip6-privacy: "-1"` (the default level) into the passthrough keys of a profile that
/// `nmcli connection modify` had saved without it, within a second and every time, and every
/// activation refreshes `connection.timestamp`. Neither is configuration, and neither can be
/// kept out of the file, so a restore is judged by the rest, byte for byte: a comment, an
/// operator's edit or any other key is still a difference. A keyfile's `#Netplan:` marker
/// goes with the line it marks.
fn without_noise(path: &str, text: &str) -> String {
    let yaml = path.ends_with(".yaml") || path.ends_with(".yml");
    let mut section = "";
    let mut kept: Vec<&str> = Vec::new();
    for line in text.lines() {
        let clean = line.trim();
        if !yaml && clean.starts_with('[') {
            section = clean;
        }
        let noise = if yaml {
            matches!(
                clean,
                "ipv6.ip6-privacy: \"-1\"" | "ipv6.ip6-privacy: '-1'" | "ipv6.ip6-privacy: -1"
            ) || clean.starts_with("connection.timestamp:")
        } else {
            (section == "[ipv6]" && clean == "ip6-privacy=-1")
                || (section == "[connection]" && clean.starts_with("timestamp="))
        };
        if noise {
            if kept
                .last()
                .is_some_and(|last| last.trim_start().starts_with("#Netplan:"))
            {
                kept.pop();
            }
            continue;
        }
        kept.push(line);
    }
    let mut out = kept.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Whether `found` is the profile `saved`: the same bytes, or the same text apart from what
/// NetworkManager rewrites by itself (`without_noise`).
fn same_profile(path: &str, saved: Option<&str>, found: Option<&str>) -> bool {
    match (saved, found) {
        (None, None) => true,
        (Some(saved), Some(found)) => {
            saved == found || without_noise(path, saved) == without_noise(path, found)
        }
        _ => false,
    }
}

/// A parent profile that Netplan stores and that is bound to no device. The
/// plugin writes it as `match: {}` and keeps no `connection.interface-name`
/// (it acknowledges the property and reads it back empty), so the profile
/// cannot represent the binding and the device's active connection is the
/// proof that it applies to the port.
fn unbound_netplan_parent(
    host: &Host,
    uuid: &str,
    before: &BTreeMap<String, String>,
) -> Result<bool> {
    Ok(before
        .get("connection.interface-name")
        .is_some_and(String::is_empty)
        && persisted(host, uuid)?.netplan)
}

/// A connection name as shown in an error: no control characters.
fn label(name: &str) -> String {
    name.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// Identity sidecar is written only after the sole committed.json commit point;
/// pending recovery finishes it before discarding the journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ownership {
    pub profiles: Profiles,
    pub connections: BTreeMap<String, String>,
}
fn ownership(host: &Host, p: &Profiles, exact: bool) -> Result<BTreeMap<String, String>> {
    let Some(text) = host.read("/var/lib/tessera-network/backend.json")? else {
        return Ok(BTreeMap::new());
    };
    let owned: Ownership =
        serde_json::from_str(&text).map_err(|_| unsupported("unsafe_configuration"))?;
    if owned.profiles.interface != p.interface
        || owned.profiles.backend != p.backend
        || owned.profiles.revision != p.revision
        || (exact && owned.profiles != *p)
    {
        return Err(unsupported("foreign_connection"));
    }
    Ok(owned.connections)
}
pub fn committed_ownership(plan: &Plan, committed: Profiles) -> Ownership {
    Ownership {
        profiles: committed,
        connections: plan
            .metadata
            .connections
            .iter()
            .filter(|c| !c.removed)
            .map(|c| (c.interface.clone(), c.uuid.clone()))
            .collect(),
    }
}

#[derive(Debug)]
struct Connection {
    uuid: String,
    name: String,
    kind: String,
    interface: String,
    mac: String,
    autoconnect: bool,
    parent: String,
    tag: Option<u16>,
}
fn inventory(host: &Host) -> Result<Vec<Connection>> {
    let mut connections = Vec::new();
    for uuid in uuids(host, false)? {
        let kind = field(host, &uuid, "connection.type")?;
        let (parent, tag) = if kind == "vlan" {
            (
                field(host, &uuid, "vlan.parent")?,
                Some(
                    field(host, &uuid, "vlan.id")?
                        .parse()
                        .map_err(|_| unsupported("foreign_vlan"))?,
                ),
            )
        } else {
            (String::new(), None)
        };
        let autoconnect = match field(host, &uuid, "connection.autoconnect")?.as_str() {
            "yes" => true,
            "no" => false,
            _ => return Err(unsupported("foreign_connection")),
        };
        connections.push(Connection {
            name: field(host, &uuid, "connection.id")?,
            interface: field(host, &uuid, "connection.interface-name")?,
            mac: if matches!(kind.as_str(), "802-3-ethernet" | "vlan") {
                field(host, &uuid, "802-3-ethernet.mac-address")?
            } else {
                String::new()
            },
            uuid,
            kind,
            autoconnect,
            parent,
            tag,
        });
    }
    Ok(connections)
}
fn check_parent(host: &Host, interface: &str, uuid: &str) -> Result<()> {
    if field(host, uuid, "connection.type")? != "802-3-ethernet" {
        return Err(unsupported("wired_port_required"));
    }
    let binding = field(host, uuid, "connection.interface-name")?;
    if !binding.is_empty() && binding != interface {
        return Err(unsupported("foreign_connection"));
    }
    let controller = field(host, uuid, "connection.master")?;
    if !controller.is_empty() {
        return Err(unsupported("attached_interfaces"));
    }
    Ok(())
}
fn eligible(connection: &Connection, p: &Profiles, parent: &str, mac: &str) -> bool {
    interfaces(p).contains(&connection.interface)
        || (connection.kind == "802-3-ethernet"
            && connection.interface.is_empty()
            && (connection.mac.is_empty() || connection.mac.eq_ignore_ascii_case(mac.trim())))
        || (connection.kind == "vlan"
            && (connection.parent == p.interface
                || connection.parent == parent
                || (connection.parent.is_empty()
                    && (connection.mac.is_empty()
                        || connection.mac.eq_ignore_ascii_case(mac.trim())))))
}
fn validate_inventory(
    host: &Host,
    previous: &Profiles,
    candidate: &Profiles,
    parent: &str,
) -> Result<Vec<Connection>> {
    let connections = inventory(host)?;
    let ownership = ownership(host, previous, true)?;
    if ownership
        .get(&previous.interface)
        .is_some_and(|uuid| uuid != parent)
    {
        return Err(unsupported("foreign_connection"));
    }
    let mac = std::fs::read_to_string(
        host.path(&format!("/sys/class/net/{}/address", previous.interface)),
    )
    .map_err(|_| unsupported("wired_port_required"))?;
    for connection in &connections {
        if connection.uuid == parent {
            continue;
        }
        let owned = ownership.get(&connection.interface) == Some(&connection.uuid)
            && previous.networks.iter().any(|n| {
                n.vlan_id.is_some()
                    && connection.name == format!("tessera-vlan-{}", previous.ifname(n))
                    && connection.interface == previous.ifname(n)
                    && connection.kind == "vlan"
                    && connection.tag == n.vlan_id
                    && (connection.parent == previous.interface || connection.parent == parent)
            });
        if owned {
            continue;
        }
        if candidate.networks.iter().any(|n| {
            n.vlan_id.is_some()
                && connection.name == format!("tessera-vlan-{}", candidate.ifname(n))
        }) {
            return Err(unsupported("foreign_connection"));
        }
        if eligible(connection, candidate, parent, &mac)
            || eligible(connection, previous, parent, &mac)
        {
            if connection.kind == "vlan" {
                return Err(unsupported("foreign_vlan"));
            }
            if connection.autoconnect || !connection.interface.is_empty() {
                return Err(unsupported("foreign_connection"));
            }
        }
    }
    Ok(connections)
}
fn settings(
    p: &Profiles,
    n: Option<&Network>,
    metric: u32,
    interface: &str,
) -> BTreeMap<String, String> {
    let management = n.is_some_and(|n| n.id == p.management);
    // NE-2.12a: set (or cleared) on every connection we modify, management only.
    let search = n
        .filter(|n| n.id == p.management)
        .map_or_else(String::new, |n| n.dns_search.join(","));
    let (method, address, gateway, dns) = match n.map(|n| &n.ipv4) {
        Some(Ipv4::Dhcp) => ("auto", String::new(), String::new(), String::new()),
        Some(Ipv4::Static {
            address,
            prefix,
            gateway,
            dns,
        }) => (
            "manual",
            format!("{address}/{prefix}"),
            gateway.map_or_else(String::new, |g| g.to_string()),
            dns.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        None => ("disabled", String::new(), String::new(), String::new()),
    };
    [
        ("connection.interface-name", interface.to_owned()),
        ("connection.autoconnect", "yes".into()),
        ("connection.wait-device-timeout", "0".into()),
        ("ipv4.method", method.into()),
        ("ipv4.addresses", address),
        ("ipv4.gateway", gateway),
        ("ipv4.dns", dns),
        ("ipv4.dns-search", search),
        (
            "ipv4.never-default",
            if management { "no" } else { "yes" }.into(),
        ),
        (
            "ipv4.ignore-auto-dns",
            if management { "no" } else { "yes" }.into(),
        ),
        (
            "ipv4.ignore-auto-routes",
            if management { "no" } else { "yes" }.into(),
        ),
        ("ipv4.route-metric", metric.to_string()),
        ("ipv4.may-fail", "yes".into()),
        ("ipv6.method", "disabled".into()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect()
}
/// What `nmcli` accepts back for a value it printed. It prints an enumeration with its name
/// (`0 (disabled)`, `-1 (unknown)`, `2 (enabled, prefer temporary IP)`) and takes the number
/// alone; the names it prints for `ipv6.ip6-privacy` are mapped as well, in case a version
/// prints only the name.
fn nmcli_value<'a>(key: &str, value: &'a str) -> &'a str {
    if key != "ipv6.ip6-privacy" {
        return value;
    }
    let word = value.split_whitespace().next().unwrap_or("");
    match word {
        "unknown" => "-1",
        "disabled" => "0",
        "prefer-public-addr" => "1",
        "prefer-temp-addr" => "2",
        _ => word,
    }
}
fn modify(host: &Host, uuid: &str, values: &BTreeMap<String, String>) -> crate::Result<()> {
    let mut args = vec!["connection", "modify", "uuid", uuid];
    for (key, value) in values {
        args.extend([key.as_str(), nmcli_value(key, value)]);
    }
    host.run(NMCLI, &args)?;
    Ok(())
}
fn down(host: &Host, uuid: &str) -> crate::Result<()> {
    if uuids(host, true)?.contains(uuid) {
        host.run(NMCLI, &["connection", "down", "uuid", uuid])?;
    }
    Ok(())
}
fn up(host: &Host, uuid: &str, asynchronous: bool) -> crate::Result<()> {
    if asynchronous {
        // NE-6.8/6.12: optional or restored media DHCP cannot hold management
        // hostage. nmcli acknowledges activation; readiness stays in Engine.
        host.run(NMCLI, &["--wait", "0", "connection", "up", "uuid", uuid])?;
    } else {
        host.run(NMCLI, &["connection", "up", "uuid", uuid])?;
    }
    Ok(())
}

/// A property value as shown in an error: no control characters, bounded.
fn shown(value: Option<&String>) -> String {
    label(value.map_or("", String::as_str))
}

/// Everything a restore promises: every connection has the properties and the activation
/// it had before the change, a connection the change made is gone, and the files that
/// persist the profile are the saved ones, apart from what NetworkManager rewrites by
/// itself (`same_profile`). Runs inside `Host::settle`, so the error is that of the last poll.
fn verify_restored(host: &Host, plan: &Plan) -> crate::Result<()> {
    for change in &plan.metadata.connections {
        let Some(before) = &change.before else {
            if uuids(host, false)?.contains(&change.uuid) {
                return Err("verify_backend: created_connection_remains".into());
            }
            continue;
        };
        let actual = properties(host, &change.uuid)?;
        if actual != *before {
            let differing: Vec<_> = before
                .iter()
                .filter(|(key, value)| actual.get(*key) != Some(*value))
                .map(|(key, value)| {
                    format!(
                        "{key} is '{}', was '{}'",
                        shown(actual.get(key)),
                        shown(Some(value))
                    )
                })
                .collect();
            return Err(format!(
                "verify_backend: connection_restore_mismatch (connection '{}': {})",
                label(&change.name),
                differing.join(", ")
            ));
        }
        let active = uuids(host, true)?.contains(&change.uuid);
        if active != change.active {
            let state = |active: bool| if active { "active" } else { "inactive" };
            return Err(format!(
                "verify_backend: connection_activation_mismatch (connection '{}' is {}, was {})",
                label(&change.name),
                state(active),
                state(change.active)
            ));
        }
    }
    if plan.handoff.is_none() {
        for file in profile_files(plan) {
            if !same_profile(
                &file.path,
                host.read(&file.path)?.as_deref(),
                file.before.as_deref(),
            ) {
                return Err(format!(
                    "verify_backend: profile_file_restore_mismatch ({})",
                    file.path
                ));
            }
        }
    }
    Ok(())
}

impl Adapter for NetworkManager {
    fn detect(&self, host: &Host, interface: &str) -> Result<bool> {
        if !host.service_active("NetworkManager.service")? {
            return Ok(false);
        }
        let uuid = host.run(
            NMCLI,
            &[
                "--escape",
                "no",
                "-g",
                "GENERAL.CON-UUID",
                "device",
                "show",
                interface,
            ],
        )?;
        Ok(valid_uuid(uuid.trim()))
    }
    fn adopt(&self, host: &Host, interface: &str) -> Result<Addressing> {
        let uuid = active_parent(host, interface)?;
        check_parent(host, interface, &uuid)?;
        // No previous owned VLANs on first adoption (NE-3.2/A8).
        let p = Profiles {
            version: 1,
            revision: 0,
            interface: interface.into(),
            backend: tessera_state::network_profiles::Backend::NetworkManager,
            networks: Vec::new(),
            management: String::new(),
            audio: String::new(),
        };
        validate_inventory(host, &p, &p, &uuid)?;
        let addressing = intent(host, &uuid)?;
        // NE-3.9b: a metric that cannot be established refuses here, by name, and not at the
        // first Apply.
        metric(host, &uuid, interface, false)?;
        Ok(addressing)
    }
    fn snapshot(&self, host: &Host, previous: &Profiles, candidate: &Profiles) -> Result<Metadata> {
        let parent = active_parent(host, &previous.interface)?;
        check_parent(host, &previous.interface, &parent)?;
        let inventory = validate_inventory(host, previous, candidate, &parent)?;
        let ownership = ownership(host, previous, true)?;
        let active = uuids(host, true)?;
        let mut metadata = Metadata {
            metric: metric(host, &parent, &previous.interface, false)?,
            ..Metadata::default()
        };
        let mut names = interfaces(candidate);
        for name in interfaces(previous) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        for interface in names {
            let row = candidate
                .networks
                .iter()
                .find(|n| candidate.ifname(n) == interface);
            let name = if interface == candidate.interface {
                field(host, &parent, "connection.id")?
            } else {
                format!("tessera-vlan-{interface}")
            };
            let existing = if interface == candidate.interface {
                Some(parent.clone())
            } else {
                inventory
                    .iter()
                    .find(|c| ownership.get(&interface) == Some(&c.uuid))
                    .map(|c| c.uuid.clone())
            };
            let metric = match &existing {
                Some(uuid) => metric(host, uuid, &interface, interface != candidate.interface)?,
                None => 400,
            };
            metadata.metrics.insert(interface.clone(), metric);
            let before = existing
                .as_ref()
                .map(|uuid| properties(host, uuid))
                .transpose()?;
            let removed = interface != candidate.interface && row.is_none();
            let mut after = if removed {
                BTreeMap::from([("connection.autoconnect".into(), "no".into())])
            } else {
                settings(candidate, row, metric, &interface)
            };
            if let (true, Some(before)) = (interface == candidate.interface, &before) {
                if unbound_netplan_parent(host, &parent, before)? {
                    after.remove("connection.interface-name");
                }
            }
            let uuid = match existing {
                Some(uuid) => uuid,
                None => new_uuid()?,
            };
            metadata.connections.push(Change {
                active: active.contains(&uuid),
                uuid,
                name,
                interface,
                tag: row.and_then(|n| n.vlan_id),
                before,
                after,
                removed,
            });
        }
        Ok(metadata)
    }
    fn render(
        &self,
        host: &Host,
        _previous: &Profiles,
        candidate: &Profiles,
        metadata: &Metadata,
    ) -> Result<Files> {
        // Netplan/NM YAML and keyfiles are never edited here: nmcli does that.
        // The parent's files are recorded unchanged (before == after), so the
        // plan keeps their exact bytes and a failed apply can put them back
        // after nmcli rewrote them (D8).
        let parent = metadata
            .connections
            .iter()
            .find(|c| c.interface == candidate.interface && !c.removed);
        match parent {
            Some(parent) => Ok(persisted(host, &parent.uuid)?.files),
            None => Ok(Files::new()),
        }
    }
    fn recheck(&self, host: &Host, plan: &Plan) -> crate::Result<()> {
        if plan.handoff.is_some() {
            return recheck_handoff(host, plan);
        }
        self.snapshot(host, &plan.previous, &plan.candidate)
            .map_err(|e| e.to_string())?;
        let existing = uuids(host, false)?;
        let active = uuids(host, true)?;
        for change in &plan.metadata.connections {
            if let Some(before) = &change.before {
                if properties(host, &change.uuid)? != *before
                    || active.contains(&change.uuid) != change.active
                {
                    return Err("NetworkManager connection changed after staging".into());
                }
            } else if existing.contains(&change.uuid) {
                return Err("Planned NetworkManager UUID was claimed after staging".into());
            }
        }
        Ok(())
    }
    fn activate(&self, host: &Host, plan: &Plan, _restore: bool) -> crate::Result<()> {
        for change in &plan.metadata.connections {
            if change.removed {
                modify(host, &change.uuid, &change.after)?;
                down(host, &change.uuid)?;
                continue;
            }
            if change.before.is_none() && !uuids(host, false)?.contains(&change.uuid) {
                let tag = change.tag.ok_or("Missing new VLAN tag")?.to_string();
                host.run(
                    NMCLI,
                    &[
                        "connection",
                        "add",
                        "type",
                        "vlan",
                        "con-name",
                        &change.name,
                        "ifname",
                        &change.interface,
                        "dev",
                        &plan.candidate.interface,
                        "id",
                        &tag,
                        "connection.uuid",
                        &change.uuid,
                    ],
                )?;
            }
            modify(host, &change.uuid, &change.after)?;
            let optional_dhcp = change
                .after
                .get("ipv4.method")
                .is_some_and(|method| method == "auto")
                && !plan.candidate.networks.iter().any(|n| {
                    plan.candidate.ifname(n) == change.interface
                        && (n.id == plan.candidate.management || n.id == plan.candidate.audio)
                });
            up(host, &change.uuid, optional_dhcp)?;
        }
        Ok(())
    }
    fn same_file(&self, path: &str, saved: Option<&str>, found: Option<&str>) -> bool {
        if in_profile_directory(path) {
            same_profile(path, saved, found)
        } else {
            saved == found
        }
    }
    fn verify(&self, host: &Host, plan: &Plan, restore: bool) -> crate::Result<()> {
        if restore {
            return host.settle(|| verify_restored(host, plan));
        }
        for change in &plan.metadata.connections {
            if change.removed {
                if field(host, &change.uuid, "connection.autoconnect")? != "no"
                    || uuids(host, true)?.contains(&change.uuid)
                {
                    return Err("verify_backend: removed_connection_active".into());
                }
            } else {
                let active = host.run(
                    NMCLI,
                    &[
                        "--escape",
                        "no",
                        "-g",
                        "GENERAL.CON-UUID",
                        "device",
                        "show",
                        &change.interface,
                    ],
                )?;
                let actual = properties(host, &change.uuid)?;
                // Only what the profile can represent is in `after`; each
                // property that did not take is named in the error.
                let differing: Vec<_> = change
                    .after
                    .iter()
                    .filter(|(key, expected)| {
                        !actual.get(*key).is_some_and(|value| {
                            if matches!(
                                key.as_str(),
                                "ipv4.addresses" | "ipv4.dns" | "ipv4.dns-search"
                            ) {
                                split(value) == split(expected)
                            } else {
                                value == *expected
                            }
                        })
                    })
                    .map(|(key, _)| key.as_str())
                    .collect();
                let mut details = Vec::new();
                if active.trim() != change.uuid {
                    details.push(format!(
                        "device {} did not select connection '{}'",
                        label(&change.interface),
                        label(&change.name)
                    ));
                }
                if !differing.is_empty() {
                    details.push(format!(
                        "connection '{}': {}",
                        label(&change.name),
                        differing.join(", ")
                    ));
                }
                if !details.is_empty() {
                    return Err(format!(
                        "verify_backend: foreign_configuration_selected ({})",
                        details.join("; ")
                    ));
                }
            }
        }
        Ok(())
    }
    fn restore(&self, host: &Host, plan: &Plan) -> crate::Result<()> {
        for change in plan.metadata.connections.iter().rev() {
            if change.before.is_none() && uuids(host, false)?.contains(&change.uuid) {
                down(host, &change.uuid)?;
                host.run(NMCLI, &["connection", "delete", "uuid", &change.uuid])?;
            }
        }
        host.write_files(plan, true)?;
        if plan.handoff.is_some() {
            host.run(NMCLI, &["connection", "reload"])?;
        }
        for change in &plan.metadata.connections {
            if let Some(before) = &change.before {
                modify(host, &change.uuid, before)?;
                if change.active {
                    let media_dhcp = before
                        .get("ipv4.method")
                        .is_some_and(|method| method == "auto")
                        && change.interface
                            != plan
                                .previous
                                .ifname(plan.previous.assigned(&plan.previous.management)?);
                    up(host, &change.uuid, media_dhcp)?;
                } else {
                    down(host, &change.uuid)?;
                }
            }
        }
        if plan.handoff.is_some() || !profile_files(plan).is_empty() {
            // nmcli persists normalized keyfiles even for equivalent settings,
            // and a Netplan-backed profile gains passthrough keys. Restore the
            // exact bytes after the properties/activation, then reload those
            // same settings without another persistent edit.
            host.write_files(plan, true)?;
            host.run(NMCLI, &["connection", "reload"])?;
        }
        Ok(())
    }
    fn commit(&self, host: &Host, plan: &Plan) -> crate::Result<()> {
        for change in plan.metadata.connections.iter().filter(|c| c.removed) {
            if uuids(host, false)?.contains(&change.uuid) {
                host.run(NMCLI, &["connection", "delete", "uuid", &change.uuid])?;
            }
        }
        Ok(())
    }
}

pub fn drift(host: &Host, p: &Profiles) -> Result<Vec<tessera_state::network_profiles::Drift>> {
    let parent = active_parent(host, &p.interface)?;
    let mac =
        std::fs::read_to_string(host.path(&format!("/sys/class/net/{}/address", p.interface)))
            .map_err(|_| unsupported("wired_port_required"))?;
    let ownership = ownership(host, p, false)?;
    let mut drift = Vec::new();
    for connection in inventory(host)? {
        if connection.uuid == parent || !eligible(&connection, p, &parent, &mac) {
            continue;
        }
        let owned = ownership.get(&connection.interface) == Some(&connection.uuid);
        if !owned {
            drift.push(tessera_state::network_profiles::Drift {
                path: connection.uuid,
                selector: format!(
                    "name={} type={} autoconnect={} ifname={} mac={} parent={} tag={:?}",
                    connection.name,
                    connection.kind,
                    connection.autoconnect,
                    connection.interface,
                    connection.mac,
                    connection.parent,
                    connection.tag
                ),
                interface: p.interface.clone(),
                foreign: true,
                blocking: true,
            });
        }
    }
    Ok(drift)
}

/// Explicitly receive the quiesced owner's UUIDs without widening ordinary adoption.
pub(crate) fn handoff_metadata(
    host: &Host,
    p: &Profiles,
    adoption: &crate::handoff::Adoption,
) -> Result<Metadata> {
    let parent = adoption
        .nm_parent_uuid
        .as_deref()
        .ok_or_else(|| unsupported("missing_parent_connection"))?;
    if !valid_uuid(parent) || active_parent(host, &p.interface)? != parent {
        return Err(unsupported("missing_parent_connection"));
    }
    check_parent(host, &p.interface, parent)?;
    let active = uuids(host, true)?;
    let inventory = inventory(host)?;
    let mac =
        std::fs::read_to_string(host.path(&format!("/sys/class/net/{}/address", p.interface)))
            .map_err(|e| e.to_string())?;
    if adoption.nm_owned.values().any(|uuid| !valid_uuid(uuid))
        || adoption
            .nm_owned
            .get(&p.interface)
            .is_some_and(|uuid| uuid != parent)
    {
        return Err(unsupported("unrepresentable_configuration"));
    }
    let tags: BTreeSet<_> = p
        .networks
        .iter()
        .filter(|n| n.vlan_id.is_some())
        .map(|n| p.ifname(n))
        .collect();
    if adoption
        .nm_owned
        .keys()
        .any(|name| !tags.contains(name) && name != &p.interface)
    {
        return Err(unsupported("unrepresentable_configuration"));
    }
    for c in &inventory {
        if c.uuid == parent {
            continue;
        }
        let owned = p.networks.iter().any(|n| {
            n.vlan_id.is_some()
                && p.ifname(n) == c.interface
                && adoption.nm_owned.get(&c.interface) == Some(&c.uuid)
                && c.kind == "vlan"
                && c.tag == n.vlan_id
                && c.name == format!("ndimon-vlan-{}", c.interface)
                && (c.parent == parent || c.parent == p.interface)
        });
        if !owned && eligible(c, p, parent, &mac) {
            return Err(unsupported("foreign_connection"));
        }
    }
    let mut metadata = Metadata {
        metric: metric(host, parent, &p.interface, false)?,
        ..Metadata::default()
    };
    for name in interfaces(p) {
        let uuid = if name == p.interface {
            parent.to_owned()
        } else {
            adoption
                .nm_owned
                .get(&name)
                .cloned()
                .ok_or_else(|| unsupported("missing_parent_connection"))?
        };
        if !valid_uuid(&uuid) || !inventory.iter().any(|c| c.uuid == uuid) {
            return Err(unsupported("missing_parent_connection"));
        }
        let before = properties(host, &uuid)?;
        let row = p.networks.iter().find(|n| p.ifname(n) == name);
        let metric = metric(host, &uuid, &name, name != p.interface)?;
        let mut after = settings(p, row, metric, &name);
        if name == p.interface && unbound_netplan_parent(host, &uuid, &before)? {
            after.remove("connection.interface-name");
        }
        let connection_name = if name == p.interface {
            field(host, &uuid, "connection.id")?
        } else {
            format!("tessera-vlan-{name}")
        };
        if name != p.interface {
            after.insert("connection.id".into(), connection_name.clone());
        }
        metadata.metrics.insert(name.clone(), metric);
        metadata.connections.push(Change {
            uuid: uuid.clone(),
            name: connection_name,
            interface: name,
            tag: row.and_then(|n| n.vlan_id),
            before: Some(before),
            active: active.contains(&uuid),
            after,
            removed: false,
        });
    }
    Ok(metadata)
}

/// VLAN keyfiles are a bounded backend artifact. Preserve UUID and DHCP identity.
pub(crate) fn handoff_keyfile(text: &str, name: &str, uuid: &str) -> Result<String> {
    if !valid_uuid(uuid) {
        return Err(unsupported("unrepresentable_configuration"));
    }
    let mut section = "";
    let mut id_seen = false;
    let mut uuid_seen = false;
    let mut output = String::new();
    for line in text.lines() {
        let clean = line.trim();
        if clean.starts_with('[') {
            section = clean;
            if ![
                "[connection]",
                "[ethernet]",
                "[vlan]",
                "[ipv4]",
                "[ipv6]",
                "[proxy]",
            ]
            .contains(&section)
            {
                return Err(unsupported("unrepresentable_configuration"));
            }
        }
        if section == "[connection]" && clean.starts_with("id=") {
            if id_seen {
                return Err(unsupported("unrepresentable_configuration"));
            }
            id_seen = true;
            output.push_str(&format!("id=tessera-vlan-{name}\n"));
        } else {
            if section == "[connection]" && clean.starts_with("uuid=") {
                if uuid_seen || clean != format!("uuid={uuid}") {
                    return Err(unsupported("unrepresentable_configuration"));
                }
                uuid_seen = true;
            }
            output.push_str(line);
            output.push('\n');
        }
    }
    if !id_seen || !uuid_seen {
        return Err(unsupported("unrepresentable_configuration"));
    }
    Ok(output)
}

pub(crate) fn recheck_handoff(host: &Host, plan: &Plan) -> crate::Result<()> {
    let t = plan.handoff.as_ref().ok_or("Missing handoff")?;
    let current =
        handoff_metadata(host, &plan.previous, &t.document.adoption).map_err(|e| e.to_string())?;
    if serde_json::to_value(&current).map_err(|e| e.to_string())?
        != serde_json::to_value(&plan.metadata).map_err(|e| e.to_string())?
    {
        return Err("NetworkManager intent changed after handoff review".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The Ethernet profile of the Raspberry Pi 4 speaker as NetworkManager's Netplan plugin
    // saved it (HW-3): as the distribution made it, after a kept change, and the kept one a
    // second after `nmcli connection reload`.
    const PRISTINE_YAML: &str = include_str!("../tests/fixtures/pi4/eth0-pristine.yaml");
    const PRISTINE_KEYFILE: &str = include_str!("../tests/fixtures/pi4/eth0-pristine.nmconnection");
    const KEPT_YAML: &str = include_str!("../tests/fixtures/pi4/eth0-kept.yaml");
    const KEPT_KEYFILE: &str = include_str!("../tests/fixtures/pi4/eth0-kept.nmconnection");
    const RESAVED_YAML: &str = include_str!("../tests/fixtures/pi4/eth0-kept-resaved.yaml");
    const YAML: &str = "/etc/netplan/90-NM-11111111-1111-4111-a111-111111111111.yaml";
    const KEYFILE: &str = "/run/NetworkManager/system-connections/netplan-eth0.nmconnection";

    fn same(path: &str, saved: &str, found: &str) -> bool {
        same_profile(path, Some(saved), Some(found))
    }

    #[test]
    fn the_line_the_writer_adds_after_a_reload_is_not_a_difference() {
        assert_ne!(KEPT_YAML, RESAVED_YAML, "the recorded files do differ");
        assert!(same(YAML, KEPT_YAML, RESAVED_YAML));
        assert!(same(YAML, RESAVED_YAML, KEPT_YAML));
        // The generated keyfile never carried it; an unchanged file is the same file.
        assert!(same(KEYFILE, KEPT_KEYFILE, KEPT_KEYFILE));
        assert!(same(YAML, PRISTINE_YAML, PRISTINE_YAML));
        assert!(same(KEYFILE, PRISTINE_KEYFILE, PRISTINE_KEYFILE));
    }

    #[test]
    fn a_real_change_is_a_difference() {
        // What a kept change did is a change from the pristine profile, both ways.
        assert!(!same(YAML, PRISTINE_YAML, KEPT_YAML));
        assert!(!same(YAML, KEPT_YAML, PRISTINE_YAML));
        assert!(!same(KEYFILE, PRISTINE_KEYFILE, KEPT_KEYFILE));
        // Only the default level is the writer's own: a level somebody chose is a setting.
        let chosen = KEPT_YAML.replace(
            "ipv6.method: \"disabled\"\n",
            "ipv6.method: \"disabled\"\n          ipv6.ip6-privacy: \"0\"\n",
        );
        assert_ne!(chosen, KEPT_YAML);
        assert!(!same(YAML, KEPT_YAML, &chosen));
        assert!(!same(YAML, &chosen, RESAVED_YAML));
        let method = KEPT_YAML.replace("\"disabled\"", "\"auto\"");
        assert!(!same(YAML, KEPT_YAML, &method));
        assert!(!same(YAML, RESAVED_YAML, &method));
        // A setting of the keyfile, and a missing file.
        let keyfile = KEPT_KEYFILE.replace("route-metric=100", "route-metric=200");
        assert!(!same(KEYFILE, KEPT_KEYFILE, &keyfile));
        assert!(!same_profile(YAML, Some(KEPT_YAML), None));
        assert!(!same_profile(YAML, None, Some(KEPT_YAML)));
        assert!(same_profile(YAML, None, None));
    }

    #[test]
    fn the_activation_time_is_not_a_setting() {
        let later = KEPT_YAML.replace("1793004353", "1793004999");
        assert!(same(YAML, KEPT_YAML, &later));
        // The generated keyfile marks the line with a comment, which goes with it.
        let keyfile = KEPT_KEYFILE.replace("timestamp=1793004353", "timestamp=1793004999");
        assert!(same(KEYFILE, KEPT_KEYFILE, &keyfile));
        let gone =
            KEPT_KEYFILE.replace("#Netplan: passthrough setting\ntimestamp=1793004353\n", "");
        assert_ne!(gone, KEPT_KEYFILE);
        assert!(same(KEYFILE, KEPT_KEYFILE, &gone));
        let with_level = KEPT_KEYFILE.replace("method=disabled", "method=disabled\nip6-privacy=-1");
        assert!(same(KEYFILE, KEPT_KEYFILE, &with_level));
        let chosen = KEPT_KEYFILE.replace("method=disabled", "method=disabled\nip6-privacy=2");
        assert!(!same(KEYFILE, KEPT_KEYFILE, &chosen));
        // The level is the writer's only in the file's own section.
        let elsewhere = KEPT_KEYFILE.replace("wake-on-lan=0", "wake-on-lan=0\nip6-privacy=-1");
        assert!(!same(KEYFILE, KEPT_KEYFILE, &elsewhere));
    }

    #[test]
    fn a_comment_or_a_trailing_newline_is_still_an_edit() {
        let commented = format!("{KEPT_YAML}# edited by hand\n");
        assert!(!same(YAML, KEPT_YAML, &commented));
        assert!(!same(YAML, RESAVED_YAML, &commented));
        assert!(!same(YAML, KEPT_YAML.trim_end(), KEPT_YAML));
        assert!(!same(
            "/etc/tessera/config.d/59-x.toml",
            "a = 1\n",
            "a = 1 \n"
        ));
        assert!(in_profile_directory(YAML) && in_profile_directory(KEYFILE));
        assert!(!in_profile_directory("/etc/netplan/sub/90-NM.yaml"));
        assert!(!in_profile_directory("/etc/tessera/config.d/59-x.toml"));
    }
}
