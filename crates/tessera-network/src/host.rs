//! OS adapters. Netplan edits only the selected Ethernet definition; native
//! networkd uses exact-match files. Unknown/complex configurations fail closed.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tessera_state::network_profiles::{Backend, Ipv4, NETWORK_API, Network, Profiles};

use crate::Result;
use crate::store::{atomic, read_regular};

/// Every external command has a deadline and runs without shell expansion.
pub trait Runner: std::fmt::Debug + Send + Sync {
    /// Return stdout, or a bounded operator-readable failure.
    fn run(&self, program: &str, args: &[&str], input: Option<&str>) -> Result<String>;
}

/// Real Linux process adapter. timeout also terminates descendants.
#[derive(Debug)]
pub struct Processes;

impl Runner for Processes {
    fn run(&self, program: &str, args: &[&str], input: Option<&str>) -> Result<String> {
        let mut child = Command::new("/usr/bin/timeout")
            .args(["--kill-after=2s", "12s", program])
            .args(args)
            .env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .env("LC_ALL", "C")
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        if let Some(input) = input {
            child
                .stdin
                .take()
                .ok_or("missing stdin")?
                .write_all(input.as_bytes())
                .map_err(|e| e.to_string())?;
        }
        let output = child.wait_with_output().map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "{program}: {}",
                String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(500)
                    .collect::<String>()
            ));
        }
        if output.stdout.len() > 2 * 1024 * 1024 {
            return Err("host command output too large".into());
        }
        String::from_utf8(output.stdout).map_err(|e| e.to_string())
    }
}

/// One file replacement, including the original bytes and permissions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub before: Option<String>,
    pub before_mode: u32,
    pub after: Option<String>,
}

/// Immutable activation plan, fully snapshotted before the first OS mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub files: Vec<FileChange>,
    pub old_firewall: Option<String>,
    pub running: bool,
    pub previous: Profiles,
    pub candidate: Profiles,
    pub web_port: u16,
}

/// Host paths are relocatable for fixtures. Production always uses `/`.
#[derive(Debug, Clone)]
pub struct Host {
    pub root: PathBuf,
    pub runner: Arc<dyn Runner>,
}

impl Default for Host {
    fn default() -> Self {
        Self {
            root: "/".into(),
            runner: Arc::new(Processes),
        }
    }
}

impl Host {
    pub fn path(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }
    pub fn read(&self, path: &str) -> Result<Option<String>> {
        read_regular(&self.path(path), 2 * 1024 * 1024)
    }
    pub fn run(&self, program: &str, args: &[&str]) -> Result<String> {
        self.runner.run(program, args, None)
    }

    /// Existing NDIMon installation/bindings take precedence over standalone UI.
    pub fn external(&self) -> bool {
        [
            "/etc/ndimon-network-interfaces.json",
            "/opt/ndimon-r",
            "/etc/tessera/config.d/60-ndimon.toml",
            "/etc/tessera/config.d/62-ndimon-network.toml",
        ]
        .iter()
        .any(|p| self.path(p).exists())
    }

    pub fn addresses(&self) -> Result<Value> {
        serde_json::from_str(&self.run("/usr/sbin/ip", &["-j", "-4", "address", "show"])?)
            .map_err(|e| e.to_string())
    }

    pub fn address(&self, profiles: &Profiles, id: &str) -> Result<Option<Ipv4Addr>> {
        let name = profiles.ifname(profiles.assigned(id)?);
        Ok(self
            .addresses()?
            .as_array()
            .into_iter()
            .flatten()
            .find(|i| i["ifname"] == name)
            .and_then(|i| i["addr_info"].as_array())
            .into_iter()
            .flatten()
            .filter(|a| a["scope"] == "global")
            .filter_map(|a| a["local"].as_str()?.parse().ok())
            .next())
    }

    pub fn adopt(&self) -> Result<Profiles> {
        if self
            .run(
                "/usr/bin/systemctl",
                &["is-active", "NetworkManager.service"],
            )
            .is_ok()
        {
            return Err("Standalone VLAN configuration currently requires systemd-networkd or Netplan's networkd renderer.".into());
        }
        self.run(
            "/usr/bin/systemctl",
            &["is-active", "systemd-networkd.service"],
        )?;
        let routes: Value = serde_json::from_str(
            &self.run("/usr/sbin/ip", &["-j", "-4", "route", "show", "default"])?,
        )
        .map_err(|e| e.to_string())?;
        let names: std::collections::BTreeSet<&str> = routes
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r["dev"].as_str())
            .collect();
        if names.len() != 1 {
            return Err(
                "Cannot determine one wired management port from the default route.".into(),
            );
        }
        let interface = names
            .into_iter()
            .next()
            .ok_or("missing management port")?
            .to_owned();
        if !tessera_state::network_profiles::valid_interface(&interface)
            || interface.contains('.')
            || self
                .path(&format!("/sys/class/net/{interface}/wireless"))
                .exists()
        {
            return Err("Adoption requires a physical wired management interface.".into());
        }
        // Do not silently take over an existing tagged interface, bond or bridge.
        let links: Value =
            serde_json::from_str(&self.run("/usr/sbin/ip", &["-j", "-d", "link", "show"])?)
                .map_err(|e| e.to_string())?;
        let base = links
            .as_array()
            .into_iter()
            .flatten()
            .find(|l| l["ifname"] == interface);
        if base.is_some_and(|l| {
            matches!(
                l["linkinfo"]["info_kind"].as_str(),
                Some("bridge" | "bond" | "vlan" | "tun" | "wireguard")
            )
        }) {
            return Err("Standalone profiles require an ordinary wired port.".into());
        }
        let index = base.and_then(|l| l["ifindex"].as_u64());
        if links.as_array().into_iter().flatten().any(|l| {
            l["link"] == interface
                || l["master"] == interface
                || index.is_some_and(|i| {
                    l["link_index"].as_u64() == Some(i) && l["linkinfo"]["info_kind"] == "vlan"
                })
        }) {
            return Err("This port already has VLANs or attached interfaces; import them manually before using standalone profiles.".into());
        }
        let active = self.run(
            "/usr/bin/networkctl",
            &["status", "--no-pager", "--", &interface],
        )?;
        let active_path = active
            .lines()
            .find_map(|l| l.trim().strip_prefix("Network File:").map(str::trim))
            .ok_or("Cannot find the active networkd configuration.")?;
        let (backend, ipv4) = if active_path.starts_with("/run/systemd/network/10-netplan-") {
            let (_, entry) = self.netplan_entry(&interface)?;
            if entry
                .get("renderer")
                .and_then(Value::as_str)
                .is_some_and(|r| r != "networkd")
            {
                return Err("Netplan must use its networkd renderer.".into());
            }
            (Backend::Netplan, parse_netplan_ipv4(&entry)?)
        } else {
            let path = active_path;
            if !path.starts_with("/etc/systemd/network/")
                && !path.starts_with("/run/systemd/network/")
                && !path.starts_with("/usr/lib/systemd/network/")
            {
                return Err("Unexpected networkd configuration path.".into());
            }
            let mut effective = self.read(path)?.ok_or("Missing active networkd file")?;
            let basename = Path::new(path)
                .file_name()
                .and_then(|p| p.to_str())
                .ok_or("invalid networkd path")?;
            for dir in [
                "/usr/lib/systemd/network",
                "/run/systemd/network",
                "/etc/systemd/network",
            ] {
                let dropins = self.path(&format!("{dir}/{basename}.d"));
                if let Ok(entries) = fs::read_dir(dropins) {
                    let mut files = entries
                        .collect::<std::io::Result<Vec<_>>>()
                        .map_err(|e| e.to_string())?;
                    files.sort_by_key(|e| e.file_name());
                    for file in files {
                        if file.path().extension().is_some_and(|e| e == "conf") {
                            effective.push('\n');
                            effective.push_str(
                                &read_regular(&file.path(), 65536)?.ok_or("missing drop-in")?,
                            );
                        }
                    }
                }
            }
            (Backend::Networkd, parse_networkd_ipv4(&effective)?)
        };
        let p = Profiles {
            version: NETWORK_API,
            revision: 0,
            interface,
            backend,
            networks: vec![Network {
                id: "untagged".into(),
                name: "Untagged".into(),
                vlan_id: None,
                ipv4,
            }],
            management: "untagged".into(),
            audio: "untagged".into(),
        };
        p.validate(&p)?;
        Ok(p)
    }

    fn netplan_entry(&self, interface: &str) -> Result<(String, Value)> {
        let text = self.run("/usr/sbin/netplan", &["get", "network.ethernets"])?;
        let entries: Value = crate::yaml::parse(&text).map_err(|e| e.to_string())?;
        let mac = fs::read_to_string(self.path(&format!("/sys/class/net/{interface}/address")))
            .unwrap_or_default();
        let matches: Vec<_> = entries
            .as_object()
            .ok_or("No Netplan Ethernet configuration")?
            .iter()
            .filter(|(key, entry)| {
                key.as_str() == interface
                    || entry["set-name"] == interface
                    || entry["match"]["name"] == interface
                    || entry["match"]["macaddress"]
                        .as_str()
                        .is_some_and(|m| m.eq_ignore_ascii_case(mac.trim()))
            })
            .collect();
        if matches.len() != 1 {
            return Err("Cannot match one Netplan Ethernet definition to this port.".into());
        }
        Ok((matches[0].0.clone(), matches[0].1.clone()))
    }

    pub fn plan(&self, previous: Profiles, candidate: Profiles) -> Result<Plan> {
        candidate.validate(&previous)?;
        let links: Value =
            serde_json::from_str(&self.run("/usr/sbin/ip", &["-j", "-d", "link", "show"])?)
                .map_err(|e| e.to_string())?;
        let parent = links
            .as_array()
            .into_iter()
            .flatten()
            .find(|l| l["ifname"] == candidate.interface)
            .and_then(|l| l["ifindex"].as_u64())
            .ok_or("The physical network port is unavailable.")?;
        for network in candidate.networks.iter().filter(|n| n.vlan_id.is_some()) {
            let name = candidate.ifname(network);
            if let Some(link) = links
                .as_array()
                .into_iter()
                .flatten()
                .find(|l| l["ifname"] == name)
            {
                if !previous
                    .networks
                    .iter()
                    .any(|n| n.vlan_id == network.vlan_id)
                    || link["linkinfo"]["info_kind"] != "vlan"
                    || link["link_index"].as_u64() != Some(parent)
                {
                    return Err(format!(
                        "{name} already exists outside these managed profiles."
                    ));
                }
            }
        }
        let web: toml::Value =
            toml::from_str(self.read("/etc/tessera/web.toml")?.as_deref().unwrap_or(""))
                .map_err(|e: toml::de::Error| e.to_string())?;
        let listen: std::net::SocketAddr = web
            .get("listen")
            .and_then(toml::Value::as_str)
            .unwrap_or("127.0.0.1:8090")
            .parse()
            .map_err(|_| "Invalid web listen address")?;
        if !listen.ip().is_unspecified() {
            return Err("Set tessera-web's listen address to 0.0.0.0 before configuring management VLANs; confirmation requires a direct connection to the new management address.".into());
        }
        self.run("/usr/sbin/nft", &["--version"])?;
        self.run("/usr/bin/arping", &["-V"])?;
        let mut files = match candidate.backend {
            Backend::Networkd => self.networkd_files(&candidate)?,
            Backend::Netplan => self.netplan_files(&candidate)?,
        };
        let audio = candidate.ifname(candidate.assigned(&candidate.audio)?);
        files.insert("/etc/tessera/config.d/59-standalone-network.toml".into(), Some(format!("# Managed by tessera-network.\n[network]\ninterfaces = [\"{audio}\"]\nlocked = [\"primary\"]\n")));
        let mut changes = Vec::new();
        for (path, after) in files {
            let before = self.read(&path)?;
            let before_mode = fs::symlink_metadata(self.path(&path)).map_or(0o600, |m| {
                std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777
            });
            changes.push(FileChange {
                path,
                before,
                before_mode,
                after,
            });
        }
        Ok(Plan {
            files: changes,
            old_firewall: self
                .run(
                    "/usr/sbin/nft",
                    &["list", "table", "inet", "tessera_network"],
                )
                .ok(),
            running: self
                .run("/usr/bin/systemctl", &["is-active", "tesserad.service"])
                .is_ok(),
            previous,
            candidate,
            web_port: listen.port(),
        })
    }

    fn networkd_files(&self, p: &Profiles) -> Result<BTreeMap<String, Option<String>>> {
        let mut files = BTreeMap::new();
        if let Ok(entries) = fs::read_dir(self.path("/etc/systemd/network")) {
            for e in entries {
                let e = e.map_err(|e| e.to_string())?;
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with("00-tessera-")
                    && (name.ends_with(".network") || name.ends_with(".netdev"))
                {
                    files.insert(format!("/etc/systemd/network/{name}"), None);
                }
            }
        }
        let base = p.networks.iter().find(|n| n.vlan_id.is_none());
        let required = if base.is_some_and(|n| n.id == p.management) {
            "yes"
        } else {
            "no"
        };
        let mut text = format!(
            "[Match]\nName={}\n\n[Link]\nRequiredForOnline={required}\n\n[Network]\nLinkLocalAddressing=no\nIPv6AcceptRA=no\n",
            p.interface
        );
        for n in &p.networks {
            if n.vlan_id.is_some() {
                text.push_str(&format!("VLAN={}\n", p.ifname(n)));
            }
        }
        if let Some(n) = base {
            text.push_str(&networkd_address(n, n.id == p.management));
        }
        files.insert(
            format!("/etc/systemd/network/00-tessera-{}.network", p.interface),
            Some(text),
        );
        for n in &p.networks {
            if let Some(tag) = n.vlan_id {
                let name = p.ifname(n);
                files.insert(
                    format!("/etc/systemd/network/00-tessera-{name}.netdev"),
                    Some(format!(
                        "[NetDev]\nName={name}\nKind=vlan\n\n[VLAN]\nId={tag}\n"
                    )),
                );
                let required = if n.id == p.management { "yes" } else { "no" };
                files.insert(format!("/etc/systemd/network/00-tessera-{name}.network"), Some(format!("[Match]\nName={name}\n\n[Link]\nRequiredForOnline={required}\n\n[Network]\nLinkLocalAddressing=no\nIPv6AcceptRA=no\n{}", networkd_address(n, n.id == p.management))));
            }
        }
        Ok(files)
    }

    fn netplan_files(&self, p: &Profiles) -> Result<BTreeMap<String, Option<String>>> {
        let (id, mut base) = self.netplan_entry(&p.interface)?;
        // Editing every original definition avoids Netplan concatenating old
        // addresses/routes into a later override. Other ports stay untouched.
        const IP_KEYS: &[&str] = &[
            "dhcp4",
            "dhcp6",
            "addresses",
            "routes",
            "gateway4",
            "gateway6",
            "nameservers",
            "dhcp4-overrides",
            "dhcp6-overrides",
            "accept-ra",
            "link-local",
        ];
        let mut files = BTreeMap::new();
        let mut entries = fs::read_dir(self.path("/etc/netplan"))
            .map_err(|e| e.to_string())?
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|e| e.to_string())?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            if !e
                .path()
                .extension()
                .is_some_and(|s| s == "yaml" || s == "yml")
            {
                continue;
            }
            let path = format!("/etc/netplan/{}", e.file_name().to_string_lossy());
            if path == "/etc/netplan/90-tessera-network.yaml" {
                continue;
            }
            let mut doc: Value =
                crate::yaml::parse(&self.read(&path)?.ok_or("Netplan file disappeared")?)
                    .map_err(|e| e.to_string())?;
            if let Some(entry) = doc["network"]["ethernets"]
                .get_mut(&id)
                .and_then(Value::as_object_mut)
            {
                for key in IP_KEYS {
                    entry.remove(*key);
                }
                files.insert(
                    path,
                    Some(crate::yaml::emit(&doc).map_err(|e| e.to_string())?),
                );
            }
        }
        let object = base.as_object_mut().ok_or("Invalid Ethernet definition")?;
        for key in IP_KEYS {
            object.remove(*key);
        }
        object.extend(
            netplan_address(p.networks.iter().find(|n| n.vlan_id.is_none()), p)
                .as_object()
                .ok_or("invalid addressing")?
                .clone(),
        );
        let mut vlans = serde_json::Map::new();
        for n in &p.networks {
            if let Some(tag) = n.vlan_id {
                let mut value = netplan_address(Some(n), p);
                value["id"] = json!(tag);
                value["link"] = json!(id);
                vlans.insert(p.ifname(n), value);
            }
        }
        let doc = json!({"network":{"version":2,"renderer":"networkd","ethernets":{id:base},"vlans":vlans}});
        files.insert(
            "/etc/netplan/90-tessera-network.yaml".into(),
            Some(crate::yaml::emit(&doc).map_err(|e| e.to_string())?),
        );
        Ok(files)
    }

    pub fn write_files(&self, plan: &Plan, restore: bool) -> Result<()> {
        for file in &plan.files {
            let path = self.path(&file.path);
            let text = if restore { &file.before } else { &file.after };
            match text {
                Some(text) => atomic(
                    &path,
                    text,
                    if restore {
                        file.before_mode
                    } else if file.path.ends_with(".network")
                        || file.path.ends_with(".netdev")
                        || file.path.ends_with(".toml")
                    {
                        0o644
                    } else {
                        0o600
                    },
                )?,
                None => {
                    let _ = self.read(&file.path)?;
                    match fs::remove_file(&path) {
                        Ok(()) => fs::File::open(path.parent().ok_or("missing parent")?)
                            .and_then(|f| f.sync_all())
                            .map_err(|e| e.to_string())?,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                        Err(e) => return Err(e.to_string()),
                    }
                }
            }
        }
        Ok(())
    }

    pub fn activate(&self, p: &Profiles, old: &Profiles) -> Result<()> {
        match p.backend {
            Backend::Netplan => {
                self.run("/usr/sbin/netplan", &["generate"])?;
                self.run("/usr/sbin/netplan", &["apply"])?;
            }
            Backend::Networkd => {
                self.run("/usr/bin/networkctl", &["reload"])?;
                self.run("/usr/bin/networkctl", &["reconfigure", "--", &p.interface])?;
            }
        }
        for row in &old.networks {
            if row.vlan_id.is_some() && !p.networks.iter().any(|n| n.vlan_id == row.vlan_id) {
                let name = old.ifname(row);
                if self
                    .run("/usr/sbin/ip", &["link", "show", "dev", &name])
                    .is_ok()
                {
                    self.run("/usr/sbin/ip", &["link", "delete", "dev", &name])?;
                }
            }
        }
        Ok(())
    }

    pub fn firewall(
        &self,
        p: &Profiles,
        port: u16,
        old: Option<&str>,
        restore: bool,
    ) -> Result<()> {
        let exists = self
            .run(
                "/usr/sbin/nft",
                &["list", "table", "inet", "tessera_network"],
            )
            .is_ok();
        let mut rules = if exists {
            "delete table inet tessera_network\n".to_owned()
        } else {
            String::new()
        };
        // Boot recovery runs before the audio service. Recreate the stable
        // cgroup before parsing either new rules or a saved firewall snapshot.
        if !restore || old.is_some() {
            self.run("/usr/bin/systemctl", &["start", "tesseraaudio.slice"])?;
        }
        if restore {
            if let Some(old) = old {
                rules.push_str(old);
            }
        } else {
            rules.push_str(&firewall_rules(p, port)?);
        }
        if !rules.is_empty() {
            self.runner
                .run("/usr/sbin/nft", &["-f", "-"], Some(&rules))?;
        }
        Ok(())
    }

    pub fn wait_addresses(&self, p: &Profiles, seconds: u64) -> Result<()> {
        for _ in 0..seconds {
            let ready = [&p.management, &p.audio].into_iter().all(|id| {
                let n = p.assigned(id).ok();
                let addr = self.address(p, id).ok().flatten();
                match n.map(|n| &n.ipv4) {
                    Some(Ipv4::Static { address, .. }) => addr == Some(*address),
                    _ => addr.is_some(),
                }
            });
            if ready {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        Err("Assigned networks did not acquire their expected IPv4 addresses.".into())
    }

    /// DHCP scopes are known only after activation. Refuse overlapping live
    /// subnets and duplicate static addresses before allowing confirmation.
    pub fn verify_addresses(&self, p: &Profiles) -> Result<()> {
        let live = self.addresses()?;
        if p.backend == Backend::Networkd {
            let mut names = vec![p.interface.clone()];
            names.extend(
                p.networks
                    .iter()
                    .filter(|n| n.vlan_id.is_some())
                    .map(|n| p.ifname(n)),
            );
            for name in names {
                let status = self.run(
                    "/usr/bin/networkctl",
                    &["status", "--no-pager", "--", &name],
                )?;
                let expected = format!("/etc/systemd/network/00-tessera-{name}.network");
                if !status.lines().any(|line| {
                    line.trim()
                        .strip_prefix("Network File:")
                        .is_some_and(|path| path.trim() == expected)
                }) {
                    return Err(format!(
                        "An earlier networkd configuration overrides {name}; previous settings will be restored."
                    ));
                }
            }
        }
        let routes: Value = serde_json::from_str(
            &self.run("/usr/sbin/ip", &["-j", "-4", "route", "show", "default"])?,
        )
        .map_err(|e| e.to_string())?;
        let management = p.ifname(p.assigned(&p.management)?);
        if routes
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r["dev"].as_str())
            .any(|name| {
                name != management
                    && (name == p.interface || p.networks.iter().any(|n| p.ifname(n) == name))
            })
        {
            return Err("A default route remains on a non-management network.".into());
        }
        let mut subnets = Vec::new();
        for network in &p.networks {
            let name = p.ifname(network);
            let addresses: Vec<_> = live
                .as_array()
                .into_iter()
                .flatten()
                .find(|l| l["ifname"] == name)
                .and_then(|l| l["addr_info"].as_array())
                .into_iter()
                .flatten()
                .filter(|a| a["scope"] == "global")
                .collect();
            if addresses.is_empty() && network.id != p.management && network.id != p.audio {
                continue;
            }
            if addresses.len() != 1 {
                return Err(format!("{name} must have exactly one IPv4 address."));
            }
            let address: Ipv4Addr = addresses[0]["local"]
                .as_str()
                .ok_or("Missing live IPv4")?
                .parse()
                .map_err(|_| "Invalid live IPv4")?;
            let prefix = addresses[0]["prefixlen"]
                .as_u64()
                .ok_or("Missing live prefix")?;
            if !(8..=30).contains(&prefix) {
                return Err("Live IPv4 prefix cannot be represented by network profiles.".into());
            }
            let mask = u32::MAX << (32 - prefix);
            let start = u32::from(address) & mask;
            let end = start | !mask;
            if subnets.iter().any(|(a, b)| start <= *b && *a <= end) {
                return Err("Live network subnets overlap; check the DHCP scopes.".into());
            }
            subnets.push((start, end));
            if let Ipv4::Static { address, .. } = &network.ipv4 {
                self.run(
                    "/usr/bin/arping",
                    &[
                        "-D",
                        "-q",
                        "-c",
                        "2",
                        "-w",
                        "2",
                        "-I",
                        &name,
                        &address.to_string(),
                    ],
                )
                .map_err(|e| format!("Static address conflict check failed on {name}: {e}"))?;
            }
        }
        Ok(())
    }
}

pub fn firewall_rules(p: &Profiles, port: u16) -> Result<String> {
    let management = p.ifname(p.assigned(&p.management)?);
    let audio = p.ifname(p.assigned(&p.audio)?);
    let interfaces = p
        .networks
        .iter()
        .map(|n| format!("\"{}\"", p.ifname(n)))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "table inet tessera_network {{\n chain input {{ type filter hook input priority -10; policy accept;\n iifname != {{ \"lo\", \"{management}\" }} tcp dport {port} drop\n socket cgroupv2 level 1 \"tesseraaudio.slice\" iifname != {{ \"lo\", \"{audio}\" }} drop\n }}\n chain output {{ type filter hook output priority -10; policy accept;\n socket cgroupv2 level 1 \"tesseraaudio.slice\" oifname != {{ \"lo\", \"{audio}\" }} drop\n }}\n chain forward {{ type filter hook forward priority -10; policy accept;\n iifname {{ {interfaces} }} drop\n oifname {{ {interfaces} }} drop\n }}\n}}\n"
    ))
}

fn networkd_address(n: &Network, management: bool) -> String {
    match &n.ipv4 {
        Ipv4::Dhcp => format!(
            "DHCP=ipv4\n\n[DHCPv4]\nUseRoutes={}\nUseDNS={}\nUseNTP=no\nUseHostname=no\n",
            if management { "yes" } else { "no" },
            if management { "yes" } else { "no" }
        ),
        Ipv4::Static {
            address,
            prefix,
            gateway,
            dns,
        } => {
            let mut s = format!("Address={address}/{prefix}\n");
            if let Some(g) = gateway {
                s.push_str(&format!("Gateway={g}\n"));
            }
            for d in dns {
                s.push_str(&format!("DNS={d}\n"));
            }
            s
        }
    }
}

fn netplan_address(n: Option<&Network>, p: &Profiles) -> Value {
    let mut v = json!({"dhcp4":false,"dhcp6":false,"accept-ra":false,"link-local":[],"optional":!n.is_some_and(|n| n.id == p.management)});
    match n.map(|n| &n.ipv4) {
        Some(Ipv4::Dhcp) => {
            v["dhcp4"] = json!(true);
            v["dhcp4-overrides"] = json!({"use-routes":n.is_some_and(|n| n.id == p.management),"use-dns":n.is_some_and(|n| n.id == p.management),"use-ntp":false,"use-hostname":false});
        }
        Some(Ipv4::Static {
            address,
            prefix,
            gateway,
            dns,
        }) => {
            v["addresses"] = json!([format!("{address}/{prefix}")]);
            if let Some(g) = gateway {
                v["routes"] = json!([{"to":"default","via":g}]);
            }
            if !dns.is_empty() {
                v["nameservers"] = json!({"addresses":dns});
            }
        }
        None => (),
    }
    v
}

pub fn parse_netplan_ipv4(v: &Value) -> Result<Ipv4> {
    if v["dhcp6"] == true
        || v["gateway6"].is_string()
        || v["routing-policy"].is_array()
        || v["routes"].as_array().is_some_and(|r| {
            r.iter()
                .any(|r| r["to"] != "default" && r["to"] != "0.0.0.0/0")
        })
    {
        return Err(
            "IPv6, policy routing and custom routes cannot be represented by standalone profiles."
                .into(),
        );
    }
    let addresses = v["addresses"].as_array().cloned().unwrap_or_default();
    let dns = v["nameservers"]["addresses"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut gateways = v["routes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["via"].as_str())
        .collect::<Vec<_>>();
    if let Some(g) = v["gateway4"].as_str() {
        gateways.push(g);
    }
    if v["dhcp4"] == true {
        if !addresses.is_empty() || !gateways.is_empty() || !dns.is_empty() {
            return Err("Mixed DHCP and static overrides cannot be adopted.".into());
        }
        return Ok(Ipv4::Dhcp);
    }
    if addresses.len() != 1 || gateways.len() > 1 {
        return Err("Adoption requires one static IPv4 address or plain DHCP.".into());
    }
    let (address, prefix) = addresses[0]
        .as_str()
        .ok_or("Invalid address")?
        .split_once('/')
        .ok_or("Missing prefix")?;
    Ok(Ipv4::Static {
        address: address.parse().map_err(|_| "Invalid IPv4 address")?,
        prefix: prefix.parse().map_err(|_| "Invalid prefix")?,
        gateway: gateways
            .first()
            .map(|g| g.parse().map_err(|_| "Invalid gateway"))
            .transpose()?,
        dns: dns
            .iter()
            .map(|d| {
                d.as_str()
                    .ok_or("Invalid DNS")?
                    .parse()
                    .map_err(|_| "Invalid DNS")
            })
            .collect::<std::result::Result<_, _>>()?,
    })
}

pub fn parse_networkd_ipv4(text: &str) -> Result<Ipv4> {
    let mut v = json!({"addresses":[],"routes":[],"nameservers":{"addresses":[]}});
    let mut section = "";
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(['#', ';']))
    {
        if line.starts_with('[') {
            section = line;
            continue;
        }
        let Some((k, val)) = line.split_once('=') else {
            continue;
        };
        let (k, val) = (k.trim(), val.trim());
        match (section, k) {
            ("[Network]", "DHCP") => {
                v["dhcp4"] = json!(matches!(val, "yes" | "true" | "ipv4"));
                if val == "ipv6" || val == "yes" {
                    return Err("IPv6 DHCP cannot be adopted by IPv4 profiles.".into());
                }
            }
            ("[Network]" | "[Address]", "Address") => {
                let a = v["addresses"].as_array_mut().ok_or("addresses")?;
                if val.is_empty() {
                    a.clear();
                } else {
                    a.push(json!(val));
                }
            }
            ("[Network]", "Gateway") => {
                v["gateway4"] = json!(val);
            }
            ("[Network]", "DNS") => {
                let a = v["nameservers"]["addresses"].as_array_mut().ok_or("DNS")?;
                if val.is_empty() {
                    a.clear();
                } else {
                    a.extend(val.split_whitespace().map(|s| json!(s)));
                }
            }
            ("[Route]" | "[RoutingPolicyRule]", _) => {
                return Err("Custom networkd routes require manual configuration.".into());
            }
            _ => (),
        }
    }
    parse_netplan_ipv4(&v)
}
