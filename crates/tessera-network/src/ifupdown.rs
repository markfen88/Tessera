//! Conservative ifupdown adoption: the administrator hands off an owned
//! drop-in, and the effective include tree must load both managed files.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::json;
use tessera_state::network_profiles::{Ipv4, Profiles};

use crate::backend::{
    Adapter, Addressing, Files, Metadata, Result, adopt_search, unrepresentable, unsupported,
};
use crate::host::{Host, Plan, glob_match, interfaces, parse_netplan_ipv4};

const MARKER: &str = "# Managed by tessera-network.\n";
const HANDOFF: &str = "# Handed off to tessera-network.";

#[derive(Debug)]
pub struct Ifupdown;
#[derive(Debug)]
struct Stanza {
    path: String,
    interface: String,
    family: String,
    method: String,
    options: BTreeMap<String, Vec<String>>,
    handed_off: bool,
}
#[derive(Debug, Default)]
struct Tree {
    stanzas: Vec<Stanza>,
    activation: Vec<(String, String)>,
    includes: Vec<String>,
    visited: BTreeSet<String>,
}
fn owned_parent(interface: &str) -> String {
    format!("/etc/network/interfaces.d/tessera-{interface}")
}
fn include_paths(host: &Host, pattern: &str, directory: bool) -> Result<Vec<String>> {
    let (dir, name) = if directory {
        (pattern, "*")
    } else {
        pattern
            .rsplit_once('/')
            .ok_or_else(|| unsupported("unsafe_configuration"))?
    };
    if !dir.starts_with("/etc/network/") || dir.split('/').any(|p| p == "..") {
        return Err(unsupported("unsafe_configuration"));
    }
    let mut paths = Vec::new();
    for entry in host.entries(dir)? {
        if glob_match(name, &entry)
            && (!directory
                || entry
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')))
        {
            paths.push(format!("{dir}/{entry}"));
        }
    }
    Ok(paths)
}
fn read_tree(host: &Host, path: &str, tree: &mut Tree) -> Result<()> {
    if !tree.visited.insert(path.into()) {
        return Err(unsupported("duplicate_stanza"));
    }
    if tree.visited.len() > 64 {
        return Err(unsupported("unsafe_configuration"));
    }
    let Some(text) = host
        .read(path)
        .map_err(|_| unsupported("unsafe_configuration"))?
    else {
        return Ok(());
    };
    let handed_off = text.lines().any(|l| l == MARKER.trim() || l == HANDOFF);
    let mut current = None;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.ends_with('\\') {
            return Err(unsupported("unrepresentable_configuration"));
        }
        let words: Vec<_> = line.split_whitespace().collect();
        match words.as_slice() {
            ["source", patterns @ ..] | ["source-directory", patterns @ ..] => {
                current = None;
                for pattern in patterns.iter() {
                    let pattern = if pattern.starts_with('/') {
                        (*pattern).to_owned()
                    } else {
                        format!(
                            "{}/{}",
                            path.rsplit_once('/').ok_or("invalid include path")?.0,
                            pattern
                        )
                    };
                    let directory = words[0] == "source-directory";
                    tree.includes.push(if directory {
                        format!("{pattern}/*")
                    } else {
                        pattern.clone()
                    });
                    for path in include_paths(host, &pattern, directory)? {
                        read_tree(host, &path, tree)?;
                    }
                }
            }
            ["auto" | "allow-auto" | "allow-hotplug", names @ ..] => {
                current = None;
                tree.activation
                    .extend(names.iter().map(|name| (path.into(), (*name).into())));
            }
            ["iface", interface, family, method] => {
                tree.stanzas.push(Stanza {
                    path: path.into(),
                    interface: (*interface).into(),
                    family: (*family).into(),
                    method: (*method).into(),
                    options: BTreeMap::new(),
                    handed_off,
                });
                current = Some(tree.stanzas.len() - 1);
            }
            [key, values @ ..] if current.is_some() => {
                tree.stanzas[current.ok_or("missing stanza")?]
                    .options
                    .entry((*key).into())
                    .or_default()
                    .push(values.join(" "));
            }
            _ => return Err(unsupported("unrepresentable_configuration")),
        }
    }
    Ok(())
}
fn tree(host: &Host) -> Result<Tree> {
    let mut tree = Tree::default();
    read_tree(host, "/etc/network/interfaces", &mut tree)?;
    Ok(tree)
}
fn stanza<'a>(tree: &'a Tree, interface: &str, parent: &str) -> Result<&'a Stanza> {
    let rows: Vec<_> = tree
        .stanzas
        .iter()
        .filter(|s| s.interface == interface)
        .collect();
    if rows.iter().any(|s| s.family != "inet") {
        return Err(unsupported("ipv6_configuration"));
    }
    if rows.len() != 1 {
        return Err(unsupported(if rows.is_empty() {
            "missing_active_configuration"
        } else {
            "duplicate_stanza"
        }));
    }
    let row = rows[0];
    if row.path == "/etc/network/interfaces" {
        return Err(unsupported("ifupdown_main_stanza"));
    }
    if row.path != owned_parent(parent) && row.path != "/etc/network/interfaces.d/tessera-vlans"
        || !row.handed_off
    {
        return Err(unsupported("foreign_connection"));
    }
    if !tree.activation.iter().any(|(_, name)| name == interface) {
        return Err(unsupported("unrepresentable_configuration"));
    }
    if tree
        .activation
        .iter()
        .any(|(_, name)| name != interface && glob_match(name, interface))
    {
        return Err(unsupported("unrepresentable_configuration"));
    }
    Ok(row)
}
fn option<'a>(row: &'a Stanza, key: &str) -> Result<Option<&'a str>> {
    let Some(values) = row.options.get(key) else {
        return Ok(None);
    };
    if values.len() != 1 {
        return Err(unsupported("unrepresentable_configuration"));
    }
    Ok(Some(values[0].as_str()))
}
/// Whether `name` is the interface of the management network.
fn is_management(p: &Profiles, name: &str) -> bool {
    p.networks
        .iter()
        .any(|n| n.id == p.management && p.ifname(n) == name)
}
/// Addressing of one stanza. Search domains (NE-2.12a) are accepted on the
/// management stanza only; `management` says whether this is that stanza.
fn intent(row: &Stanza, management: bool) -> Result<Addressing> {
    for key in row.options.keys() {
        if matches!(
            key.as_str(),
            "pre-up" | "up" | "post-up" | "pre-down" | "down" | "post-down"
        ) {
            return Err(unsupported("custom_hooks"));
        }
        if !matches!(
            key.as_str(),
            "address"
                | "netmask"
                | "gateway"
                | "dns-nameservers"
                | "dns-search"
                | "metric"
                | "vlan-raw-device"
                | "privext"
        ) {
            return Err(unsupported("unrepresentable_configuration"));
        }
    }
    // NE-3.9a: the privacy-extension level is not IPv6 intent; it only has to be a level.
    if let Some(level) =
        option(row, "privext").map_err(|_| unrepresentable(&row.path, "privext"))?
    {
        if !matches!(level, "0" | "1" | "2") {
            return Err(unrepresentable(&row.path, "privext"));
        }
    }
    let search = option(row, "dns-search")
        .map_err(|_| unrepresentable(&row.path, "dns-search"))?
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>();
    if !management && !search.is_empty() {
        return Err(unrepresentable(&row.path, "dns-search"));
    }
    let dns_search = adopt_search(&search, &row.path, "dns-search")?;
    if !matches!(row.method.as_str(), "dhcp" | "static") {
        return Err(unsupported("unrepresentable_configuration"));
    }
    let mut addresses = Vec::new();
    if let Some(address) = option(row, "address")? {
        let address = if address.contains('/') {
            address.to_owned()
        } else {
            let mask: std::net::Ipv4Addr = option(row, "netmask")?
                .ok_or_else(|| unsupported("unrepresentable_configuration"))?
                .parse()
                .map_err(|_| unsupported("unrepresentable_configuration"))?;
            let bits = u32::from(mask);
            let prefix = bits.leading_ones();
            if bits != u32::MAX.checked_shl(32 - prefix).unwrap_or(0) {
                return Err(unsupported("unrepresentable_configuration"));
            }
            format!("{address}/{prefix}")
        };
        addresses.push(address);
    }
    Ok(Addressing {
        ipv4: parse_netplan_ipv4(&json!({"dhcp4":row.method == "dhcp", "addresses":addresses,
        "gateway4":option(row, "gateway")?, "nameservers":{"addresses":option(row, "dns-nameservers")?.unwrap_or("").split_whitespace().collect::<Vec<_>>()}}))?,
        dns_search,
    })
}
fn validate_tree(host: &Host, p: &Profiles) -> Result<Tree> {
    let tree = tree(host)?;
    for target in [
        owned_parent(&p.interface),
        "/etc/network/interfaces.d/tessera-vlans".into(),
    ] {
        if !tree
            .includes
            .iter()
            .any(|pattern| glob_match(pattern, &target))
        {
            return Err(unsupported("ifupdown_include_missing"));
        }
    }
    for name in interfaces(p) {
        let row = stanza(&tree, &name, &p.interface)?;
        if name == p.interface
            && !p.networks.iter().any(|n| n.vlan_id.is_none())
            && row.method == "manual"
        {
            if row.options.keys().any(|key| key != "privext") {
                return Err(unsupported("unrepresentable_configuration"));
            }
        } else {
            intent(row, is_management(p, &name))?;
        }
    }
    for row in &tree.stanzas {
        if (row.path == owned_parent(&p.interface)
            || row.path == "/etc/network/interfaces.d/tessera-vlans")
            && !interfaces(p).contains(&row.interface)
        {
            return Err(unsupported("foreign_connection"));
        }
    }
    Ok(tree)
}
fn files(p: &Profiles, metadata: &Metadata) -> Files {
    let mut parent = format!(
        "{MARKER}auto {}\niface {} inet manual\n",
        p.interface, p.interface
    );
    let mut vlans = MARKER.to_owned();
    let mut dhcp = Vec::new();
    let mut guarded = Vec::new();
    for row in &p.networks {
        let name = p.ifname(row);
        let mut text = format!(
            "auto {name}\niface {name} inet {}\n",
            if row.ipv4 == Ipv4::Dhcp {
                "dhcp"
            } else {
                "static"
            }
        );
        if row.vlan_id.is_some() {
            text.push_str(&format!("    vlan-raw-device {}\n", p.interface));
        }
        text.push_str(&format!("    metric {}\n", metadata.metric(&name)));
        match &row.ipv4 {
            Ipv4::Dhcp => {
                dhcp.push(name.clone());
                if row.id != p.management {
                    guarded.push(name);
                }
            }
            Ipv4::Static {
                address,
                prefix,
                gateway,
                dns,
            } => {
                text.push_str(&format!("    address {address}/{prefix}\n"));
                if let Some(gateway) = gateway {
                    text.push_str(&format!("    gateway {gateway}\n"));
                }
                if !dns.is_empty() {
                    text.push_str(&format!(
                        "    dns-nameservers {}\n",
                        dns.iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(" ")
                    ));
                }
            }
        }
        // NE-2.12a: management only, for DHCP and static stanzas alike.
        if row.id == p.management && !row.dns_search.is_empty() {
            text.push_str(&format!("    dns-search {}\n", row.dns_search.join(" ")));
        }
        if row.vlan_id.is_none() {
            parent = format!("{MARKER}{text}");
        } else {
            vlans.push_str(&text);
            vlans.push('\n');
        }
    }
    // NE-4.12/U11: dhclient's own generated configuration artifact, not a
    // checked-in shell program. Suppress hostname/NTP for management too.
    let mut hook = MARKER.to_owned();
    if !dhcp.is_empty() {
        hook.push_str(&format!(
            "case \"$interface\" in\n{})\nunset new_ntp_servers new_host_name\n;;\nesac\n",
            dhcp.join("|")
        ));
    }
    if !guarded.is_empty() {
        hook.push_str(&format!("case \"$interface\" in\n{})\nunset new_routers new_static_routes new_classless_static_routes new_rfc3442_classless_static_routes new_domain_name_servers new_domain_name new_domain_search\n;;\nesac\n", guarded.join("|")));
    }
    BTreeMap::from([
        (owned_parent(&p.interface), Some(parent)),
        (
            "/etc/network/interfaces.d/tessera-vlans".into(),
            Some(vlans),
        ),
        (
            "/etc/dhcp/dhclient-enter-hooks.d/tessera-vlans".into(),
            Some(hook),
        ),
    ])
}
fn prove_client(host: &Host, p: &Profiles, metadata: &Metadata) -> Result<()> {
    prove_client_from(host, p, metadata, false)
}
fn prove_client_from(host: &Host, p: &Profiles, metadata: &Metadata, handoff: bool) -> Result<()> {
    for row in p.networks.iter().filter(|n| n.ipv4 == Ipv4::Dhcp) {
        let version = host.run("/usr/sbin/dhclient", &["--version"])?;
        if !version.trim().starts_with("isc-dhclient-") {
            return Err(unsupported("unsupported_dhcp_client"));
        }
        let rendered = files(p, metadata)
            .into_iter()
            .filter(|(path, _)| path.starts_with("/etc/network/interfaces.d/"))
            .filter_map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join("\n");
        let output = host.runner.run(
            "/usr/sbin/ifup",
            &[
                "--no-act",
                "--verbose",
                "--force",
                "--interfaces",
                "/dev/stdin",
                &p.ifname(row),
            ],
            Some(&rendered),
        )?;
        // Debian ifupdown 0.8.44 / ISC 4.4.3: prove the exact PID/lease and
        // metric contract before replacing its blocking client invocation.
        // Never execute or attempt to interpret the dry run's shell syntax.
        let name = p.ifname(row);
        let expected = format!(
            "CLIENT=\"-i\"; dhclient -4 -v $CLIENT -pf /run/dhclient.{name}.pid -lf /var/lib/dhcp/dhclient.{name}.leases -I -df /var/lib/dhcp/dhclient6.{name}.leases {name} -e IF_METRIC={}",
            metadata.metric(&name)
        );
        if !output
            .lines()
            .any(|line| line.split_whitespace().collect::<Vec<_>>().join(" ") == expected)
        {
            return Err(unsupported("unsupported_dhcp_client"));
        }
        if host.path("/etc/dhcp/dhclient-enter-hooks").exists()
            || host.path("/etc/dhcp/dhclient-exit-hooks").exists()
        {
            return Err(unsupported("custom_hooks"));
        }
        for directory in [
            "/etc/dhcp/dhclient-enter-hooks.d",
            "/etc/dhcp/dhclient-exit-hooks.d",
        ] {
            for name in host.entries(directory)? {
                if !(handoff && name == "ndimon-vlans")
                    && !matches!(
                        name.as_str(),
                        "tessera-vlans"
                            | "debug"
                            | "resolvconf"
                            | "rfc3442-classless-routes"
                            | "timesyncd"
                            | "ntp"
                            | "resolved"
                            | "resolved-enter"
                    )
                {
                    return Err(unsupported("custom_hooks"));
                }
            }
        }
    }
    Ok(())
}
// Debian ifquery --state --list reports physical=logical mappings. A
// configured mapping still blocks deconfiguration failure, but activation
// verification requires our exact physical/logical identity.
fn configured(output: &str, name: &str, exact: bool) -> bool {
    output
        .lines()
        .any(|line| match line.trim().split_once('=') {
            Some((physical, logical)) => physical == name && (!exact || logical == name),
            None => line.trim() == name,
        })
}

fn deconfigure(host: &Host, name: &str, force: bool) -> crate::Result<()> {
    let args = if force {
        vec!["--force", name]
    } else {
        vec![name]
    };
    if let Err(error) = host.run("/usr/sbin/ifdown", &args) {
        let state = host.run("/usr/sbin/ifquery", &["--state", "--list"])?;
        let live = host.addresses()?;
        let has_address = live
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["ifname"] == name)
            .filter_map(|i| i["addr_info"].as_array())
            .flatten()
            .any(|a| a["scope"] == "global");
        if configured(&state, name, false) || has_address {
            return Err(error);
        }
    }
    Ok(())
}
fn activate(host: &Host, p: &Profiles, metadata: &Metadata) -> crate::Result<()> {
    for name in interfaces(p) {
        if p.networks
            .iter()
            .any(|n| p.ifname(n) == name && n.ipv4 == Ipv4::Dhcp)
        {
            // NE-6.12: let ifupdown prepare the link/VLAN and record ifstate,
            // then start the proven ISC client with its native nowait flag.
            // The persistent stanza stays DHCP, so ordinary ifdown and boot
            // use exactly the same client PID/lease files and option hook.
            let prepared = files(p, metadata)
                .into_iter()
                .filter(|(path, _)| path.starts_with("/etc/network/interfaces.d/"))
                .filter_map(|(_, text)| text)
                .collect::<Vec<_>>()
                .join("\n")
                .replace(
                    &format!("iface {name} inet dhcp"),
                    &format!("iface {name} inet manual"),
                );
            host.runner.run(
                "/usr/sbin/ifup",
                &["--interfaces", "/dev/stdin", &name],
                Some(&prepared),
            )?;
            host.run(
                "/usr/sbin/dhclient",
                &[
                    "-4",
                    "-nw",
                    "-v",
                    "-i",
                    "-pf",
                    &format!("/run/dhclient.{name}.pid"),
                    "-lf",
                    &format!("/var/lib/dhcp/dhclient.{name}.leases"),
                    "-I",
                    "-df",
                    &format!("/var/lib/dhcp/dhclient6.{name}.leases"),
                    "-e",
                    &format!("IF_METRIC={}", metadata.metric(&name)),
                    &name,
                ],
            )?;
        } else {
            host.run("/usr/sbin/ifup", &[&name])?;
        }
    }
    Ok(())
}
impl Adapter for Ifupdown {
    fn detect(&self, host: &Host, interface: &str) -> Result<bool> {
        Ok(tree(host)?
            .stanzas
            .iter()
            .any(|s| s.interface == interface && s.family == "inet"))
    }
    fn adopt(&self, host: &Host, interface: &str) -> Result<Addressing> {
        let tree = tree(host)?;
        for target in [
            owned_parent(interface),
            "/etc/network/interfaces.d/tessera-vlans".into(),
        ] {
            if !tree
                .includes
                .iter()
                .any(|pattern| glob_match(pattern, &target))
            {
                return Err(unsupported("ifupdown_include_missing"));
            }
        }
        let row = stanza(&tree, interface, interface)?;
        let addressing = intent(row, true)?;
        let p = Profiles {
            version: 1,
            revision: 0,
            interface: interface.into(),
            backend: tessera_state::network_profiles::Backend::Ifupdown,
            networks: vec![tessera_state::network_profiles::Network {
                id: "untagged".into(),
                name: "Untagged".into(),
                vlan_id: None,
                ipv4: addressing.ipv4.clone(),
                dns_search: addressing.dns_search.clone(),
            }],
            management: "untagged".into(),
            audio: "untagged".into(),
        };
        validate_tree(host, &p)?;
        prove_client(host, &p, &Metadata::default())?;
        Ok(addressing)
    }
    fn snapshot(&self, host: &Host, previous: &Profiles, candidate: &Profiles) -> Result<Metadata> {
        let tree = validate_tree(host, previous)?;
        if let Some(hook) = host.read("/etc/dhcp/dhclient-enter-hooks.d/tessera-vlans")? {
            if !hook.starts_with(MARKER) {
                return Err(unsupported("custom_hooks"));
            }
        }
        for row in &tree.stanzas {
            if (row
                .interface
                .starts_with(&format!("{}.", previous.interface))
                || option(row, "vlan-raw-device")? == Some(previous.interface.as_str()))
                && !interfaces(previous).contains(&row.interface)
            {
                return Err(unsupported("foreign_vlan"));
            }
            if interfaces(candidate).contains(&row.interface)
                && !interfaces(previous).contains(&row.interface)
            {
                return Err(unsupported("foreign_connection"));
            }
        }
        if candidate.networks.iter().any(|n| n.vlan_id.is_some())
            && !host.path("/etc/network/if-pre-up.d/vlan").exists()
        {
            return Err(unsupported("vlan_support_missing"));
        }
        let mut metadata = Metadata::default();
        for name in interfaces(previous) {
            let row = stanza(&tree, &name, &previous.interface)?;
            metadata.metrics.insert(
                name,
                option(row, "metric")?
                    .unwrap_or("0")
                    .parse()
                    .map_err(|_| unsupported("unrepresentable_configuration"))?,
            );
        }
        metadata.metric =
            metadata.metric(&previous.ifname(previous.assigned(&previous.management)?));
        prove_client(host, previous, &metadata)?;
        prove_client(host, candidate, &metadata)?;
        Ok(metadata)
    }
    fn render(
        &self,
        _host: &Host,
        _previous: &Profiles,
        candidate: &Profiles,
        metadata: &Metadata,
    ) -> Result<Files> {
        Ok(files(candidate, metadata))
    }
    fn prepare(&self, host: &Host, plan: &Plan) -> crate::Result<()> {
        for name in interfaces(&plan.previous).iter().rev() {
            deconfigure(host, name, false)?;
        }
        Ok(())
    }
    fn activate(&self, host: &Host, plan: &Plan, restore: bool) -> crate::Result<()> {
        activate(
            host,
            if restore {
                &plan.previous
            } else {
                &plan.candidate
            },
            &plan.metadata,
        )
    }
    fn verify(&self, host: &Host, plan: &Plan, restore: bool) -> crate::Result<()> {
        let p = if restore {
            &plan.previous
        } else {
            &plan.candidate
        };
        validate_tree(host, p).map_err(|e| e.to_string())?;
        let state = host.run("/usr/sbin/ifquery", &["--state", "--list"])?;
        for name in interfaces(p) {
            if !configured(&state, &name, true) {
                return Err("verify_backend: ifupdown_interface_not_configured".into());
            }
        }
        Ok(())
    }
    fn restore(&self, host: &Host, plan: &Plan) -> crate::Result<()> {
        // NE-4.13: force-deconfigure while candidate stanzas/hook still exist.
        for name in interfaces(&plan.candidate).iter().rev() {
            deconfigure(host, name, true)?;
        }
        host.write_files(plan, true)?;
        activate(host, &plan.previous, &plan.metadata)
    }
}

pub fn drift(host: &Host, p: &Profiles) -> Result<Vec<tessera_state::network_profiles::Drift>> {
    let tree = tree(host)?;
    let mut drift = Vec::new();
    for row in &tree.stanzas {
        if interfaces(p).contains(&row.interface)
            && (row.path != owned_parent(&p.interface)
                && row.path != "/etc/network/interfaces.d/tessera-vlans"
                || !row.handed_off)
        {
            drift.push(tessera_state::network_profiles::Drift {
                path: row.path.clone(),
                selector: format!(
                    "iface {} {} {}; includes={}; activation={}",
                    row.interface,
                    row.family,
                    row.method,
                    tree.includes.join(" "),
                    tree.activation
                        .iter()
                        .filter(|(_, pattern)| glob_match(pattern, &row.interface))
                        .map(|(_, pattern)| pattern.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
                interface: row.interface.clone(),
                foreign: true,
                blocking: true,
            });
        }
    }
    for (path, pattern) in &tree.activation {
        for name in interfaces(p) {
            if pattern != &name && glob_match(pattern, &name) {
                drift.push(tessera_state::network_profiles::Drift {
                    path: path.clone(),
                    selector: format!("activation {pattern}"),
                    interface: name,
                    foreign: true,
                    blocking: true,
                });
            }
        }
    }
    Ok(drift)
}

/// Validate only the explicitly released ifupdown files; ordinary adoption stays strict.
pub(crate) fn handoff_metadata(host: &Host, p: &Profiles) -> Result<Metadata> {
    let tree = tree(host)?;
    let paths = [
        format!("/etc/network/interfaces.d/ndimon-{}", p.interface),
        "/etc/network/interfaces.d/ndimon-vlans".into(),
    ];
    for target in paths.iter().chain(
        [
            owned_parent(&p.interface),
            "/etc/network/interfaces.d/tessera-vlans".into(),
        ]
        .iter(),
    ) {
        if !tree
            .includes
            .iter()
            .any(|pattern| glob_match(pattern, target))
        {
            return Err(unsupported("ifupdown_include_missing"));
        }
    }
    let mut metadata = Metadata::default();
    let names = interfaces(p);
    for name in &names {
        let rows: Vec<_> = tree
            .stanzas
            .iter()
            .filter(|s| &s.interface == name)
            .collect();
        if rows.len() != 1
            || !paths.contains(&rows[0].path)
            || rows[0].family != "inet"
            || !tree.activation.iter().any(|(_, n)| n == name)
            || tree
                .activation
                .iter()
                .any(|(_, n)| n != name && glob_match(n, name))
        {
            return Err(unsupported("duplicate_stanza"));
        }
        let row = rows[0];
        if let Some(n) = p.networks.iter().find(|n| p.ifname(n) == *name) {
            let found = intent(row, n.id == p.management)?;
            if found.ipv4 != n.ipv4 || found.dns_search != n.dns_search {
                return Err(unsupported("unrepresentable_configuration"));
            }
            if n.vlan_id.is_some() && option(row, "vlan-raw-device")? != Some(p.interface.as_str())
            {
                return Err(unsupported("foreign_vlan"));
            }
        } else if row.method != "manual" || !row.options.is_empty() {
            return Err(unsupported("unrepresentable_configuration"));
        }
        metadata.metrics.insert(
            name.clone(),
            option(row, "metric")?
                .unwrap_or("0")
                .parse()
                .map_err(|_| unsupported("unrepresentable_configuration"))?,
        );
    }
    for row in &tree.stanzas {
        if !names.contains(&row.interface)
            && (paths.contains(&row.path)
                || row.interface.starts_with(&format!("{}.", p.interface))
                || option(row, "vlan-raw-device")? == Some(p.interface.as_str()))
        {
            return Err(unsupported("foreign_vlan"));
        }
    }
    if p.networks.iter().any(|n| n.vlan_id.is_some())
        && !host.path("/etc/network/if-pre-up.d/vlan").exists()
    {
        return Err(unsupported("vlan_support_missing"));
    }
    metadata.metric = metadata.metric(&p.ifname(p.assigned(&p.management)?));
    prove_client_from(host, p, &metadata, true)?;
    Ok(metadata)
}
