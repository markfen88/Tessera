//! Choosing interfaces, and looking at who else uses UDP port 5353.
//!
//! ADR 0010 says the daemon binds only to configured interfaces. When the
//! list is empty the default is the *first wired interface that has carrier*
//! (lowest interface index): not loopback, not wireless, not virtual (bridges,
//! veth, docker, tun, dummy... all live under `/sys/devices/virtual`), not
//! down. An explicit list is used as given.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

use crate::config::DiscoveryConfig;
use crate::error::Error;

/// Where the kernel lists network interfaces.
pub(crate) const SYS_CLASS_NET: &str = "/sys/class/net";
/// UDP port of mDNS.
pub(crate) const MDNS_PORT: u16 = 5353;

fn read_trim(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned())
}

/// The first wired interface with carrier below `root` (a `/sys/class/net`
/// layout), if any.
pub(crate) fn default_interface_in(root: &Path) -> Option<String> {
    let mut cands: Vec<(u32, String)> = Vec::new();
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let p = entry.path();
        if name == "lo" {
            continue;
        }
        // Virtual devices are symlinks into .../devices/virtual/...
        let virtual_dev = std::fs::read_link(&p)
            .map(|t| t.to_string_lossy().contains("/virtual/"))
            .unwrap_or(false);
        let wireless = p.join("wireless").exists() || p.join("phy80211").exists();
        let ether = read_trim(&p.join("type")).as_deref() == Some("1");
        // `carrier` cannot be read (EINVAL) while the link is administratively down.
        let carrier = read_trim(&p.join("carrier")).as_deref() == Some("1");
        if virtual_dev || wireless || !ether || !carrier {
            continue;
        }
        let idx = read_trim(&p.join("ifindex"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(u32::MAX);
        cands.push((idx, name));
    }
    cands.sort();
    cands.into_iter().next().map(|(_, n)| n)
}

/// The interface names to use: the configured ones, or the default one.
///
/// # Errors
/// [`Error::NoDefaultInterface`] if none are configured and there is no wired
/// interface with carrier.
pub(crate) fn resolve_interfaces(cfg: &DiscoveryConfig) -> Result<Vec<String>, Error> {
    resolve_interfaces_in(cfg, Path::new(SYS_CLASS_NET))
}

pub(crate) fn resolve_interfaces_in(
    cfg: &DiscoveryConfig,
    root: &Path,
) -> Result<Vec<String>, Error> {
    if !cfg.interfaces.is_empty() {
        return Ok(cfg.interfaces.clone());
    }
    default_interface_in(root)
        .map(|n| vec![n])
        .ok_or(Error::NoDefaultInterface)
}

// --- who owns :5353 --------------------------------------------------------------

/// A UDP socket bound to the mDNS port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdnsSocket {
    /// Local address (`0.0.0.0`/`::` = all interfaces).
    pub local: IpAddr,
    /// Owner user id.
    pub uid: u32,
    /// Socket inode (matches `/proc/<pid>/fd` links `socket:[inode]`).
    pub inode: u64,
}

fn word_bytes(w: u32) -> [u8; 4] {
    // /proc/net prints each 32-bit word in host byte order.
    if cfg!(target_endian = "little") {
        w.to_le_bytes()
    } else {
        w.to_be_bytes()
    }
}

fn parse_addr(hex: &str) -> Option<IpAddr> {
    match hex.len() {
        8 => Some(IpAddr::V4(Ipv4Addr::from(word_bytes(
            u32::from_str_radix(hex, 16).ok()?,
        )))),
        32 => {
            let mut b = [0u8; 16];
            for (i, chunk) in b.chunks_exact_mut(4).enumerate() {
                let w = u32::from_str_radix(hex.get(i * 8..i * 8 + 8)?, 16).ok()?;
                chunk.copy_from_slice(&word_bytes(w));
            }
            Some(IpAddr::V6(Ipv6Addr::from(b)))
        }
        _ => None,
    }
}

/// Sockets bound to port 5353 in the text of `/proc/net/udp` or `udp6`.
/// Total: lines that do not parse are skipped.
pub(crate) fn parse_udp_table(text: &str) -> Vec<MdnsSocket> {
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (addr, port) = f.get(1)?.split_once(':')?;
            if u16::from_str_radix(port, 16).ok()? != MDNS_PORT {
                return None;
            }
            Some(MdnsSocket {
                local: parse_addr(addr)?,
                uid: f.get(7)?.parse().ok()?,
                inode: f.get(9)?.parse().ok()?,
            })
        })
        .collect()
}

fn own_socket_inodes() -> HashSet<u64> {
    let mut set = HashSet::new();
    if let Ok(rd) = std::fs::read_dir("/proc/self/fd") {
        for e in rd.flatten() {
            if let Ok(t) = std::fs::read_link(e.path()) {
                let t = t.to_string_lossy();
                if let Some(n) = t
                    .strip_prefix("socket:[")
                    .and_then(|s| s.strip_suffix(']'))
                    .and_then(|s| s.parse().ok())
                {
                    set.insert(n);
                }
            }
        }
    }
    set
}

/// All sockets bound to UDP port 5353 in this network namespace.
fn all_mdns_sockets() -> Vec<MdnsSocket> {
    let mut v = Vec::new();
    for f in ["/proc/net/udp", "/proc/net/udp6"] {
        if let Ok(t) = std::fs::read_to_string(f) {
            v.extend(parse_udp_table(&t));
        }
    }
    v
}

/// Sockets of this process bound to UDP port 5353 (for tests: with Avahi
/// selected there must be none).
#[doc(hidden)]
#[must_use]
pub fn own_mdns_sockets() -> Vec<MdnsSocket> {
    let own = own_socket_inodes();
    all_mdns_sockets()
        .into_iter()
        .filter(|s| own.contains(&s.inode))
        .collect()
}

/// Sockets of other processes bound to port 5353 on an address that is
/// `0.0.0.0`/`::` or one of `addrs`.
pub(crate) fn foreign_mdns_sockets(addrs: &[IpAddr]) -> Vec<MdnsSocket> {
    let own = own_socket_inodes();
    all_mdns_sockets()
        .into_iter()
        .filter(|s| !own.contains(&s.inode))
        .filter(|s| s.local.is_unspecified() || addrs.contains(&s.local))
        .collect()
}

/// The addresses of the named interfaces.
pub(crate) fn interface_addresses(names: &[String]) -> Vec<IpAddr> {
    let Ok(addrs) = nix::ifaddrs::getifaddrs() else {
        return Vec::new();
    };
    addrs
        .filter(|a| names.contains(&a.interface_name))
        .filter_map(|a| {
            let sa = a.address?;
            sa.as_sockaddr_in()
                .map(|v| IpAddr::V4(v.ip()))
                .or_else(|| sa.as_sockaddr_in6().map(|v| IpAddr::V6(v.ip())))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    const UDP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n\
 1234: 00000000:14E9 00000000:0000 07 00000000:00000000 00:00000000 00000000   115        0 4242 2 0000000000000000 0\n\
 1235: 0100007F:0035 00000000:0000 07 00000000:00000000 00:00000000 00000000   101        0 4243 2 0000000000000000 0\n\
 1236: 7C00000A:14E9 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 4244 2 0000000000000000 0\n";

    #[test]
    fn parses_the_udp_table() {
        let v = parse_udp_table(UDP);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].local, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!((v[0].uid, v[0].inode), (115, 4242));
        assert_eq!(v[1].local, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 124)));
    }

    #[test]
    fn parses_v6_addresses() {
        let t = "h\n 1: 000080FE00000000FFFFFFFE05000000:14E9 0:0 07 0:0 0:0 0 5 0 9 2 0 0\n";
        let v = parse_udp_table(t);
        assert_eq!(v.len(), 1);
        assert!(matches!(v[0].local, IpAddr::V6(_)));
    }

    proptest! {
        #[test]
        fn the_table_parser_is_total(s in "\\PC{0,300}") {
            let _ = parse_udp_table(&s);
        }
    }

    fn fake(
        root: &Path,
        name: &str,
        virt: bool,
        wireless: bool,
        ty: &str,
        carrier: Option<&str>,
        idx: u32,
    ) {
        let dev_dir = if virt {
            root.join("devices/virtual/net").join(name)
        } else {
            root.join("devices/pci/net").join(name)
        };
        fs::create_dir_all(&dev_dir).unwrap();
        fs::write(dev_dir.join("type"), format!("{ty}\n")).unwrap();
        fs::write(dev_dir.join("ifindex"), format!("{idx}\n")).unwrap();
        if let Some(c) = carrier {
            fs::write(dev_dir.join("carrier"), format!("{c}\n")).unwrap();
        }
        if wireless {
            fs::create_dir_all(dev_dir.join("wireless")).unwrap();
        }
        fs::create_dir_all(root.join("class/net")).unwrap();
        symlink(&dev_dir, root.join("class/net").join(name)).unwrap();
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("tessera-ifaces-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn default_is_the_first_wired_interface_with_carrier() {
        let r = tmp("default");
        fake(&r, "lo", true, false, "772", Some("1"), 1);
        fake(&r, "docker0", true, false, "1", Some("1"), 2);
        fake(&r, "wlan0", false, true, "1", Some("1"), 3);
        fake(&r, "enp3s0", false, false, "1", Some("0"), 4); // no carrier
        fake(&r, "enp4s0", false, false, "1", None, 5); // down: carrier unreadable
        fake(&r, "tun0", false, false, "65534", Some("1"), 6);
        fake(&r, "enp9s0", false, false, "1", Some("1"), 9);
        fake(&r, "enp5s0", false, false, "1", Some("1"), 7);
        let root = r.join("class/net");
        assert_eq!(default_interface_in(&root).as_deref(), Some("enp5s0"));
        fs::remove_dir_all(r).unwrap();
    }

    #[test]
    fn no_default_without_a_wired_interface_and_explicit_lists_win() {
        let r = tmp("none");
        fake(&r, "veth1", true, false, "1", Some("1"), 2);
        fake(&r, "wlan0", false, true, "1", Some("1"), 3);
        let root = r.join("class/net");
        assert_eq!(default_interface_in(&root), None);
        let mut cfg = DiscoveryConfig::default();
        assert!(matches!(
            resolve_interfaces_in(&cfg, &root),
            Err(Error::NoDefaultInterface)
        ));
        cfg.interfaces = vec!["wlan0".into(), "veth1".into()];
        assert_eq!(
            resolve_interfaces_in(&cfg, &root).unwrap(),
            ["wlan0", "veth1"]
        );
        fs::remove_dir_all(r).unwrap();
    }
}
