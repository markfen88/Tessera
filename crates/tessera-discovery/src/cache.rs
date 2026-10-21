//! Sans-IO merge of what backends report into [`BrowseEvent`]s.
//!
//! A backend reports "source `s` currently sees instance `i` with this
//! host/port/TXT/addresses" and "source `s` no longer sees `i`". A source is
//! whatever the backend distinguishes (for Avahi: interface, protocol and
//! resolver address family; for the built-in backend: one source). The cache
//! merges the views of all sources of an instance, turns changes into
//! appear/update/disappear events and never emits an event when nothing
//! observable changed.
//!
//! The number of tracked instances per kind is capped: the data comes from the
//! LAN and is untrusted (ADR 0010).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::IpAddr;

use crate::model::{BrowseEvent, ServiceId, ServiceInstance, ServiceKind};
use crate::txt::TxtRecord;

/// Opaque identifier of one place an instance was seen.
pub(crate) type SourceKey = String;

/// What one source knows about an instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceView {
    pub host: String,
    pub port: u16,
    pub addresses: BTreeSet<IpAddr>,
    pub txt: TxtRecord,
}

#[derive(Debug)]
struct Entry {
    id: ServiceId,
    sources: BTreeMap<SourceKey, SourceView>,
    published: Option<ServiceInstance>,
}

impl Entry {
    fn merged(&self) -> Option<ServiceInstance> {
        let first = self.sources.values().next()?;
        let addresses: BTreeSet<IpAddr> = self
            .sources
            .values()
            .flat_map(|v| v.addresses.iter().copied())
            .collect();
        Some(ServiceInstance {
            id: self.id.clone(),
            host: first.host.clone(),
            port: first.port,
            addresses: addresses.into_iter().collect(),
            txt: first.txt.clone(),
        })
    }
}

/// See the module docs.
#[derive(Debug)]
pub(crate) struct BrowseCache {
    max_per_kind: usize,
    entries: HashMap<(ServiceKind, String), Entry>,
    per_kind: HashMap<ServiceKind, usize>,
    dropped: u64,
}

fn key(id: &ServiceId) -> (ServiceKind, String) {
    // DNS folds ASCII case only.
    (id.kind, id.instance.to_ascii_lowercase())
}

impl BrowseCache {
    pub(crate) fn new(max_per_kind: usize) -> Self {
        Self {
            max_per_kind,
            entries: HashMap::new(),
            per_kind: HashMap::new(),
            dropped: 0,
        }
    }

    /// Instances that were refused because of the cap.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Currently published instances, sorted.
    pub(crate) fn current(&self) -> Vec<ServiceInstance> {
        let mut v: Vec<_> = self
            .entries
            .values()
            .filter_map(|e| e.published.clone())
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// `source` reports `view` for `id`.
    pub(crate) fn on_resolved(
        &mut self,
        id: ServiceId,
        source: SourceKey,
        view: SourceView,
    ) -> Option<BrowseEvent> {
        let k = key(&id);
        if !self.entries.contains_key(&k) {
            let n = self.per_kind.entry(id.kind).or_insert(0);
            if *n >= self.max_per_kind {
                self.dropped += 1;
                return None;
            }
            *n += 1;
            self.entries.insert(
                k.clone(),
                Entry {
                    id,
                    sources: BTreeMap::new(),
                    published: None,
                },
            );
        }
        let e = self.entries.get_mut(&k)?;
        e.sources.insert(source, view);
        Self::publish(e)
    }

    /// `source` no longer sees `id`.
    pub(crate) fn on_removed(&mut self, id: &ServiceId, source: &str) -> Option<BrowseEvent> {
        let k = key(id);
        let e = self.entries.get_mut(&k)?;
        e.sources.remove(source);
        if e.sources.is_empty() {
            let e = self.entries.remove(&k)?;
            if let Some(n) = self.per_kind.get_mut(&e.id.kind) {
                *n = n.saturating_sub(1);
            }
            return e.published.map(|_| BrowseEvent::Disappeared(e.id));
        }
        Self::publish(e)
    }

    fn publish(e: &mut Entry) -> Option<BrowseEvent> {
        let merged = e.merged()?;
        match &e.published {
            None => {
                e.published = Some(merged.clone());
                Some(BrowseEvent::Appeared(merged))
            }
            Some(prev) if *prev != merged => {
                e.published = Some(merged.clone());
                Some(BrowseEvent::Updated(merged))
            }
            Some(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::net::Ipv4Addr;

    fn id(i: &str) -> ServiceId {
        ServiceId::new(ServiceKind::Arc, i)
    }

    fn view(port: u16, last: u8) -> SourceView {
        SourceView {
            host: "h.local.".into(),
            port,
            addresses: [IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))].into(),
            txt: TxtRecord::new(),
        }
    }

    #[test]
    fn appear_update_disappear() {
        let mut c = BrowseCache::new(10);
        assert!(matches!(
            c.on_resolved(id("a"), "s".into(), view(1, 1)),
            Some(BrowseEvent::Appeared(_))
        ));
        assert_eq!(c.on_resolved(id("a"), "s".into(), view(1, 1)), None);
        assert!(matches!(
            c.on_resolved(id("a"), "s".into(), view(2, 1)),
            Some(BrowseEvent::Updated(i)) if i.port == 2
        ));
        assert_eq!(
            c.on_removed(&id("a"), "s"),
            Some(BrowseEvent::Disappeared(id("a")))
        );
        assert_eq!(c.on_removed(&id("a"), "s"), None);
        assert!(c.current().is_empty());
    }

    #[test]
    fn sources_are_merged_and_disappear_when_last_goes() {
        let mut c = BrowseCache::new(10);
        c.on_resolved(id("a"), "s1".into(), view(1, 1));
        let ev = c.on_resolved(id("A"), "s2".into(), view(1, 2));
        match ev {
            Some(BrowseEvent::Updated(i)) => assert_eq!(i.addresses.len(), 2),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            c.on_removed(&id("a"), "s1"),
            Some(BrowseEvent::Updated(i)) if i.addresses.len() == 1
        ));
        assert!(matches!(
            c.on_removed(&id("a"), "s2"),
            Some(BrowseEvent::Disappeared(_))
        ));
    }

    #[test]
    fn cap_drops_and_counts() {
        let mut c = BrowseCache::new(2);
        for n in 0..5 {
            c.on_resolved(id(&format!("i{n}")), "s".into(), view(1, 1));
        }
        assert_eq!(c.current().len(), 2);
        assert_eq!(c.dropped(), 3);
        // Known instances still update, and room is freed on removal.
        assert!(c.on_resolved(id("i0"), "s".into(), view(9, 1)).is_some());
        c.on_removed(&id("i0"), "s");
        assert!(matches!(
            c.on_resolved(id("new"), "s".into(), view(1, 1)),
            Some(BrowseEvent::Appeared(_))
        ));
    }

    #[derive(Debug, Clone)]
    enum Op {
        Resolve(u8, u8, u16, u8),
        Remove(u8, u8),
    }

    fn arb_ops() -> impl Strategy<Value = Vec<Op>> {
        proptest::collection::vec(
            prop_oneof![
                (0u8..6, 0u8..3, 0u16..3, 0u8..3).prop_map(|(i, s, p, a)| Op::Resolve(i, s, p, a)),
                (0u8..6, 0u8..3).prop_map(|(i, s)| Op::Remove(i, s)),
            ],
            0..80,
        )
    }

    proptest! {
        /// Replaying the emitted events reconstructs exactly the cache state,
        /// and the cap holds.
        #[test]
        fn event_stream_reconstructs_state(ops in arb_ops(), cap in 1usize..5) {
            let mut c = BrowseCache::new(cap);
            let mut replay: HashMap<ServiceId, ServiceInstance> = HashMap::new();
            for op in ops {
                let ev = match op {
                    Op::Resolve(i, s, p, a) => c.on_resolved(id(&format!("i{i}")), format!("s{s}"), view(p, a)),
                    Op::Remove(i, s) => c.on_removed(&id(&format!("i{i}")), &format!("s{s}")),
                };
                match ev {
                    Some(BrowseEvent::Appeared(i)) => { prop_assert!(replay.insert(i.id.clone(), i).is_none()); }
                    Some(BrowseEvent::Updated(i)) => { prop_assert!(replay.insert(i.id.clone(), i).is_some()); }
                    Some(BrowseEvent::Disappeared(i)) => { prop_assert!(replay.remove(&i).is_some()); }
                    Some(BrowseEvent::Overflow { .. }) => prop_assert!(false, "the cache never reports overflow"),
                    None => {}
                }
                let mut want: Vec<_> = replay.values().cloned().collect();
                want.sort_by(|a, b| a.id.cmp(&b.id));
                prop_assert_eq!(want, c.current());
                prop_assert!(replay.len() <= cap);
            }
        }
    }
}
