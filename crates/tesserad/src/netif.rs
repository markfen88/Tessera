//! Network interface lookups: address, hardware address, and the default
//! media interface (ADR 0010: interfaces are named, never guessed from an
//! address).
//!
//! Everything here reads `getifaddrs` and `/sys/class/net`; nothing changes
//! the host.

use std::io;
use std::net::Ipv4Addr;
use std::path::Path;

use nix::ifaddrs::getifaddrs;
use tessera_control_types::{InterfaceStatus, NetworkStatus, PrimarySource};

/// What the daemon needs to know about an interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfInfo {
    /// Interface name.
    pub name: String,
    /// Kernel interface index. A removed and re-created interface (an
    /// adapter unplugged and plugged back in, a driver reset) gets a new one
    /// even when everything else is the same; sockets bound to the old one
    /// are dead.
    pub index: u32,
    /// First IPv4 address.
    pub addr: Ipv4Addr,
    /// Hardware address (zero if the interface has none, for example `lo`).
    pub mac: [u8; 6],
    /// Wi-Fi interface (refused for media unless `allow_wireless`).
    pub wireless: bool,
}

fn sys(name: &str, leaf: &str) -> String {
    format!("/sys/class/net/{name}/{leaf}")
}

fn read_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
}

/// Parse `aa:bb:cc:dd:ee:ff`.
#[must_use]
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut parts = s.split(':');
    for b in &mut out {
        *b = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(out)
}

fn is_wireless(name: &str) -> bool {
    Path::new(&sys(name, "wireless")).exists() || Path::new(&sys(name, "phy80211")).exists()
}

fn is_virtual(name: &str) -> bool {
    std::fs::canonicalize(sys(name, ""))
        .map(|p| p.to_string_lossy().contains("/virtual/"))
        .unwrap_or(false)
}

/// The first IPv4 address of `name`.
fn first_v4(name: &str) -> Option<Ipv4Addr> {
    getifaddrs().ok()?.find_map(|ifa| {
        if ifa.interface_name != name {
            return None;
        }
        let sin = ifa.address?.as_sockaddr_in().copied()?;
        Some(sin.ip())
    })
}

/// Look an interface up by name.
///
/// # Errors
/// `NotFound` if there is no such interface or it has no IPv4 address.
pub fn lookup(name: &str) -> io::Result<IfInfo> {
    if !Path::new(&sys(name, "")).exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no network interface `{name}`"),
        ));
    }
    let addr = first_v4(name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("interface `{name}` has no IPv4 address"),
        )
    })?;
    let mac = read_trim(&sys(name, "address"))
        .and_then(|s| parse_mac(&s))
        .unwrap_or([0; 6]);
    let index = read_trim(&sys(name, "ifindex"))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no network interface `{name}` (no index)"),
            )
        })?;
    Ok(IfInfo {
        name: name.to_owned(),
        index,
        addr,
        mac,
        wireless: is_wireless(name),
    })
}

/// One interface of the host, as a candidate for the primary (ADR 0012).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostIf {
    /// Interface name.
    pub name: String,
    /// Hardware address, if it has one.
    pub mac: Option<[u8; 6]>,
    /// First IPv4 address.
    pub ipv4: Option<Ipv4Addr>,
    /// Link up.
    pub carrier: bool,
    /// Wi-Fi.
    pub wireless: bool,
    /// Software interface (bridge, veth, tunnel, ...).
    pub is_virtual: bool,
}

impl HostIf {
    /// The automatic rule: wired, not virtual, link up, an IPv4 address.
    fn auto_candidate(&self) -> bool {
        !self.wireless && !self.is_virtual && self.carrier && self.ipv4.is_some()
    }

    /// Why this interface cannot be the primary now, if it cannot.
    fn unusable(&self, allow_wireless: bool) -> Option<&'static str> {
        if self.wireless && !allow_wireless {
            Some("Wi-Fi interface and `allow_wireless` is off")
        } else if self.ipv4.is_none() {
            Some("no IPv4 address")
        } else {
            None
        }
    }
}

/// Every interface of the host except loopback, sorted by name.
#[must_use]
pub fn scan() -> Vec<HostIf> {
    let mut names: Vec<String> = std::fs::read_dir("/sys/class/net")
        .map(|d| {
            d.filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| n != "lo")
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
        .into_iter()
        .map(|n| HostIf {
            mac: read_trim(&sys(&n, "address"))
                .and_then(|s| parse_mac(&s))
                .filter(|m| *m != [0; 6]),
            ipv4: first_v4(&n),
            carrier: read_trim(&sys(&n, "carrier")).as_deref() == Some("1"),
            wireless: is_wireless(&n),
            is_virtual: is_virtual(&n),
            name: n,
        })
        .collect()
}

/// The automatic choice: the first wired, non-virtual interface (by name)
/// with carrier and an IPv4 address.
#[must_use]
pub fn default_interface() -> Option<String> {
    scan()
        .into_iter()
        .find(HostIf::auto_candidate)
        .map(|h| h.name)
}

/// The selection inputs from the effective model and configuration.
#[derive(Debug, Clone, Copy)]
pub struct Selection<'a> {
    /// `[network] interfaces`.
    pub configured: &'a [String],
    /// The runtime selection.
    pub selected: Option<&'a str>,
    /// `[network] locked = ["primary"]`.
    pub locked: bool,
    /// `[network] allow_wireless`.
    pub allow_wireless: bool,
}

impl<'a> Selection<'a> {
    /// The inputs of a runtime plan.
    #[must_use]
    pub fn of(rp: &'a crate::engine_thread::RuntimePlan) -> Self {
        Self {
            configured: &rp.config.network.interfaces,
            selected: rp.model.network.selected.as_deref(),
            locked: rp.model.network.locked,
            allow_wireless: rp.config.network.allow_wireless,
        }
    }
}

/// The primary-interface decision (ADR 0012).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The interfaces to use, the primary first.
    pub interfaces: Vec<String>,
    /// How the primary was chosen.
    pub source: PrimarySource,
    /// Why a runtime selection is not in effect, if it is not.
    pub fallback_reason: Option<String>,
}

impl Resolution {
    /// The primary interface, if there is one.
    #[must_use]
    pub fn primary(&self) -> Option<&str> {
        self.interfaces.first().map(String::as_str)
    }
}

/// Decide the interfaces in the ADR 0012 order: lock, runtime selection
/// (if its interface is usable now), configured list, automatic choice.
#[must_use]
pub fn resolve(sel: Selection<'_>, host: &[HostIf]) -> Resolution {
    let (base, base_source) = if sel.configured.is_empty() {
        let auto = host
            .iter()
            .find(|h| h.auto_candidate())
            .map(|h| h.name.clone());
        (auto.into_iter().collect(), PrimarySource::Automatic)
    } else {
        (sel.configured.to_vec(), PrimarySource::Configured)
    };
    if sel.locked {
        return Resolution {
            interfaces: base,
            source: PrimarySource::Locked,
            fallback_reason: None,
        };
    }
    let Some(want) = sel.selected else {
        return Resolution {
            interfaces: base,
            source: base_source,
            fallback_reason: None,
        };
    };
    let why = match host.iter().find(|h| h.name == want) {
        None => Some("not present"),
        Some(h) => h.unusable(sel.allow_wireless),
    };
    match why {
        None => {
            let mut v = vec![want.to_owned()];
            v.extend(sel.configured.iter().filter(|n| *n != want).cloned());
            Resolution {
                interfaces: v,
                source: PrimarySource::Selected,
                fallback_reason: None,
            }
        }
        Some(why) => Resolution {
            interfaces: base,
            source: PrimarySource::Fallback,
            fallback_reason: Some(format!("selected interface `{want}`: {why}")),
        },
    }
}

/// The status of the decision and the candidates.
#[must_use]
pub fn network_status(r: &Resolution, sel: Selection<'_>, host: &[HostIf]) -> NetworkStatus {
    let mut n = NetworkStatus::new(r.primary().map(str::to_owned), r.source);
    n.selected = sel.selected.map(str::to_owned);
    n.locked = sel.locked;
    n.allow_wireless = sel.allow_wireless;
    n.interfaces = host
        .iter()
        .map(|h| {
            let mut i = InterfaceStatus::new(h.name.clone());
            i.mac = h.mac.map(|m| {
                m.iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(":")
            });
            i.ipv4 = h.ipv4.map(|a| a.to_string());
            i.carrier = h.carrier;
            i.wireless = h.wireless;
            i.is_virtual = h.is_virtual;
            let why = h.unusable(sel.allow_wireless);
            i.usable = why.is_none();
            i.reason = why.map(str::to_owned);
            i
        })
        .collect();
    n
}

/// The interfaces to use for a runtime plan, looking at the host now.
#[must_use]
pub fn effective_interfaces(rp: &crate::engine_thread::RuntimePlan) -> Vec<String> {
    resolve(Selection::of(rp), &scan()).interfaces
}

/// True if every interface is the loopback interface (no media network).
#[must_use]
pub fn only_loopback(names: &[String]) -> bool {
    names.iter().all(|n| n == "lo")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macs_parse_and_garbage_does_not() {
        assert_eq!(
            parse_mac("00:1a:2B:3c:4d:5e"),
            Some([0x00, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e])
        );
        for bad in [
            "",
            "00:11",
            "00:11:22:33:44:55:66",
            "zz:11:22:33:44:55",
            "1:2:3:4:5:",
        ] {
            assert_eq!(parse_mac(bad), None, "{bad}");
        }
    }

    #[test]
    fn loopback_resolves() {
        let lo = lookup("lo").unwrap();
        assert_eq!(lo.addr, Ipv4Addr::LOCALHOST);
        assert!(lo.index >= 1, "loopback has an interface index");
        assert!(!lo.wireless);
        assert!(lookup("definitely-not-an-interface").is_err());
        assert!(only_loopback(&["lo".into()]));
        assert!(!only_loopback(&["lo".into(), "eth0".into()]));
    }

    fn wired(name: &str, carrier: bool, addr: bool) -> HostIf {
        HostIf {
            name: name.into(),
            mac: Some([2, 0, 0, 0, 0, 1]),
            ipv4: addr.then(|| Ipv4Addr::new(10, 0, 0, 1)),
            carrier,
            wireless: false,
            is_virtual: false,
        }
    }

    fn host() -> Vec<HostIf> {
        let mut wifi = wired("wlan0", true, true);
        wifi.wireless = true;
        let mut br = wired("br0", true, true);
        br.is_virtual = true;
        vec![
            br,
            wired("enp1s0", false, false),
            wired("enp3s0", true, true),
            wired("enp4s0", true, true),
            wifi,
        ]
    }

    fn sel<'a>(configured: &'a [String], selected: Option<&'a str>) -> Selection<'a> {
        Selection {
            configured,
            selected,
            locked: false,
            allow_wireless: false,
        }
    }

    #[test]
    fn automatic_takes_the_first_wired_physical_interface_with_link_and_address() {
        let r = resolve(sel(&[], None), &host());
        assert_eq!(r.interfaces, vec!["enp3s0".to_owned()]);
        assert_eq!(r.source, PrimarySource::Automatic);
        assert_eq!(resolve(sel(&[], None), &[]).primary(), None);
    }

    #[test]
    fn a_configured_list_wins_over_the_automatic_choice() {
        let cfg = vec!["enp4s0".to_owned()];
        let r = resolve(sel(&cfg, None), &host());
        assert_eq!(r.interfaces, cfg);
        assert_eq!(r.source, PrimarySource::Configured);
    }

    #[test]
    fn a_usable_selection_goes_first_and_keeps_the_other_configured_ports() {
        let cfg = vec!["enp3s0".to_owned(), "enp4s0".to_owned()];
        let r = resolve(sel(&cfg, Some("enp4s0")), &host());
        assert_eq!(r.interfaces, vec!["enp4s0".to_owned(), "enp3s0".to_owned()]);
        assert_eq!(r.source, PrimarySource::Selected);
        assert_eq!(r.fallback_reason, None);
    }

    #[test]
    fn an_unusable_selection_falls_back_and_says_why() {
        for (want, why) in [
            ("eth9", "not present"),
            ("enp1s0", "no IPv4 address"),
            ("wlan0", "Wi-Fi"),
        ] {
            let r = resolve(sel(&[], Some(want)), &host());
            assert_eq!(r.interfaces, vec!["enp3s0".to_owned()], "{want}");
            assert_eq!(r.source, PrimarySource::Fallback);
            assert!(r.fallback_reason.unwrap().contains(why), "{want}");
        }
        let mut s = sel(&[], Some("wlan0"));
        s.allow_wireless = true;
        assert_eq!(resolve(s, &host()).primary(), Some("wlan0"));
    }

    #[test]
    fn a_lock_ignores_the_selection() {
        let mut s = sel(&[], Some("enp4s0"));
        s.locked = true;
        let r = resolve(s, &host());
        assert_eq!(r.primary(), Some("enp3s0"));
        assert_eq!(r.source, PrimarySource::Locked);
    }

    #[test]
    fn the_status_lists_every_candidate_with_the_reason() {
        let s = sel(&[], Some("enp4s0"));
        let h = host();
        let n = network_status(&resolve(s, &h), s, &h);
        assert_eq!(n.primary.as_deref(), Some("enp4s0"));
        assert_eq!(n.selected.as_deref(), Some("enp4s0"));
        assert_eq!(n.interfaces.len(), 5);
        let wifi = n.interfaces.iter().find(|i| i.name == "wlan0").unwrap();
        assert!(!wifi.usable && wifi.wireless && wifi.reason.is_some());
        let e = n.interfaces.iter().find(|i| i.name == "enp3s0").unwrap();
        assert!(e.usable && e.carrier);
        assert_eq!(e.mac.as_deref(), Some("02:00:00:00:00:01"));
        assert_eq!(e.ipv4.as_deref(), Some("10.0.0.1"));
    }
}
