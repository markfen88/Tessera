//! Pure helpers of the host router (`docs/plans/2026-11-22-host-router.md`):
//! loop detection through taps, the follow-binding graph, port names. The
//! saved grid, its validation against the devices and bridges, and the
//! commands are in the engine (`engine/router.rs`).

use std::collections::{BTreeMap, BTreeSet};

use tessera_control_types::{ControlRef, CrosspointKey, RouterPort};

/// The first crosspoint that closes a loop, if the crosspoints have one.
///
/// A crosspoint whose source is a destination port (a tap) makes an edge
/// from that destination to its own destination; the grid must have no
/// cycle along those edges, or a signal would feed itself. A crosspoint from
/// a port to itself is the shortest loop.
#[must_use]
pub fn find_loop<'a>(
    crosspoints: impl IntoIterator<Item = &'a CrosspointKey>,
) -> Option<CrosspointKey> {
    let mut edges: BTreeMap<&RouterPort, Vec<&CrosspointKey>> = BTreeMap::new();
    for x in crosspoints {
        if x.source == x.destination {
            return Some(x.clone());
        }
        if x.source.is_destination() {
            edges.entry(&x.source).or_default().push(x);
        }
    }
    // Depth-first search with colours: 1 on the stack, 2 done.
    let mut colour: BTreeMap<&RouterPort, u8> = BTreeMap::new();
    let starts: Vec<&RouterPort> = edges.keys().copied().collect();
    for start in starts {
        if colour.contains_key(start) {
            continue;
        }
        let mut stack: Vec<(&RouterPort, usize)> = vec![(start, 0)];
        colour.insert(start, 1);
        while let Some((node, next)) = stack.pop() {
            let out = edges.get(node).map_or(&[][..], Vec::as_slice);
            if let Some(&x) = out.get(next) {
                stack.push((node, next + 1));
                match colour.get(&x.destination) {
                    Some(1) => return Some(x.clone()),
                    Some(_) => {}
                    None => {
                        colour.insert(&x.destination, 1);
                        stack.push((&x.destination, 0));
                    }
                }
            } else {
                colour.insert(node, 2);
            }
        }
    }
    None
}

/// The first `follow` link that closes a cycle of controls driving each
/// other, if any. A pair of controls following each other (feedback both
/// ways) is one link, not a cycle; a control following itself is a cycle.
///
/// Links are treated as **undirected** on purpose: a followed control
/// answers with feedback (a motorized fader moves, a light changes, and the
/// desk echoes the new position back), so a value travels both ways along
/// every link whatever its direction. A triangle `a → b → c → a`, or even
/// `a → b`, `a → c`, `b → c`, is therefore a loop that could keep
/// re-sending a value around; only a tree of links (pairs included) is
/// safe. The caller passes the links of enabled surfaces only.
#[must_use]
pub fn find_follow_cycle<'a>(
    links: impl IntoIterator<Item = (&'a ControlRef, &'a ControlRef)>,
) -> Option<(ControlRef, ControlRef)> {
    // Union-find over the undirected links, each pair counted once.
    let mut parent: BTreeMap<ControlRef, ControlRef> = BTreeMap::new();
    fn root(parent: &mut BTreeMap<ControlRef, ControlRef>, c: &ControlRef) -> ControlRef {
        let mut r = c.clone();
        while let Some(p) = parent.get(&r) {
            if *p == r {
                break;
            }
            r = p.clone();
        }
        r
    }
    let mut seen: BTreeSet<(ControlRef, ControlRef)> = BTreeSet::new();
    for (from, to) in links {
        if from == to {
            return Some((from.clone(), to.clone()));
        }
        let pair = if from < to {
            (from.clone(), to.clone())
        } else {
            (to.clone(), from.clone())
        };
        if !seen.insert(pair) {
            continue;
        }
        parent.entry(from.clone()).or_insert_with(|| from.clone());
        parent.entry(to.clone()).or_insert_with(|| to.clone());
        let (a, b) = (root(&mut parent, from), root(&mut parent, to));
        if a == b {
            return Some((from.clone(), to.clone()));
        }
        parent.insert(a, b);
    }
    None
}

/// The card id an ALSA PCM names (`hw:CARD=CQ,DEV=0` and `hw:CQ,0` give
/// `CQ`), for default port labels.
#[must_use]
pub fn card_name(pcm: &str) -> &str {
    let Some((_, args)) = pcm.split_once(':') else {
        return pcm;
    };
    let first = args.split(',').next().unwrap_or("").trim();
    let first = first.strip_prefix("CARD=").unwrap_or(first);
    if first.is_empty() { pcm } else { first }
}

/// A port in words, for messages (`card_in hw:CARD=CQ,DEV=0 15`,
/// `host_out main 1`).
#[must_use]
pub fn describe(port: &RouterPort) -> String {
    match (port.device(), port.card()) {
        (Some(d), _) => format!("{} {} {}", port.kind(), d.0, port.channel()),
        (None, Some(c)) => format!("{} {c} {}", port.kind(), port.channel()),
        (None, None) => format!("{} {}", port.kind(), port.channel()),
    }
}

/// A crosspoint in words (`card_in hw:CARD=CQ,DEV=0 15 → host_out main 1`).
#[must_use]
pub fn describe_key(x: &CrosspointKey) -> String {
    format!("{} → {}", describe(&x.source), describe(&x.destination))
}

/// The label a port gets when none is saved (local ports; network ports use
/// the channel labels): `CQ in 15`, `CQ out 3`, `App 1`, `Out 1`.
#[must_use]
pub fn default_label(port: &RouterPort) -> String {
    let n = port.channel();
    match port {
        RouterPort::CardIn { card, .. } => format!("{} in {n}", card_name(card)),
        RouterPort::CardOut { card, .. } => format!("{} out {n}", card_name(card)),
        RouterPort::AppOut { .. } => format!("App {n}"),
        RouterPort::HostOut { .. } => format!("Out {n}"),
        RouterPort::NetworkRx { .. } => format!("RX {n}"),
        RouterPort::NetworkTx { .. } => format!("TX {n}"),
        _ => format!("{} {n}", port.kind()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_control_types::{Channel, DeviceKey};

    fn ch(n: u16) -> Channel {
        Channel::new(n).unwrap()
    }
    fn host(n: u16) -> RouterPort {
        RouterPort::HostOut {
            device: DeviceKey("main".into()),
            channel: ch(n),
        }
    }
    fn tx(n: u16) -> RouterPort {
        RouterPort::NetworkTx {
            device: DeviceKey("main".into()),
            channel: ch(n),
        }
    }
    fn card_out(n: u16) -> RouterPort {
        RouterPort::CardOut {
            card: "hw:CARD=X,DEV=0".into(),
            channel: ch(n),
        }
    }
    fn x(source: RouterPort, destination: RouterPort) -> CrosspointKey {
        CrosspointKey::new(source, destination)
    }

    #[test]
    fn taps_in_a_chain_are_fine_and_a_cycle_is_found() {
        let rx = RouterPort::NetworkRx {
            device: DeviceKey("main".into()),
            channel: ch(1),
        };
        let chain = [
            x(rx.clone(), host(1)),
            x(tx(1), host(1)),
            x(host(1), card_out(1)),
        ];
        assert_eq!(find_loop(&chain), None);
        let mut looped = chain.to_vec();
        looped.push(x(card_out(1), tx(1)));
        let found = find_loop(&looped).expect("a loop");
        assert!(found.source.is_destination());
        assert_eq!(find_loop(&[x(host(2), host(2))]), Some(x(host(2), host(2))));
    }

    #[test]
    fn a_follow_pair_is_one_link_and_a_triangle_is_a_cycle() {
        let c = |s: &str| ControlRef::new(s, "f1");
        let (a, b, d) = (c("a"), c("b"), c("d"));
        assert_eq!(find_follow_cycle([(&a, &b), (&b, &a)]), None);
        assert_eq!(find_follow_cycle([(&a, &b), (&a, &d)]), None);
        assert!(find_follow_cycle([(&a, &b), (&b, &d), (&d, &a)]).is_some());
        assert!(find_follow_cycle([(&a, &a)]).is_some());
    }

    #[test]
    fn names_and_labels() {
        assert_eq!(card_name("hw:CARD=CQ,DEV=0"), "CQ");
        assert_eq!(card_name("hw:CQ,0"), "CQ");
        assert_eq!(card_name("CQ"), "CQ");
        let p = RouterPort::CardIn {
            card: "hw:CARD=CQ,DEV=0".into(),
            channel: ch(15),
        };
        assert_eq!(default_label(&p), "CQ in 15");
        assert_eq!(describe(&host(1)), "host_out main 1");
        assert_eq!(
            describe_key(&x(p, host(1))),
            "card_in hw:CARD=CQ,DEV=0 15 → host_out main 1"
        );
    }
}
