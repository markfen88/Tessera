//! Authority-supplied complete network facts. Gathering these facts, pinning the
//! actual egress/source and proving their namespace/interface incarnation remain
//! production obligations. Canonical content is the version; lease expiry times
//! are intentionally absent. A changed snapshot closes the role, never refreshes it.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Prefix {
    network: Ipv4Addr,
    length: u8,
}
impl Prefix {
    fn new(address: Ipv4Addr, length: u8) -> Option<Self> {
        if length > 32 {
            return None;
        }
        let mask = u32::MAX.checked_shl(32 - u32::from(length)).unwrap_or(0);
        Some(Self {
            network: (u32::from(address) & mask).into(),
            length,
        })
    }
    fn contains(self, address: Ipv4Addr) -> bool {
        Self::new(address, self.length).is_some_and(|p| p.network == self.network)
    }
    fn overlaps(self, other: Self) -> bool {
        self.contains(other.network) || other.contains(self.network)
    }
    fn broadcast(self) -> Ipv4Addr {
        let mask = u32::MAX
            .checked_shl(32 - u32::from(self.length))
            .unwrap_or(0);
        (u32::from(self.network) | !mask).into()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RouteKind {
    Unicast,
    Multicast,
    Local,
    Broadcast,
    Other,
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Route {
    table: u32,
    destination: Prefix,
    output_index: u32,
    preferred_source: Ipv4Addr,
    gateway: Option<Ipv4Addr>,
    link_scope: bool,
    kernel: bool,
    kind: RouteKind,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Rule {
    priority: u32,
    table: u32,
    // A fact gatherer must retain non-canonical rules rather than discarding
    // selectors, actions or suppress options into a (priority, table) pair.
    unconditional_lookup: bool,
}
fn standard_rules() -> BTreeSet<Rule> {
    [
        Rule {
            priority: 0,
            table: 255,
            unconditional_lookup: true,
        },
        Rule {
            priority: 32766,
            table: 254,
            unconditional_lookup: true,
        },
        Rule {
            priority: 32767,
            table: 253,
            unconditional_lookup: true,
        },
    ]
    .into()
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NetworkFacts {
    pub(super) binding: guards::Binding,
    namespace: u128,
    interface_index: u32,
    interface_incarnation: u128,
    // All effective local addresses in this namespace, including secondaries
    // and addresses on interfaces other than the registered audio interface.
    local_addresses: BTreeSet<Ipv4Addr>,
    connected_prefixes: BTreeSet<Prefix>,
    configured_broadcasts: BTreeSet<Ipv4Addr>,
    routes: BTreeSet<Route>,
    rules: BTreeSet<Rule>,
    complete: bool,
    egress_and_source_pinned: bool,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct TxPolicy {
    opt_in_multicast: bool,
    multicast_groups: BTreeSet<Ipv4Addr>,
}
impl NetworkFacts {
    pub(super) fn supports(&self, binding: &guards::Binding) -> bool {
        self.binding == *binding
            && binding.valid()
            && self.complete
            && self.namespace != 0
            && self.interface_index != 0
            && self.interface_incarnation != 0
            && self.egress_and_source_pinned
            && self.local_addresses.contains(&binding.address)
            && self.rules == standard_rules()
            && self.routes.iter().filter(|r| r.table == 255).all(|r| {
                r.kernel
                    && r.gateway.is_none()
                    && matches!(r.kind, RouteKind::Local | RouteKind::Broadcast)
            })
            && self
                .connected_prefixes
                .iter()
                .any(|p| p.contains(binding.address))
    }
    fn direct_route(&self, peer: Ipv4Addr, multicast: bool) -> bool {
        // Only the exact standard rules are supported. Linux consults tables
        // in rule order, then chooses the longest prefix within that table.
        // An earlier unsupported route is a refusal, never a table miss.
        for rule in &self.rules {
            let matching: Vec<_> = self
                .routes
                .iter()
                .filter(|r| r.table == rule.table && r.destination.contains(peer))
                .collect();
            let Some(longest) = matching.iter().map(|r| r.destination.length).max() else {
                continue;
            };
            let selected: Vec<_> = matching
                .into_iter()
                .filter(|r| r.destination.length == longest)
                .collect();
            // Multiple routes can imply ECMP or an unsupported tie.
            if selected.len() != 1 {
                return false;
            }
            let route = selected[0];
            return route.kernel
                && route.link_scope
                && route.gateway.is_none()
                && route.output_index == self.interface_index
                && route.preferred_source == self.binding.address
                && route.kind
                    == if multicast {
                        RouteKind::Multicast
                    } else {
                        RouteKind::Unicast
                    };
        }
        false
    }
    pub(super) fn allows(&self, peer: Ipv4Addr, policy: &TxPolicy, device_multicast: bool) -> bool {
        let octets = peer.octets();
        if !self.supports(&self.binding)
            || octets[0] == 0
            || peer.is_loopback()
            || peer.is_link_local()
            || peer.is_broadcast()
            || octets[0] >= 240
            || self.local_addresses.contains(&peer)
            || self.configured_broadcasts.contains(&peer)
            || self.connected_prefixes.iter().any(|prefix| {
                prefix.length <= 30 && (peer == prefix.network || peer == prefix.broadcast())
            })
        {
            return false;
        }
        if peer.is_multicast() {
            let forbidden = (octets[0] == 224 && octets[1] == 0 && octets[2] <= 1)
                || octets[0] == 232
                || peer == Ipv4Addr::new(239, 255, 255, 250);
            return !forbidden
                && policy.opt_in_multicast
                && device_multicast
                && policy.multicast_groups.contains(&peer)
                && self.direct_route(peer, true);
        }
        let on_link = self.connected_prefixes.iter().any(|prefix| {
            prefix.contains(self.binding.address) && prefix.contains(peer)
                // /31 has two point-to-point hosts. /32 contains only the local
                // address, which was excluded above; it grants no remote peer.
                // A user route at least as specific as this connected prefix
                // makes its direct-only premise unknown, even for other peers.
                && !self.routes.iter().any(|route| !route.kernel
                    && route.destination.length >= prefix.length && route.destination.overlaps(*prefix))
        });
        on_link && self.direct_route(peer, false)
    }

    /// Explicit fixture facts, never a product-side inferred /24.
    pub(super) fn fixture(binding: &guards::Binding) -> Self {
        let prefix = Prefix::new(binding.address, 24).unwrap();
        Self {
            binding: binding.clone(),
            namespace: 1,
            interface_index: 2,
            interface_incarnation: 1,
            local_addresses: [binding.address, Ipv4Addr::LOCALHOST].into(),
            connected_prefixes: [prefix].into(),
            configured_broadcasts: [prefix.broadcast()].into(),
            routes: [
                Route {
                    table: 254,
                    destination: prefix,
                    output_index: 2,
                    preferred_source: binding.address,
                    gateway: None,
                    link_scope: true,
                    kernel: true,
                    kind: RouteKind::Unicast,
                },
                Route {
                    table: 255,
                    destination: Prefix::new(Ipv4Addr::LOCALHOST, 8).unwrap(),
                    output_index: 1,
                    preferred_source: Ipv4Addr::LOCALHOST,
                    gateway: None,
                    link_scope: false,
                    kernel: true,
                    kind: RouteKind::Local,
                },
                Route {
                    table: 255,
                    destination: Prefix::new(binding.address, 32).unwrap(),
                    output_index: 2,
                    preferred_source: binding.address,
                    gateway: None,
                    link_scope: false,
                    kernel: true,
                    kind: RouteKind::Local,
                },
                Route {
                    table: 255,
                    destination: Prefix::new(prefix.broadcast(), 32).unwrap(),
                    output_index: 2,
                    preferred_source: binding.address,
                    gateway: None,
                    link_scope: true,
                    kernel: true,
                    kind: RouteKind::Broadcast,
                },
            ]
            .into(),
            rules: standard_rules(),
            complete: true,
            egress_and_source_pinned: true,
        }
    }
}
