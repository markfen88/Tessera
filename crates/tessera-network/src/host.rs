//! OS adapters. Netplan edits only the selected Ethernet definition; native
//! networkd uses exact-match files. Unknown/complex configurations fail closed.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tessera_state::dropin;
use tessera_state::network_profiles::{
    Backend, ConfirmPolicy, Ipv4, NETWORK_API, Network, NetworkPolicy, Profiles,
};

use crate::Result;
use crate::backend::{self, Addressing, Metadata, adopt_search, unrepresentable, unsupported};
use crate::store::{atomic, read_regular};

/// Every external command has a deadline and runs without shell expansion.
pub trait Runner: std::fmt::Debug + Send + Sync {
    /// Return stdout, or a bounded operator-readable failure.
    fn run(&self, program: &str, args: &[&str], input: Option<&str>) -> Result<String>;
    fn run_timed(
        &self,
        program: &str,
        args: &[&str],
        input: Option<&str>,
        _seconds: u64,
    ) -> Result<String> {
        self.run(program, args, input)
    }
    /// Wait between the polls of [`Host::settle`]. A fixture that models time replaces it.
    fn pause(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// [`Host::settle`] polls every `SETTLE_INTERVAL`, `SETTLE_POLLS` times: ten seconds.
const SETTLE_INTERVAL: Duration = Duration::from_millis(250);
const SETTLE_POLLS: u32 = 40;

/// Real Linux process adapter. timeout also terminates descendants.
#[derive(Debug)]
pub struct Processes;

impl Runner for Processes {
    fn run(&self, program: &str, args: &[&str], input: Option<&str>) -> Result<String> {
        self.run_timed(program, args, input, command_seconds(program, args))
    }
    fn run_timed(
        &self,
        program: &str,
        args: &[&str],
        input: Option<&str>,
        seconds: u64,
    ) -> Result<String> {
        let duration = format!("{seconds}s");
        let mut child = Command::new("/usr/bin/timeout")
            .args(["--kill-after=2s", &duration, program])
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
        let stdout = child.stdout.take().ok_or("missing stdout")?;
        let stderr = child.stderr.take().ok_or("missing stderr")?;
        let capture = |stream: Box<dyn Read + Send>| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                stream
                    .take(2 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|e| e.to_string())?;
                if bytes.len() > 2 * 1024 * 1024 {
                    return Err("host command output too large".to_owned());
                }
                Ok(bytes)
            })
        };
        let output = capture(Box::new(stdout));
        let errors = capture(Box::new(stderr));
        let status = child.wait().map_err(|e| e.to_string())?;
        let output = output.join().map_err(|_| "stdout capture failed")??;
        let errors = errors.join().map_err(|_| "stderr capture failed")??;
        if !status.success() {
            return Err(format!(
                "{program}: {}",
                String::from_utf8_lossy(&errors)
                    .chars()
                    .take(500)
                    .collect::<String>()
            ));
        }
        let output = if output.is_empty()
            && ((program == "/usr/sbin/dhclient" && args == ["--version"])
                || (program == "/usr/sbin/ifup" && args.contains(&"--no-act")))
        {
            errors
        } else {
            output
        };
        String::from_utf8(output).map_err(|e| e.to_string())
    }
}

// iproute2 emits a same-namespace parent as `link`, and may emit a numeric
// `link_index` instead. If both are present they must agree with the parent.
fn vlan_parent_matches(link: &Value, interface: &str, index: u64) -> bool {
    let named = link["link"].as_str();
    let numbered = link["link_index"].as_u64();
    (named.is_some() || numbered.is_some())
        && named.is_none_or(|name| name == interface)
        && numbered.is_none_or(|parent| parent == index)
}

fn command_seconds(program: &str, args: &[&str]) -> u64 {
    match (program, args.first().copied()) {
        ("/usr/sbin/netplan", Some("generate"))
        | ("/usr/bin/networkctl", Some("reconfigure"))
        | ("/usr/sbin/ifdown", _) => 30,
        ("/usr/sbin/netplan", Some("apply")) | ("/usr/sbin/ifup", _) => 40,
        ("/usr/bin/nmcli", _) if args.windows(2).any(|a| a == ["connection", "up"]) => 40,
        ("/usr/bin/arping", _) => 10,
        _ => 20,
    }
}
#[derive(Debug)]
struct BudgetRunner {
    inner: Arc<dyn Runner>,
    deadline: Instant,
}
impl Runner for BudgetRunner {
    fn run(&self, program: &str, args: &[&str], input: Option<&str>) -> Result<String> {
        let remaining = self
            .deadline
            .saturating_duration_since(Instant::now())
            .as_secs();
        if remaining == 0 {
            return Err("Network apply deadline exceeded".into());
        }
        self.inner.run_timed(
            program,
            args,
            input,
            remaining.min(command_seconds(program, args)),
        )
    }
    fn pause(&self, duration: Duration) {
        self.inner.pause(duration);
    }
}

/// One file replacement, including the original bytes and permissions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub before: Option<String>,
    pub before_mode: u32,
    #[serde(default)]
    pub before_uid: u32,
    #[serde(default)]
    pub before_gid: u32,
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
    #[serde(default)]
    pub metadata: Metadata,
    #[serde(default)]
    pub confirm_policy: ConfirmPolicy,
    #[serde(default)]
    pub drift: Vec<tessera_state::network_profiles::Drift>,
    #[serde(default)]
    pub session: Option<tessera_state::network_profiles::SessionPath>,
    #[serde(default)]
    pub handoff: Option<crate::handoff::Transfer>,
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
    pub fn bounded(&self, seconds: u64) -> Self {
        Self {
            root: self.root.clone(),
            runner: Arc::new(BudgetRunner {
                inner: Arc::clone(&self.runner),
                deadline: Instant::now() + Duration::from_secs(seconds),
            }),
        }
    }
    pub fn service_active(&self, unit: &str) -> Result<bool> {
        Ok(self
            .run(
                "/usr/bin/systemctl",
                &["show", "--property=ActiveState", "--value", "--", unit],
            )?
            .trim()
            == "active")
    }
    pub fn firewall_snapshot(&self) -> Result<Option<String>> {
        self.table_snapshot("tessera_network")
    }
    pub fn table_snapshot(&self, table: &str) -> Result<Option<String>> {
        if !["tessera_network", "ndimon_management"].contains(&table) {
            return Err("Invalid network guard table".into());
        }
        let tables: Value =
            serde_json::from_str(&self.run("/usr/sbin/nft", &["-j", "list", "tables"])?)
                .map_err(|e| e.to_string())?;
        if tables["nftables"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|t| t["table"]["family"] == "inet" && t["table"]["name"] == table)
        {
            self.run("/usr/sbin/nft", &["list", "table", "inet", table])
                .map(Some)
        } else {
            Ok(None)
        }
    }
    pub fn path(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }
    pub fn read(&self, path: &str) -> Result<Option<String>> {
        read_regular(&self.path(path), 2 * 1024 * 1024)
    }
    pub fn run(&self, program: &str, args: &[&str]) -> Result<String> {
        self.runner.run(program, args, None)
    }

    /// Run `check` until it passes, for at most ten seconds, and answer its last failure.
    /// What NetworkManager reports (and saves) right after a reload or an activation is
    /// not final for a moment: its writers and file monitor finish on their own (HW-3,
    /// D14, D15), so a mismatch seen at once is no proof of one.
    pub fn settle<T>(&self, mut check: impl FnMut() -> Result<T>) -> Result<T> {
        let mut polls = 0;
        loop {
            match check() {
                Err(_) if polls < SETTLE_POLLS => {
                    polls += 1;
                    self.runner.pause(SETTLE_INTERVAL);
                }
                answer => return answer,
            }
        }
    }

    /// The portal's configuration as it runs: web.toml with the drop-ins of
    /// web.d merged over it in name order (`tessera_state::dropin`, the
    /// definition tessera-web uses). Every file passes the same owner checks
    /// as web.toml alone, so a drop-in is as trustworthy as the file it
    /// extends.
    pub fn web_config(&self) -> Result<toml::Table> {
        fn parse(name: &str, text: &str) -> Result<toml::Table> {
            text.parse::<toml::Table>()
                .map_err(|e| format!("{name}: {e}"))
        }
        let mut merged = parse(
            "/etc/tessera/web.toml",
            self.read("/etc/tessera/web.toml")?.as_deref().unwrap_or(""),
        )?;
        let directory = self.path("/etc/tessera/web.d");
        let mut names = Vec::new();
        match fs::read_dir(&directory) {
            Ok(entries) => {
                for entry in entries {
                    let name = entry.map_err(|e| e.to_string())?.file_name();
                    let name = name.to_string_lossy().into_owned();
                    if dropin::counts(&name) {
                        names.push(name);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", directory.display())),
        }
        names.sort();
        for name in names {
            let path = format!("/etc/tessera/web.d/{name}");
            if let Some(text) = read_regular(&self.path(&path), dropin::MAX_BYTES)? {
                dropin::merge(&mut merged, parse(&path, &text)?);
            }
        }
        Ok(merged)
    }

    /// Read `[network] confirm` from the existing owner-controlled web.toml
    /// and its web.d drop-ins. This is administrator intent, outside
    /// browser-writable profiles: the helper remains the only enforcer and a
    /// host can lock the policy simply by owning these files. No command can
    /// change it or copy it into state.
    pub fn network_policy(&self) -> Result<NetworkPolicy> {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Config {
            network: NetworkPolicy,
        }
        let config: Config = toml::Value::Table(self.web_config()?)
            .try_into()
            .map_err(|e: toml::de::Error| e.to_string())?;
        Ok(config.network)
    }
    pub fn confirm_policy(&self) -> Result<ConfirmPolicy> {
        self.network_policy().map(|p| p.confirm)
    }

    /// Existing NDIMon installation/bindings take precedence over standalone UI.
    pub fn external(&self) -> bool {
        if crate::handoff::received(self).unwrap_or(false) {
            return false;
        }
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

    /// Wired ports that could carry management: the default-route ports and the
    /// physical ports of the machine, without wireless links and without `except`.
    fn wired_candidates(&self, routed: &[&str], except: Option<&str>) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> =
            routed.iter().map(|name| (*name).to_owned()).collect();
        for name in self.entries("/sys/class/net").unwrap_or_default() {
            if self.path(&format!("/sys/class/net/{name}/device")).exists() {
                names.insert(name);
            }
        }
        names
            .into_iter()
            .filter(|name| {
                tessera_state::network_profiles::valid_interface(name)
                    && !name.contains('.')
                    && Some(name.as_str()) != except
                    && !self
                        .path(&format!("/sys/class/net/{name}/wireless"))
                        .exists()
            })
            .collect()
    }

    /// A refusal about the management port. The atom stays the typed `what`; the
    /// message says why, names the setting that chooses the port (D6) and the
    /// wired ports found.
    fn port_refusal(
        &self,
        what: &str,
        why: &str,
        routed: &[&str],
        except: Option<&str>,
    ) -> backend::Failure {
        let candidates = self.wired_candidates(routed, except);
        let example = candidates.first().map_or("<port>", String::as_str);
        let found = if candidates.is_empty() {
            "no other wired port was found".to_owned()
        } else {
            format!("wired candidates: {}", candidates.join(", "))
        };
        backend::unsupported_with(
            what,
            &format!(
                "{why} Name the wired management port: set `interface = \"{example}\"` under `[network]` in /etc/tessera/web.toml or a file in /etc/tessera/web.d/ ({found})."
            ),
        )
    }

    pub fn adopt(&self) -> backend::Result<Profiles> {
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
        let routed: Vec<&str> = names.iter().copied().collect();
        let interface = match self.network_policy()?.interface {
            Some(interface) => interface,
            None => {
                if names.is_empty() {
                    return Err(self.port_refusal(
                        "no_wired_interface",
                        "No port has a default route, so there is no management port to choose.",
                        &routed,
                        None,
                    ));
                }
                if names.len() != 1 {
                    return Err(self.port_refusal(
                        "ambiguous_management_port",
                        &format!(
                            "The default routes use more than one port ({}), so the management port cannot be chosen.",
                            routed.join(", ")
                        ),
                        &routed,
                        None,
                    ));
                }
                names
                    .into_iter()
                    .next()
                    .ok_or("missing management port")?
                    .to_owned()
            }
        };
        let shown = shown_port(&interface);
        if !tessera_state::network_profiles::valid_interface(&interface)
            || interface.contains('.')
            || self
                .path(&format!("/sys/class/net/{interface}/wireless"))
                .exists()
        {
            return Err(self.port_refusal(
                "wired_port_required",
                &format!("{shown} is not an ordinary wired port (it is wireless, tagged or not a valid port name)."),
                &routed,
                Some(&interface),
            ));
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
        if base
            .and_then(|l| l["linkinfo"]["info_kind"].as_str())
            .is_some_and(|kind| kind != "ether" && !(kind == "veth" && self.root != Path::new("/")))
        {
            return Err(self.port_refusal(
                "unsupported_link_kind",
                &format!(
                    "{shown} is not an Ethernet port (it is a bond, bridge or other virtual link)."
                ),
                &routed,
                Some(&interface),
            ));
        }
        let base = base.ok_or_else(|| {
            self.port_refusal(
                "wired_port_required",
                &format!("{shown} is not a network port of this machine."),
                &routed,
                Some(&interface),
            )
        })?;
        if !base["master"].is_null() {
            return Err(self.port_refusal(
                "attached_interfaces",
                &format!("{shown} belongs to a bond or bridge."),
                &routed,
                Some(&interface),
            ));
        }
        let index = base["ifindex"].as_u64();
        if links.as_array().into_iter().flatten().any(|l| {
            l["link"] == interface
                || l["master"] == interface
                || index.is_some_and(|i| {
                    l["link_index"].as_u64() == Some(i) && l["linkinfo"]["info_kind"] == "vlan"
                })
        }) {
            let vlan = links.as_array().into_iter().flatten().any(|l| {
                l["linkinfo"]["info_kind"] == "vlan"
                    && (l["link"] == interface || l["link_index"].as_u64() == index)
            });
            return Err(unsupported(if vlan {
                "foreign_vlan"
            } else {
                "attached_interfaces"
            }));
        }
        let nm = backend::adapter(Backend::NetworkManager).detect(self, &interface)?;
        let networkd_active = self.service_active("systemd-networkd.service")?;
        let networkd_managed = if networkd_active {
            let output = self.run(
                "/usr/bin/networkctl",
                &["status", "--no-pager", "--", &interface],
            )?;
            output.lines().any(|line| {
                line.trim()
                    .strip_prefix("Network File:")
                    .is_some_and(|p| p.trim().ends_with(".network"))
            })
        } else {
            false
        };
        if nm && networkd_managed {
            return Err(unsupported("ambiguous_backend"));
        }
        let backend = if nm {
            Backend::NetworkManager
        } else if networkd_managed {
            if backend::adapter(Backend::Netplan).detect(self, &interface)? {
                Backend::Netplan
            } else {
                Backend::Networkd
            }
        } else if backend::adapter(Backend::Ifupdown).detect(self, &interface)? {
            Backend::Ifupdown
        } else {
            return Err(self.port_refusal(
                "unknown_backend",
                &format!(
                    "No supported network manager (NetworkManager, systemd-networkd, Netplan or ifupdown) manages {shown}."
                ),
                &routed,
                Some(&interface),
            ));
        };
        let addressing = crate::backend::adapter(backend).adopt(self, &interface)?;
        let p = Profiles {
            version: NETWORK_API,
            revision: 0,
            interface,
            backend,
            networks: vec![Network {
                id: "untagged".into(),
                name: "Untagged".into(),
                vlan_id: None,
                ipv4: addressing.ipv4,
                dns_search: addressing.dns_search,
            }],
            management: "untagged".into(),
            audio: "untagged".into(),
        };
        p.validate(&p)?;
        Ok(p)
    }

    pub(crate) fn active_network_file(&self, interface: &str) -> Result<String> {
        let active = self.run(
            "/usr/bin/networkctl",
            &["status", "--no-pager", "--", interface],
        )?;
        active
            .lines()
            .find_map(|line| {
                line.trim()
                    .strip_prefix("Network File:")
                    .map(|p| p.trim().to_owned())
            })
            .ok_or_else(|| "Cannot find the active networkd configuration.".into())
    }

    pub(crate) fn effective_networkd(&self, path: &str) -> backend::Result<String> {
        const ROOTS: [&str; 4] = [
            "/usr/lib/systemd/network",
            "/usr/local/lib/systemd/network",
            "/run/systemd/network",
            "/etc/systemd/network",
        ];
        let base = Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| unsupported("unexpected_configuration_path"))?;
        if !ROOTS.iter().any(|root| path == format!("{root}/{base}")) || !base.ends_with(".network")
        {
            return Err(unsupported("unexpected_configuration_path"));
        }
        let mut effective = self
            .read(path)
            .map_err(|_| unsupported("unsafe_configuration"))?
            .ok_or_else(|| unsupported("missing_active_configuration"))?;
        let mut dropins = BTreeMap::new();
        for root in ROOTS {
            for name in self.entries(&format!("{root}/{base}.d"))? {
                if name.ends_with(".conf") {
                    dropins.insert(name.clone(), format!("{root}/{base}.d/{name}"));
                }
            }
        }
        for path in dropins.values() {
            effective.push('\n');
            effective.push_str(
                &self
                    .read(path)
                    .map_err(|_| unsupported("unsafe_configuration"))?
                    .ok_or_else(|| unsupported("missing_active_configuration"))?,
            );
        }
        Ok(effective)
    }

    pub(crate) fn entries(&self, directory: &str) -> Result<Vec<String>> {
        let entries = match fs::read_dir(self.path(directory)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.to_string()),
        };
        let mut names = entries
            .map(|e| {
                e.map_err(|e| e.to_string()).and_then(|e| {
                    e.file_name()
                        .into_string()
                        .map_err(|_| "Invalid configuration filename".into())
                })
            })
            .collect::<Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }

    pub(crate) fn networkd_metadata(
        &self,
        p: &Profiles,
        netplan: bool,
    ) -> backend::Result<Metadata> {
        let mut metadata = Metadata::default();
        let names = interfaces(p);
        for name in names {
            let path = self.active_network_file(&name)?;
            let effective = self.effective_networkd(&path)?;
            let dhcp = p
                .networks
                .iter()
                .find(|n| p.ifname(n) == name)
                .is_some_and(|n| n.ipv4 == Ipv4::Dhcp);
            let metric = if netplan && name == p.interface {
                let (_, entry) = self.netplan_entry(&p.interface)?;
                netplan_metric(&entry, dhcp)?
            } else {
                networkd_metric(
                    &effective,
                    if dhcp {
                        if netplan { 100 } else { 1024 }
                    } else {
                        0
                    },
                )?
            };
            metadata.selected.insert(name.clone(), path);
            metadata.metrics.insert(name, metric);
        }
        let management = p.ifname(p.assigned(&p.management)?);
        metadata.metric = metadata.metric(&management);
        Ok(metadata)
    }

    pub(crate) fn netplan_entry(&self, interface: &str) -> backend::Result<(String, Value)> {
        let text = self.run("/usr/sbin/netplan", &["get", "network.ethernets"])?;
        let entries = crate::yaml::parse(&text)?;
        let mac = fs::read_to_string(self.path(&format!("/sys/class/net/{interface}/address")))
            .map_err(|_| unsupported("wired_port_required"))?;
        let entries = entries
            .as_object()
            .ok_or_else(|| unsupported("missing_parent_definition"))?;
        let mut matches = Vec::new();
        for (key, entry) in entries {
            let selectors = entry.get("match");
            let name = entry["match"]["name"].as_str();
            let mac_match = entry["match"]["macaddress"].as_str();
            if selectors.and_then(Value::as_object).is_some_and(|m| {
                m.keys()
                    .any(|k| !matches!(k.as_str(), "name" | "macaddress"))
            }) {
                return Err(unsupported("unrepresentable_configuration"));
            }
            let matched = if selectors.is_some() {
                let has_selector = name.is_some() || mac_match.is_some();
                let name_matches = match name {
                    None => true,
                    Some(pattern) if glob_match(pattern, interface) => true,
                    Some(pattern) if entry["set-name"] == interface => {
                        self.pre_rename_match(interface, pattern)?
                    }
                    Some(_) => false,
                };
                has_selector
                    && name_matches
                    && mac_match.is_none_or(|m| m.eq_ignore_ascii_case(mac.trim()))
            } else {
                key == interface
            };
            if matched
                && entry["set-name"]
                    .as_str()
                    .is_none_or(|name| name == interface)
            {
                matches.push((key, entry));
            }
        }
        if matches.len() != 1 {
            return Err(unsupported(if matches.is_empty() {
                "missing_parent_definition"
            } else {
                "ambiguous_parent_definition"
            }));
        }
        let mut entry = matches[0].1.clone();
        let global = self.run("/usr/sbin/netplan", &["get", "network.renderer"])?;
        let renderer = entry["renderer"].as_str().unwrap_or(global.trim());
        if !matches!(renderer, "networkd" | "null" | "") {
            return Err(unsupported(if renderer == "NetworkManager" {
                "backend_not_managing_port"
            } else {
                "unsupported_renderer"
            }));
        }
        if let Some(object) = entry.as_object_mut() {
            object.remove("renderer");
        }
        Ok((matches[0].0.clone(), entry))
    }

    fn pre_rename_match(&self, interface: &str, original: &str) -> backend::Result<bool> {
        let status = self.run(
            "/usr/bin/networkctl",
            &["status", "--no-pager", "--", interface],
        )?;
        let Some(path) = status
            .lines()
            .find_map(|line| line.trim().strip_prefix("Link File:").map(str::trim))
        else {
            return Ok(false);
        };
        const ROOTS: [&str; 4] = [
            "/usr/lib/systemd/network",
            "/usr/local/lib/systemd/network",
            "/run/systemd/network",
            "/etc/systemd/network",
        ];
        let base = Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| unsupported("unsafe_configuration"))?;
        if !base.ends_with(".link") || !ROOTS.iter().any(|root| path == format!("{root}/{base}")) {
            return Ok(false);
        }
        // Selected .link evidence proves the original-name match ran before
        // udev renamed the port. A desired set-name alone is not that proof.
        for root in ROOTS {
            if self
                .entries(&format!("{root}/{base}.d"))?
                .iter()
                .any(|name| name.ends_with(".conf"))
            {
                return Err(unsupported("unrepresentable_configuration"));
            }
        }
        let text = self
            .read(path)?
            .ok_or_else(|| unsupported("missing_active_configuration"))?;
        let mut section = "";
        let mut old = Vec::new();
        let mut new = Vec::new();
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                section = line;
            }
            if let Some((key, value)) = line.split_once('=') {
                match (section, key.trim()) {
                    ("[Match]", "OriginalName") => old = systemd_words(value)?,
                    ("[Link]", "Name") => new = systemd_words(value)?,
                    _ => (),
                }
            }
        }
        Ok(old == [original] && new == [interface])
    }

    /// NE-6.15/U14: a pending move is protected by the journal/timers. Any
    /// immediate path-cutting operation must use this same guard with false.
    pub fn guard_session(
        &self,
        previous: &Profiles,
        candidate: &Profiles,
        session: &tessera_state::network_profiles::SessionPath,
        protected: bool,
    ) -> backend::Result<()> {
        let local = session.local.and_then(|ip| match ip {
            std::net::IpAddr::V4(ip) => Some(ip),
            std::net::IpAddr::V6(ip) => ip.to_ipv4_mapped(),
        });
        let Some(local) = local else {
            return Ok(());
        };
        for old in &previous.networks {
            if self.address(previous, &old.id)? != Some(local) {
                continue;
            }
            let changed = !candidate
                .networks
                .iter()
                .any(|n| n.id == old.id && n.vlan_id == old.vlan_id && n.ipv4 == old.ipv4);
            if changed && !protected {
                return Err(backend::Failure {
                    kind: Some(tessera_state::network_profiles::ConfirmationError::Locked {
                        field: "network".into(),
                        by: "session_path".into(),
                    }),
                    message: "This network carries your session.".into(),
                });
            }
        }
        Ok(())
    }

    pub fn plan(&self, previous: Profiles, candidate: Profiles) -> backend::Result<Plan> {
        self.build_plan(previous, candidate, None)
    }

    pub(crate) fn plan_handoff(
        &self,
        profiles: Profiles,
        adoption: &crate::handoff::Adoption,
        paths: &[String],
    ) -> backend::Result<Plan> {
        self.build_plan(profiles.clone(), profiles, Some((adoption, paths)))
    }

    fn build_plan(
        &self,
        previous: Profiles,
        mut candidate: Profiles,
        handoff: Option<(&crate::handoff::Adoption, &[String])>,
    ) -> backend::Result<Plan> {
        for network in &mut candidate.networks {
            network.name = network.name.trim().to_owned();
        }
        if candidate.revision != previous.revision {
            return Err(backend::Failure {
                kind: Some(
                    tessera_state::network_profiles::ConfirmationError::RevisionConflict {
                        current_revision: previous.revision.to_string(),
                    },
                ),
                message: "Network revision changed; reload the current document.".into(),
            });
        }
        if candidate != previous && previous.revision == u64::MAX {
            return Err(backend::Failure {
                kind: Some(
                    tessera_state::network_profiles::ConfirmationError::Invalid {
                        field: "revision".into(),
                        reason: "revision_overflow".into(),
                    },
                ),
                message: "Network revision exhausted.".into(),
            });
        }
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
        if handoff.is_some() {
            let base = links
                .as_array()
                .into_iter()
                .flatten()
                .find(|l| l["ifname"] == candidate.interface)
                .ok_or_else(|| unsupported("wired_port_required"))?;
            if self
                .path(&format!("/sys/class/net/{}/wireless", candidate.interface))
                .exists()
                || !base["master"].is_null()
                || base["linkinfo"]["info_kind"].as_str().is_some_and(|kind| {
                    kind != "ether" && !(kind == "veth" && self.root != Path::new("/"))
                })
            {
                return Err(unsupported("wired_port_required"));
            }
            for link in links
                .as_array()
                .into_iter()
                .flatten()
                .filter(|l| l["ifname"] != candidate.interface)
            {
                if (link["link"] == candidate.interface
                    || link["master"] == candidate.interface
                    || link["link_index"].as_u64() == Some(parent))
                    && !candidate
                        .networks
                        .iter()
                        .any(|n| n.vlan_id.is_some() && link["ifname"] == candidate.ifname(n))
                {
                    return Err(unsupported("foreign_vlan"));
                }
            }
        }
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
                    || !vlan_parent_matches(link, &candidate.interface, parent)
                    || link["linkinfo"]["info_data"]["id"].as_u64()
                        != network.vlan_id.map(u64::from)
                {
                    return Err(unsupported("foreign_vlan"));
                }
            }
        }
        let web = self.web_config()?;
        let listen: std::net::SocketAddr = web
            .get("listen")
            .and_then(toml::Value::as_str)
            .unwrap_or("127.0.0.1:8090")
            .parse()
            .map_err(|_| "Invalid web listen address")?;
        if !listen.ip().is_unspecified() {
            return Err(unsupported("management_listener_unavailable"));
        }
        self.run("/usr/sbin/nft", &["--version"])?;
        self.run(
            "/usr/sbin/nft",
            &["describe", "socket", "cgroupv2", "level", "1"],
        )
        .map_err(|_| unsupported("guard_unavailable"))?;
        if !self.path("/sys/fs/cgroup/cgroup.controllers").exists() {
            return Err(unsupported("guard_unavailable"));
        }
        self.run("/usr/bin/arping", &["-V"])?;
        let adapter = backend::adapter(candidate.backend);
        let mut metadata = match handoff {
            Some((adoption, _)) if candidate.backend == Backend::NetworkManager => {
                crate::networkmanager::handoff_metadata(self, &candidate, adoption)?
            }
            Some((_, _)) if candidate.backend == Backend::Ifupdown => {
                crate::ifupdown::handoff_metadata(self, &candidate)?
            }
            _ => adapter.snapshot(self, &previous, &candidate)?,
        };
        let drift = if handoff.is_some()
            && matches!(
                candidate.backend,
                Backend::NetworkManager | Backend::Ifupdown
            ) {
            Vec::new() // the explicit handoff metadata validates the whole inventory
        } else {
            self.drift(&candidate)?
                .into_iter()
                .filter(|d| !handoff.is_some_and(|(_, paths)| paths.contains(&d.path)))
                .collect()
        };
        if let Some(blocking) = drift.iter().find(|d| d.blocking) {
            let mut error = unsupported("unrepresentable_configuration");
            error.message = format!(
                "Foreign configuration {} blocks {} ({})",
                blocking.path, blocking.interface, blocking.selector
            );
            return Err(error);
        }
        let mut files = adapter.render(self, &previous, &candidate, &metadata)?;
        if handoff.is_some() && candidate.backend == Backend::NetworkManager {
            // The handoff review lists every file with an after-image as a write.
            // The parent's recorded files are unchanged, so a handoff leaves them out.
            files.clear();
        }
        for (path, text) in &files {
            if path.ends_with(".network") {
                if let Some(text) = text {
                    metadata.pins.insert(path.clone(), text.clone());
                }
            }
        }
        if candidate.networks.iter().any(|n| n.vlan_id.is_some()) {
            files.insert(
                "/etc/modules-load.d/tessera-8021q.conf".into(),
                Some("8021q\n".into()),
            );
        }
        let audio = candidate.ifname(candidate.assigned(&candidate.audio)?);
        files.insert("/etc/tessera/config.d/59-standalone-network.toml".into(), Some(format!("# Managed by tessera-network.\n[network]\ninterfaces = [\"{audio}\"]\nlocked = [\"primary\"]\n")));
        let mut changes = Vec::new();
        for (path, after) in files {
            let before = self.read(&path)?;
            for row in candidate.networks.iter().filter(|n| {
                n.vlan_id.is_some() && !previous.networks.iter().any(|old| old.vlan_id == n.vlan_id)
            }) {
                let name = candidate.ifname(row);
                if before.is_some()
                    && matches!(candidate.backend, Backend::Networkd | Backend::Netplan)
                    && (path == format!("/etc/systemd/network/00-tessera-{name}.network")
                        || path == format!("/etc/systemd/network/00-tessera-{name}.netdev"))
                {
                    return Err(unsupported("foreign_vlan"));
                }
            }
            let before_mode = fs::symlink_metadata(self.path(&path)).map_or(0o600, |m| {
                std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777
            });
            let metadata = fs::symlink_metadata(self.path(&path)).ok();
            use std::os::unix::fs::MetadataExt;
            changes.push(FileChange {
                before_uid: metadata.as_ref().map_or(0, MetadataExt::uid),
                before_gid: metadata.as_ref().map_or(0, MetadataExt::gid),
                path,
                before,
                before_mode,
                after,
            });
        }
        Ok(Plan {
            files: changes,
            old_firewall: self.firewall_snapshot()?,
            running: self.service_active("tesserad.service")?,
            previous,
            candidate,
            web_port: listen.port(),
            metadata,
            confirm_policy: self.confirm_policy()?,
            drift,
            session: None,
            handoff: None,
        })
    }

    pub(crate) fn networkd_files(
        &self,
        previous: &Profiles,
        p: &Profiles,
        metadata: &Metadata,
        netdevs: bool,
    ) -> backend::Result<BTreeMap<String, Option<String>>> {
        let mut files = BTreeMap::new();
        // NE-4.1: derive ownership from the previous document, never a glob.
        for n in previous.networks.iter().filter(|n| n.vlan_id.is_some()) {
            if !p.networks.iter().any(|new| new.vlan_id == n.vlan_id) {
                let name = previous.ifname(n);
                files.insert(
                    format!("/etc/systemd/network/00-tessera-{name}.network"),
                    None,
                );
                if netdevs {
                    files.insert(
                        format!("/etc/systemd/network/00-tessera-{name}.netdev"),
                        None,
                    );
                }
            }
        }
        let base = p.networks.iter().find(|n| n.vlan_id.is_none());
        let mut text = format!(
            "[Match]\nName={}\n\n[Link]\nRequiredForOnline=no\n\n[Network]\nLinkLocalAddressing=no\nIPv6AcceptRA=no\n",
            p.interface
        );
        for n in &p.networks {
            if n.vlan_id.is_some() {
                text.push_str(&format!("VLAN={}\n", p.ifname(n)));
            }
        }
        if let Some(n) = base {
            text.push_str(&networkd_address(
                n,
                n.id == p.management,
                metadata.metric(&p.ifname(n)),
            ));
        } else {
            text.push_str("DHCP=no\n");
        }
        files.insert(
            format!("/etc/systemd/network/00-tessera-{}.network", p.interface),
            Some(text),
        );
        for n in &p.networks {
            if let Some(tag) = n.vlan_id {
                let name = p.ifname(n);
                if netdevs {
                    files.insert(
                        format!("/etc/systemd/network/00-tessera-{name}.netdev"),
                        Some(format!(
                            "[NetDev]\nName={name}\nKind=vlan\n\n[VLAN]\nId={tag}\n"
                        )),
                    );
                }
                let required = "no";
                files.insert(format!("/etc/systemd/network/00-tessera-{name}.network"), Some(format!("[Match]\nName={name}\n\n[Link]\nRequiredForOnline={required}\n\n[Network]\nLinkLocalAddressing=no\nIPv6AcceptRA=no\n{}", networkd_address(n, n.id == p.management, metadata.metric(&p.ifname(n))))));
            }
        }
        Ok(files)
    }

    pub(crate) fn netplan_files(
        &self,
        previous: &Profiles,
        p: &Profiles,
        metadata: &Metadata,
    ) -> backend::Result<BTreeMap<String, Option<String>>> {
        let (id, mut base) = self.netplan_entry(&p.interface)?;
        // Editing every original definition avoids Netplan concatenating old
        // addresses/routes into a later override. Other ports stay untouched.
        // NE-3.9a: the privacy-extension setting goes with the other IPv6 keys; the managed
        // definitions have IPv6 off, so it has no effect there, and the source file's
        // before-image brings it back on Revert.
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
            "ipv6-privacy",
        ];
        let mut files = BTreeMap::new();
        let mut sources = BTreeMap::new();
        for root in ["/lib/netplan", "/etc/netplan", "/run/netplan"] {
            for name in self.entries(root)? {
                if name.ends_with(".yaml") || name.ends_with(".yml") {
                    sources.insert(name.clone(), format!("{root}/{name}"));
                }
            }
        }
        for path in sources.into_values() {
            if path == "/etc/netplan/90-tessera-network.yaml" {
                continue;
            }
            let mut doc: Value =
                crate::yaml::parse(&self.read(&path)?.ok_or("Netplan file disappeared")?)
                    .map_err(|e| e.to_string())?;
            for (name, vlan) in doc["network"]["vlans"].as_object().into_iter().flatten() {
                if (vlan["link"] == id || interfaces(p).contains(name))
                    && !previous
                        .networks
                        .iter()
                        .any(|n| n.vlan_id.is_some() && previous.ifname(n) == *name)
                {
                    return Err(unsupported("foreign_vlan"));
                }
            }
            if doc["network"]["ethernets"].get(&id).is_some() && sensitive(&doc) {
                return Err(unsupported("unsafe_configuration"));
            }
            if let Some(entry) = doc["network"]["ethernets"]
                .get_mut(&id)
                .and_then(Value::as_object_mut)
            {
                for key in IP_KEYS {
                    entry.remove(*key);
                }
                if doc["network"].get("wifis").is_some() {
                    return Err(unsupported("unsafe_configuration"));
                }
                if !path.starts_with("/etc/netplan/") {
                    return Err(unsupported("unsafe_configuration"));
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
            netplan_address(
                p.networks.iter().find(|n| n.vlan_id.is_none()),
                p,
                metadata.metric(&p.interface),
            )
            .as_object()
            .ok_or("invalid addressing")?
            .clone(),
        );
        let mut vlans = serde_json::Map::new();
        for n in &p.networks {
            if let Some(tag) = n.vlan_id {
                let mut value = netplan_address(Some(n), p, metadata.metric(&p.ifname(n)));
                value["id"] = json!(tag);
                value["link"] = json!(id);
                vlans.insert(p.ifname(n), value);
            }
        }
        base["renderer"] = json!("networkd");
        let mut doc = self
            .read("/etc/netplan/90-tessera-network.yaml")?
            .map(|text| crate::yaml::parse(&text))
            .transpose()?
            .unwrap_or_else(|| json!({"network":{"version":2,"ethernets":{},"vlans":{}}}));
        if sensitive(&doc) {
            return Err(unsupported("unsafe_configuration"));
        }
        doc["network"]["ethernets"][&id] = base;
        if doc["network"].get("vlans").is_none() {
            doc["network"]["vlans"] = json!({});
        }
        let old_vlans = doc["network"]["vlans"]
            .as_object_mut()
            .ok_or_else(|| unsupported("unrepresentable_configuration"))?;
        for row in &previous.networks {
            if row.vlan_id.is_some() {
                old_vlans.remove(&previous.ifname(row));
            }
        }
        for (name, mut entry) in vlans {
            if old_vlans.contains_key(&name) {
                return Err(unsupported("foreign_vlan"));
            }
            entry["renderer"] = json!("networkd");
            old_vlans.insert(name, entry);
        }
        files.insert(
            "/etc/netplan/90-tessera-network.yaml".into(),
            Some(crate::yaml::emit(&doc).map_err(|e| e.to_string())?),
        );
        Ok(files)
    }

    pub(crate) fn prepare_parent(&self, path: &Path) -> Result<()> {
        let parent = path.parent().ok_or("missing parent")?;
        if !parent.exists() {
            self.prepare_parent(parent)?;
            fs::create_dir(parent).map_err(|e| e.to_string())?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o755))
                .map_err(|e| e.to_string())?;
        }
        crate::store::safe_parent(path)
    }

    /// NE-5.1/5.2: effective same-basename precedence and append/reset selectors.
    pub fn drift(
        &self,
        p: &Profiles,
    ) -> backend::Result<Vec<tessera_state::network_profiles::Drift>> {
        if p.backend == Backend::NetworkManager {
            return crate::networkmanager::drift(self, p);
        }
        if p.backend == Backend::Ifupdown {
            return crate::ifupdown::drift(self, p);
        }
        let mut files = BTreeMap::new();
        for root in [
            "/usr/lib/systemd/network",
            "/usr/local/lib/systemd/network",
            "/run/systemd/network",
            "/etc/systemd/network",
        ] {
            for name in self.entries(root)? {
                if name.ends_with(".network") {
                    files.insert(name.clone(), format!("{root}/{name}"));
                }
            }
        }
        let mut drift = Vec::new();
        for path in files.values() {
            let text = self.effective_networkd(path)?;
            let (names, macs) = networkd_selectors(&text)?;
            for interface in interfaces(p) {
                let pin = format!("/etc/systemd/network/00-tessera-{interface}.network");
                if *path == pin {
                    continue;
                }
                let mac =
                    fs::read_to_string(self.path(&format!("/sys/class/net/{interface}/address")))
                        .unwrap_or_default();
                if selectors_match(&names, &interface)
                    && (macs.is_empty() || macs.iter().any(|m| m.eq_ignore_ascii_case(mac.trim())))
                {
                    drift.push(tessera_state::network_profiles::Drift {
                        path: path.clone(),
                        selector: format!("Name={}", names.join(" ")),
                        interface,
                        foreign: true,
                        blocking: Path::new(path).file_name() < Path::new(&pin).file_name(),
                    });
                }
            }
        }
        for interface in interfaces(p) {
            let pin = format!("00-tessera-{interface}.network");
            for root in [
                "/usr/lib/systemd/network",
                "/usr/local/lib/systemd/network",
                "/run/systemd/network",
                "/etc/systemd/network",
            ] {
                for name in self.entries(&format!("{root}/{pin}.d"))? {
                    if !name.ends_with(".conf") {
                        continue;
                    }
                    let path = format!("{root}/{pin}.d/{name}");
                    if self.read(&path)?.is_some_and(|text| {
                        text.lines()
                            .map(str::trim)
                            .any(|line| !line.is_empty() && !line.starts_with(['#', ';']))
                    }) {
                        drift.push(tessera_state::network_profiles::Drift {
                            path,
                            selector: format!("drop-in for {pin}"),
                            interface: interface.clone(),
                            foreign: true,
                            blocking: true,
                        });
                    }
                }
            }
        }
        if p.backend == Backend::Netplan {
            for root in ["/lib/netplan", "/etc/netplan", "/run/netplan"] {
                for file in self.entries(root)? {
                    if !file.ends_with(".yaml") && !file.ends_with(".yml") {
                        continue;
                    }
                    let path = format!("{root}/{file}");
                    if path == "/etc/netplan/90-tessera-network.yaml" {
                        continue;
                    }
                    let doc = crate::yaml::parse(
                        &self.read(&path)?.ok_or("Netplan source disappeared")?,
                    )?;
                    for (id, entry) in doc["network"]["ethernets"]
                        .as_object()
                        .into_iter()
                        .flatten()
                    {
                        for interface in interfaces(p) {
                            let mac = fs::read_to_string(
                                self.path(&format!("/sys/class/net/{interface}/address")),
                            )
                            .or_else(|_| {
                                fs::read_to_string(
                                    self.path(&format!("/sys/class/net/{}/address", p.interface)),
                                )
                            })
                            .unwrap_or_default();
                            let name_match = entry["match"]["name"].as_str().map_or(
                                id == &interface || entry["set-name"] == interface,
                                |pattern| glob_match(pattern, &interface),
                            );
                            if name_match
                                && entry["match"]["macaddress"]
                                    .as_str()
                                    .is_none_or(|m| m.eq_ignore_ascii_case(mac.trim()))
                            {
                                drift.push(tessera_state::network_profiles::Drift {
                                    path: path.clone(),
                                    selector: format!(
                                        "Ethernet {id}: match.name={}",
                                        entry["match"]["name"].as_str().unwrap_or(id)
                                    ),
                                    interface,
                                    foreign: true,
                                    blocking: false,
                                });
                            }
                        }
                    }
                }
            }
        }
        Ok(drift)
    }

    pub fn write_files(&self, plan: &Plan, restore: bool) -> Result<()> {
        for file in &plan.files {
            // Keep unchanged artifacts in the immutable plan for drift checks,
            // without rewriting them during a successful apply.
            if !restore && file.before == file.after {
                continue;
            }
            let path = self.path(&file.path);
            self.prepare_parent(&path)?;
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
                        || file.path.ends_with(".conf")
                        || file.path.starts_with("/etc/network/interfaces.d/")
                        || file.path.starts_with("/etc/dhcp/dhclient-enter-hooks.d/")
                    {
                        0o644
                    } else {
                        0o600
                    },
                )
                .and_then(|()| {
                    if restore {
                        nix::unistd::chown(
                            &path,
                            Some(nix::unistd::Uid::from_raw(file.before_uid)),
                            Some(nix::unistd::Gid::from_raw(file.before_gid)),
                        )
                        .map_err(|e| e.to_string())?;
                        fs::File::open(&path)
                            .and_then(|f| f.sync_all())
                            .map_err(|e| e.to_string())?;
                    }
                    Ok(())
                })?,
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

    /// Recheck the immutable before-images before any service or OS mutation.
    pub fn recheck(&self, plan: &Plan) -> Result<()> {
        if let Some(transfer) = &plan.handoff {
            crate::handoff::verify_reservation(self, transfer)?;
        } else if self.external() {
            return Err("NDIMon owns networking".into());
        }
        if self.service_active("tesserad.service")? != plan.running
            || self.firewall_snapshot()? != plan.old_firewall
        {
            return Err("Network runtime state changed after staging".into());
        }
        let adapter = backend::adapter(plan.candidate.backend);
        for file in &plan.files {
            if !adapter.same_file(
                &file.path,
                file.before.as_deref(),
                self.read(&file.path)?.as_deref(),
            ) {
                return Err(format!(
                    "Configuration changed after staging: {}",
                    file.path
                ));
            }
            if file.before.is_some() {
                use std::os::unix::fs::MetadataExt;
                let metadata =
                    fs::symlink_metadata(self.path(&file.path)).map_err(|e| e.to_string())?;
                if metadata.mode() & 0o777 != file.before_mode
                    || metadata.uid() != file.before_uid
                    || metadata.gid() != file.before_gid
                {
                    return Err("Configuration ownership or mode changed after staging".into());
                }
            }
        }
        if plan.handoff.is_some() {
            return crate::handoff::recheck_backend(self, plan);
        }
        if self
            .drift(&plan.candidate)
            .map_err(|e| e.to_string())?
            .iter()
            .any(|d| d.blocking)
        {
            return Err("Foreign configuration appeared after staging".into());
        }
        backend::adapter(plan.candidate.backend).recheck(self, plan)
    }

    pub fn activate(&self, plan: &Plan, restore: bool) -> Result<()> {
        let (p, old) = if restore {
            (&plan.previous, &plan.candidate)
        } else {
            (&plan.candidate, &plan.previous)
        };
        backend::adapter(p.backend).activate(self, plan, restore)?;
        self.cleanup_links(p, old)
    }

    fn cleanup_links(&self, p: &Profiles, old: &Profiles) -> Result<()> {
        for row in &old.networks {
            if row.vlan_id.is_some() && !p.networks.iter().any(|n| n.vlan_id == row.vlan_id) {
                let name = old.ifname(row);
                let links: Value =
                    serde_json::from_str(&self.run("/usr/sbin/ip", &["-j", "-d", "link", "show"])?)
                        .map_err(|e| e.to_string())?;
                if links
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|l| l["ifname"] == name)
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
        let exists = self.firewall_snapshot()?.is_some();
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
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            let live = self.addresses()?;
            let ready = p.networks.iter().all(|n| {
                let required = n.id == p.management
                    || n.id == p.audio
                    || matches!(n.ipv4, Ipv4::Static { .. });
                if !required {
                    return true;
                }
                let name = p.ifname(n);
                let addresses: Vec<_> = live
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|i| i["ifname"] == name)
                    .filter_map(|i| i["addr_info"].as_array())
                    .flatten()
                    .filter(|a| a["scope"] == "global")
                    .collect();
                if addresses.len() != 1 {
                    return false;
                }
                match &n.ipv4 {
                    Ipv4::Static {
                        address, prefix, ..
                    } => {
                        addresses[0]["local"] == address.to_string()
                            && addresses[0]["prefixlen"] == u64::from(*prefix)
                    }
                    Ipv4::Dhcp => addresses[0]["local"].is_string(),
                }
            });
            if ready {
                return Ok(());
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Err("Assigned networks did not acquire their expected IPv4 addresses.".into())
    }

    pub(crate) fn verify_networkd(&self, plan: &Plan, restore: bool) -> Result<()> {
        let p = if restore {
            &plan.previous
        } else {
            &plan.candidate
        };
        if !restore
            && self
                .drift(p)
                .map_err(|e| e.to_string())?
                .iter()
                .any(|d| d.blocking)
        {
            return Err("verify_backend: foreign_configuration_selected".into());
        }
        for name in interfaces(p) {
            let selected = self.active_network_file(&name)?;
            let expected = if restore {
                if let Some(path) = plan.metadata.selected.get(&name) {
                    path.clone()
                } else {
                    // API-1 journals predate backend metadata. The immutable
                    // file snapshots are already restored; validate old intent.
                    let pin = format!("/etc/systemd/network/00-tessera-{name}.network");
                    if selected != pin {
                        let row = p
                            .networks
                            .iter()
                            .find(|n| p.ifname(n) == name)
                            .ok_or("Missing original parent intent")?;
                        let found = parse_networkd_addressing(
                            &self
                                .effective_networkd(&selected)
                                .map_err(|e| e.to_string())?,
                            &selected,
                        )
                        .map_err(|e| e.to_string())?;
                        if found.ipv4 != row.ipv4 || found.dns_search != row.dns_search {
                            return Err("verify_backend: legacy_restore_intent_mismatch".into());
                        }
                    }
                    selected.clone()
                }
            } else {
                format!("/etc/systemd/network/00-tessera-{name}.network")
            };
            if selected != expected {
                return Err("verify_backend: foreign_configuration_selected".into());
            }
            if !restore {
                let expected_text = plan
                    .metadata
                    .pins
                    .get(&expected)
                    .ok_or("Missing planned pin contents")?;
                if self.read(&expected)?.as_ref() != Some(expected_text) {
                    return Err("verify_backend: pin_changed".into());
                }
            }
        }
        Ok(())
    }

    pub fn verify(&self, plan: &Plan, restore: bool) -> Result<()> {
        backend::adapter(plan.candidate.backend).verify(self, plan, restore)?;
        let p = if restore {
            &plan.previous
        } else {
            &plan.candidate
        };
        self.verify_addresses(p)?;
        let routes: Value = serde_json::from_str(
            &self.run("/usr/sbin/ip", &["-j", "-4", "route", "show", "default"])?,
        )
        .map_err(|e| e.to_string())?;
        let management = p.ifname(p.assigned(&p.management)?);
        for route in routes
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["dev"] == management)
        {
            if route["metric"].as_u64().unwrap_or(0) != u64::from(plan.metadata.metric(&management))
            {
                return Err("verify_backend: unexpected_route_metric".into());
            }
        }
        Ok(())
    }

    pub fn restore(&self, plan: &Plan) -> Result<()> {
        crate::backend::adapter(plan.previous.backend).restore(self, plan)?;
        self.cleanup_links(&plan.previous, &plan.candidate)
    }

    /// DHCP scopes are known only after activation. Refuse overlapping live
    /// subnets and duplicate static addresses before allowing confirmation.
    pub fn verify_addresses(&self, p: &Profiles) -> Result<()> {
        let live = self.addresses()?;
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
            if let Ipv4::Static {
                address: expected,
                prefix: expected_prefix,
                ..
            } = &network.ipv4
            {
                if address != *expected || prefix != u64::from(*expected_prefix) {
                    return Err("verify_addresses: unexpected_static_prefix".into());
                }
                let address = expected;
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

/// A configured port name as shown in a message: only a valid name is echoed.
fn shown_port(interface: &str) -> String {
    if tessera_state::network_profiles::valid_interface(interface) {
        format!("`{interface}`")
    } else {
        "the configured port".into()
    }
}

/// The TCP port of the speaker's EQ engine GUI (CamillaGUI, package `tessera-dsp`).
/// It has no login, so it is reachable on the management network only: the guard
/// rejects it on every other link, as it does the portal's port.
pub const CAMILLAGUI_PORT: u16 = 5005;

pub fn firewall_rules(p: &Profiles, port: u16) -> Result<String> {
    let management = p.ifname(p.assigned(&p.management)?);
    let audio = p.ifname(p.assigned(&p.audio)?);
    let interfaces = p
        .networks
        .iter()
        .map(|n| format!("\"{}\"", p.ifname(n)))
        .collect::<Vec<_>>()
        .join(", ");
    let gui = if port == CAMILLAGUI_PORT {
        String::new()
    } else {
        format!(" iifname != {{ \"lo\", \"{management}\" }} tcp dport {CAMILLAGUI_PORT} reject;\n")
    };
    Ok(format!(
        "table inet tessera_network {{\n chain input {{ type filter hook input priority -5; policy accept;\n iifname != {{ \"lo\", \"{management}\" }} tcp dport {port} reject;\n{gui} }}\n chain forward {{ type filter hook forward priority -5; policy accept;\n iifname {{ {interfaces} }} reject;\n oifname {{ {interfaces} }} reject;\n }}\n chain media_output {{ type filter hook output priority 0; policy accept;\n socket cgroupv2 level 1 \"tesseraaudio.slice\" oifname != {{ \"lo\", \"{audio}\" }} counter reject;\n }}\n chain media_input {{ type filter hook input priority 0; policy accept;\n socket cgroupv2 level 1 \"tesseraaudio.slice\" iifname != {{ \"lo\", \"{audio}\" }} counter reject;\n }}\n}}\n"
    ))
}

fn networkd_address(n: &Network, management: bool, metric: u32) -> String {
    // NE-2.12a: configured search domains belong to management only.
    let domains = if management && !n.dns_search.is_empty() {
        format!("Domains={}\n", n.dns_search.join(" "))
    } else {
        String::new()
    };
    match &n.ipv4 {
        Ipv4::Dhcp => format!(
            "DHCP=ipv4\n{domains}\n[DHCPv4]\nUseRoutes={}\nUseDNS={}\nUseNTP=no\nUseHostname=no\nRouteMetric={metric}\n",
            if management { "yes" } else { "no" },
            if management { "yes" } else { "no" }
        ),
        Ipv4::Static {
            address,
            prefix,
            gateway,
            dns,
        } => {
            let mut s = format!("DHCP=no\nAddress={address}/{prefix}\n");
            for d in dns {
                s.push_str(&format!("DNS={d}\n"));
            }
            s.push_str(&domains);
            if let Some(g) = gateway {
                s.push_str(&format!(
                    "\n[Route]\nDestination=0.0.0.0/0\nGateway={g}\nMetric={metric}\n"
                ));
            }
            s
        }
    }
}

fn netplan_address(n: Option<&Network>, p: &Profiles, metric: u32) -> Value {
    let mut v = json!({"dhcp4":false,"dhcp6":false,"accept-ra":false,"link-local":[],"optional":true,"addresses":[],"routes":[],"nameservers":{"addresses":[]}});
    match n.map(|n| &n.ipv4) {
        Some(Ipv4::Dhcp) => {
            v["dhcp4"] = json!(true);
            v["dhcp4-overrides"] = json!({"use-routes":n.is_some_and(|n| n.id == p.management),"use-dns":n.is_some_and(|n| n.id == p.management),"use-ntp":false,"use-hostname":false,"route-metric":metric});
        }
        Some(Ipv4::Static {
            address,
            prefix,
            gateway,
            dns,
        }) => {
            v["addresses"] = json!([format!("{address}/{prefix}")]);
            if let Some(g) = gateway {
                v["routes"] = json!([{"to":"default","via":g,"metric":metric}]);
            }
            if !dns.is_empty() {
                v["nameservers"] = json!({"addresses":dns});
            }
        }
        None => (),
    }
    // NE-2.12a: only the management definition carries search domains.
    if let Some(n) = n.filter(|n| n.id == p.management && !n.dns_search.is_empty()) {
        v["nameservers"]["search"] = json!(n.dns_search);
    }
    v
}

/// Configured `nameservers.search` of one Netplan definition (NE-2.12a).
fn netplan_search(v: &Value, source: &str) -> backend::Result<Vec<String>> {
    const KEY: &str = "nameservers.search";
    let Some(list) = v["nameservers"].get("search").filter(|s| !s.is_null()) else {
        return Ok(Vec::new());
    };
    let domains = list
        .as_array()
        .and_then(|a| a.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
        .ok_or_else(|| unrepresentable(source, KEY))?;
    adopt_search(&domains, source, KEY)
}

/// Adopt one Netplan definition: addressing plus its configured search domains.
/// `source` names the definition in refusals.
pub fn parse_netplan_addressing(v: &Value, source: &str) -> backend::Result<Addressing> {
    Ok(Addressing {
        ipv4: parse_netplan_ipv4(v)?,
        dns_search: netplan_search(v, source)?,
    })
}

/// Addressing only. `nameservers.search` is accepted here, but it is read by the
/// caller that knows which key and source to name; backends that synthesize
/// this shape read their own search key.
pub(crate) fn parse_netplan_ipv4(v: &Value) -> backend::Result<Ipv4> {
    if sensitive(v) {
        return Err(unsupported("unrepresentable_configuration"));
    }
    let fields = v
        .as_object()
        .ok_or_else(|| unsupported("unrepresentable_configuration"))?;
    for key in fields.keys() {
        if !matches!(
            key.as_str(),
            "dhcp4"
                | "dhcp6"
                | "addresses"
                | "routes"
                | "routing-policy"
                | "gateway4"
                | "gateway6"
                | "nameservers"
                | "dhcp4-overrides"
                | "dhcp6-overrides"
                | "accept-ra"
                | "link-local"
                | "ipv6-privacy"
                | "match"
                | "set-name"
                | "renderer"
                | "mtu"
                | "wakeonlan"
                | "optional"
                | "optional-addresses"
                | "critical"
                | "activation-mode"
                | "ignore-carrier"
                | "emit-lldp"
        ) {
            return Err(unsupported("unrepresentable_configuration"));
        }
    }
    if v["nameservers"].as_object().is_some_and(|fields| {
        fields.iter().any(|(key, value)| {
            !matches!(key.as_str(), "addresses" | "search")
                && !value.is_null()
                && value != &json!([])
        })
    }) {
        return Err(unsupported("unrepresentable_configuration"));
    }
    if v["routing-policy"]
        .as_array()
        .is_some_and(|a| !a.is_empty())
    {
        return Err(unsupported("policy_routing"));
    }
    // NE-3.9b (U18): a bare `dhcp6: true` is the distribution's default (stock Armbian Netplan
    // sets it on every Ethernet definition), not operator IPv6 intent. Managed output sets
    // `dhcp6: false` and Revert puts the source file back. DHCPv6 overrides, a gateway, an
    // explicit router-advertisement switch and IPv6 addresses or DNS (below) still refuse.
    if v["dhcp6-overrides"]
        .as_object()
        .is_some_and(|fields| !fields.is_empty())
        || v["gateway6"].is_string()
        || v["accept-ra"] == true
    {
        return Err(unsupported("ipv6_configuration"));
    }
    // NE-3.9a: privacy extensions are not IPv6 intent and never refuse adoption, but Netplan
    // knows them as a boolean only.
    if v.get("ipv6-privacy")
        .is_some_and(|p| !p.is_boolean() && !p.is_null())
    {
        return Err(unsupported("unrepresentable_configuration"));
    }
    let addresses = v["addresses"].as_array().cloned().unwrap_or_default();
    let dns = v["nameservers"]["addresses"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut gateways = Vec::new();
    for route in v["routes"].as_array().into_iter().flatten() {
        if !matches!(route["to"].as_str(), Some("default" | "0.0.0.0/0"))
            || route.as_object().is_none_or(|r| {
                r.keys()
                    .any(|k| !matches!(k.as_str(), "to" | "via" | "metric"))
            })
        {
            return Err(unsupported("custom_routes"));
        }
        gateways.push(
            route["via"]
                .as_str()
                .ok_or_else(|| unsupported("unrepresentable_configuration"))?,
        );
    }
    if let Some(gateway) = v["gateway4"].as_str().filter(|g| !g.is_empty()) {
        gateways.push(gateway);
    }
    gateways.sort();
    gateways.dedup();
    if addresses
        .iter()
        .any(|a| a.as_str().is_none_or(|s| s.contains(':')))
        || dns
            .iter()
            .any(|a| a.as_str().is_none_or(|s| s.contains(':')))
    {
        return Err(unsupported("ipv6_configuration"));
    }
    if gateways.len() > 1 {
        return Err(unsupported("multiple_gateways"));
    }
    if dns.len() > 3 {
        return Err(unsupported("unrepresentable_configuration"));
    }
    if v["dhcp4"] == true {
        if !addresses.is_empty() || !gateways.is_empty() || !dns.is_empty() {
            return Err(unsupported("mixed_dhcp_static"));
        }
        if v["dhcp4-overrides"]
            .as_object()
            .is_some_and(|o| o.keys().any(|k| k != "route-metric"))
        {
            return Err(unsupported("unrepresentable_configuration"));
        }
        return Ok(Ipv4::Dhcp);
    }
    if addresses.len() != 1 {
        return Err(unsupported("multiple_addresses"));
    }
    let (address, prefix) = addresses[0]
        .as_str()
        .and_then(|s| s.split_once('/'))
        .ok_or_else(|| unsupported("unrepresentable_configuration"))?;
    Ok(Ipv4::Static {
        address: address
            .parse()
            .map_err(|_| unsupported("unrepresentable_configuration"))?,
        prefix: prefix
            .parse()
            .map_err(|_| unsupported("unrepresentable_configuration"))?,
        gateway: gateways
            .first()
            .map(|g| {
                g.parse()
                    .map_err(|_| unsupported("unrepresentable_configuration"))
            })
            .transpose()?,
        dns: dns
            .iter()
            .map(|d| {
                d.as_str()
                    .ok_or_else(|| unsupported("unrepresentable_configuration"))?
                    .parse()
                    .map_err(|_| unsupported("unrepresentable_configuration"))
            })
            .collect::<backend::Result<Vec<_>>>()?,
    })
}

/// Effective networkd input, including append/reset semantics and default routes.
/// `source` is the network file, named when the `Domains=` list is refused.
pub fn parse_networkd_addressing(text: &str, source: &str) -> backend::Result<Addressing> {
    let mut v = json!({"addresses":[],"routes":[],"nameservers":{"addresses":[]}});
    let mut section = "";
    let mut route = serde_json::Map::new();
    let mut domains: Vec<&str> = Vec::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(['#', ';']))
    {
        if line.starts_with('[') {
            if section == "[Route]" && !route.is_empty() {
                v["routes"]
                    .as_array_mut()
                    .ok_or("routes")?
                    .push(Value::Object(std::mem::take(&mut route)));
            }
            section = line;
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| unsupported("unrepresentable_configuration"))?;
        let (key, value) = (key.trim(), value.trim());
        match (section, key) {
            // NE-3.9b (U18): DHCPv6 on its own (`yes` is both families, `ipv6` the second) is the
            // distribution's default, not operator IPv6 intent. The managed pin says
            // `DHCP=ipv4`, and the source file stays as it is. A `[DHCPv6]` section with
            // settings (the `dhcp6-overrides` of Netplan) still refuses, below.
            ("[Network]", "DHCP") => {
                if !matches!(
                    value,
                    "yes" | "true" | "ipv4" | "ipv6" | "no" | "false" | ""
                ) {
                    return Err(unsupported("unrepresentable_configuration"));
                }
                v["dhcp4"] = json!(matches!(value, "yes" | "true" | "ipv4"));
            }
            ("[DHCPv6]", _) if !value.is_empty() => {
                return Err(unsupported("ipv6_configuration"));
            }
            ("[Network]" | "[Address]", "Address") | ("[Network]", "DNS") => {
                let list = if key == "Address" {
                    &mut v["addresses"]
                } else {
                    &mut v["nameservers"]["addresses"]
                };
                let list = list.as_array_mut().ok_or("invalid address list")?;
                if value.is_empty() {
                    list.clear();
                } else {
                    list.extend(value.split_whitespace().map(|s| json!(s)));
                }
            }
            ("[Network]", "Gateway") => {
                if value.is_empty() {
                    v["routes"] = json!([]);
                } else {
                    v["routes"]
                        .as_array_mut()
                        .ok_or("routes")?
                        .push(json!({"to":"default","via":value}));
                }
            }
            ("[Route]", "Destination" | "Gateway" | "Metric") => {
                let key = match key {
                    "Destination" => "to",
                    "Gateway" => "via",
                    _ => "metric",
                };
                route.insert(
                    key.into(),
                    if key == "metric" {
                        json!(
                            value
                                .parse::<u32>()
                                .map_err(|_| unsupported("unrepresentable_configuration"))?
                        )
                    } else {
                        json!(value)
                    },
                );
            }
            ("[RoutingPolicyRule]", _) => return Err(unsupported("policy_routing")),
            ("[Route]" | "[Address]", _) if !value.is_empty() => {
                return Err(unsupported("unrepresentable_configuration"));
            }
            // NE-3.9a: not IPv6 intent. The product's managed files have no IPv6 at all.
            ("[Network]", "IPv6PrivacyExtensions") => {
                if !matches!(
                    value,
                    "" | "yes" | "true" | "no" | "false" | "prefer-public" | "kernel"
                ) {
                    return Err(unsupported("unrepresentable_configuration"));
                }
            }
            ("[Network]", "IPv6AcceptRA") if matches!(value, "yes" | "true") => {
                return Err(unsupported("ipv6_configuration"));
            }
            ("[Network]", "IPv6Address" | "IPv6SendRA" | "DHCPPrefixDelegation")
                if !matches!(value, "" | "no" | "false") =>
            {
                return Err(unsupported("ipv6_configuration"));
            }
            ("[Network]", "Bridge" | "Bond" | "VRF") if !value.is_empty() => {
                return Err(unsupported("unsupported_link_kind"));
            }
            // NE-2.12a: repeated Domains= append, an empty assignment resets.
            ("[Network]", "Domains") => {
                if value.is_empty() {
                    domains.clear();
                } else {
                    domains.extend(value.split_whitespace());
                }
            }
            ("[Network]", "DNSDefaultRoute" | "IPMasquerade" | "IPForward")
                if !matches!(value, "" | "no" | "false") =>
            {
                return Err(unsupported("unrepresentable_configuration"));
            }
            ("[Network]", "DHCPServer" | "IPProxyARP" | "IPv4ProxyARP" | "IPv6ProxyNDP")
                if !matches!(value, "" | "no" | "false") =>
            {
                return Err(unsupported("unrepresentable_configuration"));
            }
            ("[Network]", key)
                if !value.is_empty()
                    && (key.contains("Address")
                        || key.contains("Route")
                        || key.contains("Gateway")
                        || key.contains("DHCP")
                        || key.contains("DNS")
                        || key.contains("IPv6"))
                    && !matches!(
                        key,
                        "IPv6AcceptRA"
                            | "IPv6SendRA"
                            | "DHCPPrefixDelegation"
                            | "IPv6Address"
                            | "DNSDefaultRoute"
                            | "LinkLocalAddressing"
                            | "DHCPServer"
                    ) =>
            {
                return Err(unsupported("unrepresentable_configuration"));
            }
            (
                "[DHCPServer]"
                | "[DHCPServerStaticLease]"
                | "[IPv6AcceptRA]"
                | "[IPv6SendRA]"
                | "[IPv6Prefix]"
                | "[IPv6RoutePrefix]"
                | "[NextHop]"
                | "[IPv6Neighbor]"
                | "[Neighbor]",
                _,
            ) if !value.is_empty() => return Err(unsupported("unrepresentable_configuration")),
            ("[Match]", _) => (),
            ("[Link]", "RequiredForOnline") => (),
            ("[Network]", "VLAN") if !value.is_empty() => return Err(unsupported("foreign_vlan")),
            ("[Network]", "LinkLocalAddressing") if !matches!(value, "no" | "false" | "") => {
                return Err(unsupported("unrepresentable_configuration"));
            }
            (
                "[Network]",
                "IPv6AcceptRA"
                | "IPv6Address"
                | "IPv6SendRA"
                | "DHCPPrefixDelegation"
                | "DNSDefaultRoute"
                | "IPMasquerade"
                | "IPForward"
                | "DHCPServer"
                | "IPProxyARP"
                | "IPv4ProxyARP"
                | "IPv6ProxyNDP"
                | "LinkLocalAddressing",
            ) if matches!(value, "no" | "false" | "") => (),
            ("[DHCPv4]" | "[DHCP]", "RouteMetric" | "UseNTP" | "UseHostname") => (),
            ("[DHCPv4]" | "[DHCP]", "UseDNS" | "UseRoutes" | "UseGateway")
                if matches!(value, "yes" | "true" | "") => {}
            _ if !value.is_empty() => return Err(unsupported("unrepresentable_configuration")),
            _ => (),
        }
    }
    if !route.is_empty() {
        v["routes"]
            .as_array_mut()
            .ok_or("routes")?
            .push(Value::Object(route));
    }
    for route in v["routes"].as_array_mut().ok_or("routes")? {
        if route.get("to").is_none() {
            route["to"] = json!("default");
        }
    }
    Ok(Addressing {
        ipv4: parse_netplan_ipv4(&v)?,
        dns_search: adopt_search(&domains, source, "Domains")?,
    })
}

pub(crate) fn interfaces(p: &Profiles) -> Vec<String> {
    let mut names = vec![p.interface.clone()];
    names.extend(
        p.networks
            .iter()
            .filter(|n| n.vlan_id.is_some())
            .map(|n| p.ifname(n)),
    );
    names
}

pub(crate) fn glob_match(pattern: &str, name: &str) -> bool {
    // Bounded dynamic programming, no recursive/exponential wildcard matching.
    let mut matched = vec![false; name.len() + 1];
    matched[0] = true;
    let bytes = pattern.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        let class_start = cursor + 1;
        let mut class_end = None;
        if byte == b'[' {
            let mut first = class_start;
            if bytes.get(first).is_some_and(|b| matches!(*b, b'!' | b'^')) {
                first += 1;
            }
            if bytes.get(first) == Some(&b']') {
                first += 1;
            }
            class_end = bytes[first..]
                .iter()
                .position(|b| *b == b']')
                .map(|offset| first + offset);
        }
        let literal = if byte == b'\\' && cursor + 1 < bytes.len() {
            cursor += 1;
            bytes[cursor]
        } else {
            byte
        };
        let mut next = vec![false; name.len() + 1];
        if byte == b'*' {
            next[0] = matched[0];
        }
        for (i, actual) in name.bytes().enumerate() {
            let accepts = class_end.map_or(byte == b'?' || literal == actual, |end| {
                let mut class = &bytes[class_start..end];
                let negate = class.first().is_some_and(|b| matches!(*b, b'!' | b'^'));
                if negate {
                    class = &class[1..];
                }
                let mut found = false;
                while !class.is_empty() {
                    if class.len() >= 3 && class[1] == b'-' {
                        found |= (class[0]..=class[2]).contains(&actual);
                        class = &class[3..];
                    } else {
                        found |= class[0] == actual;
                        class = &class[1..];
                    }
                }
                found != negate
            });
            next[i + 1] = if byte == b'*' {
                matched[i + 1] || next[i]
            } else {
                matched[i] && accepts
            };
        }
        matched = next;
        cursor = class_end.unwrap_or(cursor) + 1;
    }
    matched[name.len()]
}
fn netplan_metric(entry: &Value, dhcp: bool) -> backend::Result<u32> {
    let values: Vec<_> = if dhcp {
        entry["dhcp4-overrides"]
            .get("route-metric")
            .into_iter()
            .collect()
    } else {
        entry["routes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r.get("metric"))
            .collect()
    };
    let mut metric = None;
    for value in values {
        let value = value
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| unsupported("unrepresentable_configuration"))?;
        if metric.is_some_and(|old| old != value) {
            return Err(unsupported("unrepresentable_configuration"));
        }
        metric = Some(value);
    }
    Ok(metric.unwrap_or(if dhcp { 100 } else { 0 }))
}
fn networkd_metric(text: &str, default: u32) -> backend::Result<u32> {
    let mut metric = default;
    let mut section = "";
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line;
        }
        if let Some((key, value)) = line.split_once('=') {
            if matches!(
                (section, key.trim()),
                ("[DHCPv4]" | "[DHCP]", "RouteMetric") | ("[Route]", "Metric")
            ) {
                metric = value
                    .trim()
                    .parse()
                    .map_err(|_| unsupported("unrepresentable_configuration"))?;
            }
        }
    }
    Ok(metric)
}

fn systemd_words(value: &str) -> backend::Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut chars = value.trim().chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let escaped = chars
                .next()
                .ok_or_else(|| unsupported("unrepresentable_configuration"))?;
            let decoded = match escaped {
                'a' => '\u{7}',
                'b' => '\u{8}',
                'f' => '\u{c}',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'v' => '\u{b}',
                's' => ' ',
                '\\' | '\'' | '"' => escaped,
                'x' | 'u' | 'U' => {
                    let count = match escaped {
                        'x' => 2,
                        'u' => 4,
                        _ => 8,
                    };
                    let digits: String = chars.by_ref().take(count).collect();
                    if digits.len() != count || !digits.bytes().all(|c| c.is_ascii_hexdigit()) {
                        return Err(unsupported("unrepresentable_configuration"));
                    }
                    char::from_u32(
                        u32::from_str_radix(&digits, 16)
                            .map_err(|_| unsupported("unrepresentable_configuration"))?,
                    )
                    .ok_or_else(|| unsupported("unrepresentable_configuration"))?
                }
                '0'..='7' => {
                    let digits = format!("{escaped}{}", chars.by_ref().take(2).collect::<String>());
                    if digits.len() != 3 {
                        return Err(unsupported("unrepresentable_configuration"));
                    }
                    char::from_u32(
                        u32::from_str_radix(&digits, 8)
                            .map_err(|_| unsupported("unrepresentable_configuration"))?,
                    )
                    .ok_or_else(|| unsupported("unrepresentable_configuration"))?
                }
                _ => return Err(unsupported("unrepresentable_configuration")),
            };
            word.push(decoded);
        } else if quote == Some(c) {
            quote = None;
        } else if quote.is_none() && matches!(c, '\'' | '"') {
            quote = Some(c);
        } else if quote.is_none() && c.is_whitespace() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(c);
        }
    }
    if quote.is_some() {
        return Err(unsupported("unrepresentable_configuration"));
    }
    if !word.is_empty() {
        words.push(word);
    }
    Ok(words)
}

fn networkd_selectors(text: &str) -> backend::Result<(Vec<String>, Vec<String>)> {
    let mut names = Vec::new();
    let mut macs = Vec::new();
    let mut section = "";
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line;
        }
        if section != "[Match]" {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let values = match key.trim() {
                "Name" => &mut names,
                "MACAddress" => &mut macs,
                _ => continue,
            };
            if value.trim().is_empty() {
                values.clear();
            } else {
                values.extend(systemd_words(value)?);
            }
        }
    }
    if names.iter().skip(1).any(|name| name.starts_with('!')) {
        return Err(unsupported("unrepresentable_configuration"));
    }
    Ok((names, macs))
}
fn selectors_match(patterns: &[String], interface: &str) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let inverted = patterns[0].starts_with('!');
    let matched = patterns.iter().enumerate().any(|(i, pattern)| {
        glob_match(
            if i == 0 && inverted {
                &pattern[1..]
            } else {
                pattern
            },
            interface,
        )
    });
    matched != inverted
}

fn sensitive(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            matches!(
                key.as_str(),
                "auth" | "wifis" | "access-points" | "private-key" | "psk" | "token"
            ) || key.contains("password")
                || key.contains("secret")
                || sensitive(value)
        }),
        Value::Array(values) => values.iter().any(sensitive),
        _ => false,
    }
}
