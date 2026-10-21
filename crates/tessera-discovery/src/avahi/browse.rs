//! Browsing through Avahi: a service browser per kind and interface, a
//! resolver per found instance and address family. A resolver's `Found`
//! signal repeats when the records change, which becomes an update event.
//!
//! Properties of the D-Bus side that shape this code:
//!
//! * Avahi starts emitting signals as soon as a browser or resolver object
//!   exists, possibly before a client could subscribe to that object's path.
//!   The `ItemNew`/`ItemRemove` stream is therefore created from a path-less
//!   match rule *before* the browser object is created. The `Found` stream is
//!   created after it, from the client's path namespace (`/Client<N>`, taken
//!   from the browser's path), and before any resolver exists.
//! * `zbus` delivers each message to every matching stream and stops reading
//!   the socket while one stream's queue (64) is full. A task that waits for a
//!   method reply while its streams fill up would therefore deadlock the
//!   connection. The loop that reads the streams never awaits a D-Bus call:
//!   resolvers are created and freed by a second task, which hands the new
//!   objects back through a channel. Browsing uses its own connection so that
//!   it cannot hold up the responder either.
//! * `avahi-daemon` limits the objects one client may hold (see the crate
//!   docs for the observed limit). A failed resolver creation releases the
//!   item's slot and is counted (`Browser::resolve_failures`).
//! * `avahi-daemon` can stop or restart. The task notices through the owner
//!   watch, reports everything it knew as gone, and after the daemon is back
//!   opens a fresh browser (the old objects and the `/Client<N>` namespace are
//!   gone with the old daemon).

use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
use zbus::proxy::CacheProperties;
use zbus::{Message, MessageStream};

use super::proxies::{
    PROTO_INET, PROTO_INET6, SERVER_RUNNING, ServerProxy, ServiceBrowser, ServiceResolver,
};
use super::{Owner, bus_err, ifindexes, protocol, signal_rule, signal_stream};
use crate::BrowseHandle;
use crate::cache::SourceView;
use crate::config::{DiscoveryConfig, IpFamilies};
use crate::error::Error;
use crate::model::{BrowseEvent, ServiceId, ServiceKind};
use crate::names::sanitize;
use crate::sink::BrowseShared;
use crate::txt::TxtRecord;

const BROWSER_IFACE: &str = "org.freedesktop.Avahi.ServiceBrowser";
const RESOLVER_IFACE: &str = "org.freedesktop.Avahi.ServiceResolver";
/// How long shutting a browse session down waits for pending resolver requests.
const CLEANUP_WAIT: Duration = Duration::from_secs(3);
/// Warnings about failed resolutions are logged at most this often.
const WARN_EVERY: Duration = Duration::from_secs(30);
/// How long to wait for a restarted daemon before giving up one attempt.
const READY_WAIT: Duration = Duration::from_secs(15);

/// Address families to resolve for each found instance.
fn aprotocols(f: IpFamilies) -> Vec<i32> {
    match f {
        IpFamilies::V4 => vec![PROTO_INET],
        IpFamilies::V6 => vec![PROTO_INET6],
        IpFamilies::Both => vec![PROTO_INET, PROTO_INET6],
    }
}

async fn server_proxy(conn: &zbus::Connection) -> Result<ServerProxy<'static>, Error> {
    ServerProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(bus_err)
}

pub(super) async fn start(
    conn: &zbus::Connection,
    cfg: &Arc<DiscoveryConfig>,
    kinds: &[ServiceKind],
    owner: watch::Receiver<Owner>,
    shutdown: watch::Receiver<bool>,
) -> Result<(mpsc::Receiver<BrowseEvent>, BrowseHandle), Error> {
    let server = server_proxy(conn).await?;
    let idxs = ifindexes(cfg)?;
    let (shared, view, rx) = BrowseShared::new(cfg.max_instances_per_kind, cfg.event_capacity);
    let (stop_tx, stop_rx) = watch::channel(false);
    for &kind in kinds {
        for &interface in &idxs {
            let task = Arc::new(Task {
                conn: conn.clone(),
                server: server.clone(),
                kind,
                interface,
                protocol: protocol(cfg.ip_families),
                aprotocols: aprotocols(cfg.ip_families),
                cap: cfg.max_instances_per_kind,
                shared: shared.clone(),
            });
            match task.open().await {
                Ok(opened) => {
                    tokio::spawn(task.run(
                        opened,
                        stop_rx.clone(),
                        shutdown.clone(),
                        owner.clone(),
                    ));
                }
                Err(e) => {
                    let _ = stop_tx.send(true);
                    return Err(e);
                }
            }
        }
    }
    // Only the tasks hold producers from here on: when they have all ended
    // (browser dropped, backend shut down) the Browser's stream ends.
    drop(shared);
    let handle = BrowseHandle::new(
        view,
        Box::new(move || {
            // The tasks free their daemon-side objects when they see this.
            let _ = stop_tx.send(true);
        }),
    );
    Ok((rx, handle))
}

struct Task {
    conn: zbus::Connection,
    server: ServerProxy<'static>,
    kind: ServiceKind,
    interface: i32,
    protocol: i32,
    aprotocols: Vec<i32>,
    cap: usize,
    shared: BrowseShared,
}

struct Opened {
    task: Arc<Task>,
    browser: ServiceBrowser,
    items: MessageStream,
    found: MessageStream,
}

impl Task {
    async fn open(self: &Arc<Self>) -> Result<Opened, Error> {
        // Item stream first, browser object second (see the module docs).
        let items = signal_stream(&self.conn, signal_rule(BROWSER_IFACE, None)?).await?;
        let path = self
            .server
            .service_browser_new(self.interface, self.protocol, self.kind.type_label(), "", 0)
            .await
            .map_err(bus_err)?;
        // `/Client<N>/ServiceBrowser<M>`: resolvers of this connection live
        // under the same `/Client<N>`.
        let client = path
            .as_str()
            .split('/')
            .nth(1)
            .map(|c| format!("/{c}"))
            .ok_or_else(|| bus_err("unexpected Avahi object path"))?;
        let found = signal_stream(&self.conn, signal_rule(RESOLVER_IFACE, Some(&client))?).await?;
        let browser = ServiceBrowser::new(&self.conn, path);
        Ok(Opened {
            task: Arc::clone(self),
            browser,
            items,
            found,
        })
    }

    /// Runs sessions until stopped, opening a new one after each restart of
    /// the daemon.
    async fn run(
        self: Arc<Self>,
        first: Opened,
        mut stop: watch::Receiver<bool>,
        mut shutdown: watch::Receiver<bool>,
        mut owner: watch::Receiver<Owner>,
    ) {
        let mut next = Some(first);
        loop {
            let opened = match next.take() {
                Some(o) => o,
                None => match self.reopen(&mut stop, &mut shutdown, &mut owner).await {
                    Some(o) => o,
                    None => return,
                },
            };
            if let End::Stop = opened.session(&mut stop, &mut shutdown, &mut owner).await {
                return;
            }
        }
    }

    /// Waits for the daemon and opens a new browser; `None` when stopped.
    async fn reopen(
        self: &Arc<Self>,
        stop: &mut watch::Receiver<bool>,
        shutdown: &mut watch::Receiver<bool>,
        owner: &mut watch::Receiver<Owner>,
    ) -> Option<Opened> {
        loop {
            // Wait until the daemon is on the bus.
            loop {
                if *stop.borrow() || *shutdown.borrow() {
                    return None;
                }
                if owner.borrow_and_update().present {
                    break;
                }
                tokio::select! {
                    _ = stop.changed() => return None,
                    _ = shutdown.changed() => return None,
                    r = owner.changed() => if r.is_err() { return None },
                }
            }
            let generation = owner.borrow().generation;
            let deadline = Instant::now() + READY_WAIT;
            let ready = loop {
                if matches!(self.server.get_state().await, Ok(SERVER_RUNNING)) {
                    break true;
                }
                if Instant::now() > deadline {
                    break false;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            };
            if ready {
                match self.open().await {
                    // A flap while opening makes the new objects stale again.
                    Ok(o) if owner.borrow().generation == generation && owner.borrow().present => {
                        return Some(o);
                    }
                    Ok(_) => {}
                    Err(e) => tracing::debug!("cannot reopen the Avahi browser: {e}"),
                }
            }
            tokio::select! {
                _ = stop.changed() => return None,
                _ = shutdown.changed() => return None,
                () = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }
}

/// `(interface, protocol, name)` of a browsed item.
type Item = (i32, i32, String);

/// What the tracker decided about a new item.
#[derive(Debug, PartialEq, Eq)]
enum NewItem {
    /// Track it and resolve it.
    Accepted,
    /// Already tracked.
    Known,
    /// The cap is reached.
    Refused,
}

/// The items one session knows about, with whatever object `A` is held for
/// each resolver. Sans-IO: the cache side effects go to `shared`.
struct Tracker<A> {
    kind: ServiceKind,
    cap: usize,
    aprotocols: Vec<i32>,
    known: HashMap<Item, Vec<A>>,
}

impl<A> Tracker<A> {
    fn new(kind: ServiceKind, cap: usize, aprotocols: Vec<i32>) -> Self {
        Self {
            kind,
            cap,
            aprotocols,
            known: HashMap::new(),
        }
    }

    fn item_new(&mut self, item: &Item) -> NewItem {
        if self.known.contains_key(item) {
            NewItem::Known
        } else if self.known.len() >= self.cap {
            NewItem::Refused
        } else {
            self.known.insert(item.clone(), Vec::new());
            NewItem::Accepted
        }
    }

    fn source(item: &Item, aprotocol: i32) -> String {
        format!("{}/{}/{aprotocol}", item.0, item.1)
    }

    /// Forgets `item` and removes **every** source it can have contributed to
    /// the cache: one per address family, whether or not a resolver object
    /// was ever handed back (a `Found` can be processed before that, and an
    /// item removed in between would otherwise stay as a ghost). Returns the
    /// resolver objects to free and whether the consumer is still there.
    fn item_remove(&mut self, item: &Item, shared: &BrowseShared) -> (Vec<A>, bool) {
        let actives = self.known.remove(item).unwrap_or_default();
        let id = ServiceId::new(self.kind, item.2.clone());
        let mut open = true;
        for &ap in &self.aprotocols {
            let src = Self::source(item, ap);
            open &= shared.update(|c| c.on_removed(&id, &src));
        }
        (actives, open)
    }

    /// A resolver could not be created: release the item's slot (it is tried
    /// again if Avahi announces it anew).
    fn resolver_failed(
        &mut self,
        item: &Item,
        error: &str,
        shared: &BrowseShared,
    ) -> (Vec<A>, bool) {
        shared.note_failure(error);
        self.item_remove(item, shared)
    }

    /// Handles a `Found`: the signal names its item, so it does not matter
    /// whether the resolver object has been handed back yet. Ignored if the
    /// item is not tracked (removed meanwhile, or another task's).
    fn found(&self, f: Found, shared: &BrowseShared) -> bool {
        if f.ty != self.kind.type_label()
            || !self
                .known
                .contains_key(&(f.interface, f.protocol, f.name.clone()))
        {
            return true;
        }
        let source = Self::source(&(f.interface, f.protocol, f.name.clone()), f.aprotocol);
        let id = ServiceId::new(self.kind, f.name);
        let view = SourceView {
            host: format!("{}.", f.host.trim_end_matches('.')),
            port: f.port,
            addresses: BTreeSet::from([f.addr]),
            // Avahi hands the strings out in reverse wire order.
            txt: TxtRecord::from_strings(f.txt.iter().rev().map(Vec::as_slice)).record,
        };
        shared.update(|c| c.on_resolved(id, source, view))
    }
}

struct Found {
    interface: i32,
    protocol: i32,
    name: String,
    ty: String,
    host: String,
    aprotocol: i32,
    addr: IpAddr,
    port: u16,
    txt: Vec<Vec<u8>>,
}

/// One resolver of an item.
struct Active {
    proxy: ServiceResolver,
    /// Which connection of the pool holds it.
    conn: usize,
}

enum Req {
    Resolve {
        item: Item,
        domain: String,
        aprotocol: i32,
    },
    Free(ServiceResolver, usize),
}

enum Created {
    Ok {
        item: Item,
        proxy: ServiceResolver,
        conn: usize,
    },
    Failed {
        item: Item,
        error: String,
    },
    /// The `Found` stream of a connection added to the pool. Sent before the
    /// first resolver of that connection.
    Stream(MessageStream),
}

/// Most connections one browse session may use (see [`creator`]).
const MAX_POOL: usize = 8;

/// A D-Bus connection that holds resolvers.
struct PoolConn {
    conn: zbus::Connection,
    server: ServerProxy<'static>,
    /// The daemon refused more objects for this client.
    full: bool,
}

fn too_many_objects(e: &zbus::Error) -> bool {
    matches!(e, zbus::Error::MethodError(name, _, _) if name.as_str().ends_with("TooManyObjectsError"))
}

/// Creates and frees resolver objects; the only place that awaits D-Bus calls
/// for a browse session.
///
/// `avahi-daemon` limits the objects of one client (about 1024; a resolver
/// counts for three, which allows roughly 330 resolvers per connection).
/// When the daemon says `TooManyObjectsError`, the resolver is created on
/// another connection instead, opened on demand, up to [`MAX_POOL`]. Avahi
/// sends a resolver's signals only to its owner, so every extra connection has
/// its own `Found` stream, handed to the session loop before first use.
async fn creator(
    first: PoolConn,
    kind: ServiceKind,
    mut requests: mpsc::UnboundedReceiver<Req>,
    created: mpsc::UnboundedSender<Created>,
) {
    let mut pool = vec![first];
    while let Some(req) = requests.recv().await {
        match req {
            Req::Free(proxy, idx) => {
                let _ = proxy.free().await;
                if let Some(c) = pool.get_mut(idx) {
                    c.full = false;
                }
            }
            Req::Resolve {
                item,
                domain,
                aprotocol,
            } => {
                let mut error = String::from("no connection available");
                let mut i = 0;
                loop {
                    if i == pool.len() {
                        if pool.len() >= MAX_POOL {
                            break;
                        }
                        // Open another connection; its stream first.
                        let opened = async {
                            let conn = zbus::Connection::system().await.map_err(bus_err)?;
                            let stream =
                                signal_stream(&conn, signal_rule(RESOLVER_IFACE, None)?).await?;
                            let server = server_proxy(&conn).await?;
                            Ok::<_, Error>((
                                PoolConn {
                                    conn,
                                    server,
                                    full: false,
                                },
                                stream,
                            ))
                        }
                        .await;
                        match opened {
                            Ok((c, stream)) => {
                                let _ = created.send(Created::Stream(stream));
                                pool.push(c);
                            }
                            Err(e) => {
                                error = e.to_string();
                                break;
                            }
                        }
                    }
                    if pool[i].full {
                        i += 1;
                        continue;
                    }
                    match pool[i]
                        .server
                        .service_resolver_new(
                            item.0,
                            item.1,
                            &item.2,
                            kind.type_label(),
                            &domain,
                            aprotocol,
                            0,
                        )
                        .await
                    {
                        Ok(path) => {
                            let proxy = ServiceResolver::new(&pool[i].conn, path);
                            let _ = created.send(Created::Ok {
                                item: item.clone(),
                                proxy,
                                conn: i,
                            });
                            error.clear();
                            break;
                        }
                        Err(e) if too_many_objects(&e) => {
                            pool[i].full = true;
                            error = e.to_string();
                            i += 1;
                        }
                        Err(e) => {
                            error = e.to_string();
                            break;
                        }
                    }
                }
                if !error.is_empty() {
                    let _ = created.send(Created::Failed { item, error });
                }
            }
        }
    }
}

struct ItemSig {
    interface: i32,
    protocol: i32,
    name: String,
    domain: String,
    path: String,
}

fn item_sig(msg: &Message) -> Option<ItemSig> {
    let (interface, protocol, name, _ty, domain, _flags): (i32, i32, String, String, String, u32) =
        msg.body().deserialize().ok()?;
    Some(ItemSig {
        path: msg.header().path()?.to_string(),
        interface,
        protocol,
        name,
        domain,
    })
}

type FoundBody = (
    i32,
    i32,
    String,
    String,
    String,
    String,
    i32,
    String,
    u16,
    Vec<Vec<u8>>,
    u32,
);

fn parse_found(msg: &Message) -> Option<Found> {
    let (interface, protocol, name, ty, _domain, host, aprotocol, address, port, txt, _flags) =
        msg.body().deserialize::<FoundBody>().ok()?;
    Some(Found {
        interface,
        protocol,
        name,
        ty,
        host,
        aprotocol,
        addr: address.parse().ok()?,
        port,
        txt,
    })
}

enum End {
    Stop,
    Owner,
}

impl Opened {
    async fn session(
        self,
        stop: &mut watch::Receiver<bool>,
        shutdown: &mut watch::Receiver<bool>,
        owner: &mut watch::Receiver<Owner>,
    ) -> End {
        let Opened {
            task: t,
            browser,
            mut items,
            found: first_found,
        } = self;
        // The `Found` streams of all pool connections, merged.
        let mut found = futures_util::stream::SelectAll::new();
        found.push(first_found);
        let browser_path = browser.path().to_string();
        let session_owner = *owner.borrow_and_update();
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (created_tx, mut created_rx) = mpsc::unbounded_channel();
        let creator = tokio::spawn(creator(
            PoolConn {
                conn: t.conn.clone(),
                server: t.server.clone(),
                full: false,
            },
            t.kind,
            req_rx,
            created_tx,
        ));
        let mut tracker: Tracker<Active> = Tracker::new(t.kind, t.cap, t.aprotocols.clone());
        let mut last_warn: Option<Instant> = None;
        let end = 'main: loop {
            tokio::select! {
                _ = stop.changed() => break End::Stop,
                _ = shutdown.changed() => break End::Stop,
                r = owner.changed() => {
                    if r.is_err() {
                        break End::Stop;
                    }
                    let o = *owner.borrow_and_update();
                    if !o.present || o.generation != session_owner.generation {
                        break End::Owner;
                    }
                }
                Some(Ok(msg)) = items.next() => {
                    let Some(sig) = item_sig(&msg) else { continue };
                    if sig.path != browser_path {
                        continue;
                    }
                    let member = msg.header().member().map(ToString::to_string).unwrap_or_default();
                    let item = (sig.interface, sig.protocol, sig.name.clone());
                    match member.as_str() {
                        "ItemNew" => match tracker.item_new(&item) {
                            NewItem::Accepted => {
                                for &ap in &t.aprotocols {
                                    let _ = req_tx.send(Req::Resolve {
                                        item: item.clone(),
                                        domain: sig.domain.clone(),
                                        aprotocol: ap,
                                    });
                                }
                            }
                            NewItem::Known => {}
                            NewItem::Refused => t.shared.note_refused(),
                        },
                        "ItemRemove" => {
                            let (actives, open) = tracker.item_remove(&item, &t.shared);
                            for a in actives {
                                let _ = req_tx.send(Req::Free(a.proxy, a.conn));
                            }
                            if !open {
                                break 'main End::Stop;
                            }
                        }
                        _ => {}
                    }
                }
                Some(c) = created_rx.recv() => match c {
                    Created::Stream(s) => found.push(s),
                    Created::Ok { item, proxy, conn } => {
                        // The item may have been removed while the resolver was created.
                        match tracker.known.get_mut(&item) {
                            Some(list) => list.push(Active { proxy, conn }),
                            None => {
                                let _ = req_tx.send(Req::Free(proxy, conn));
                            }
                        }
                    }
                    Created::Failed { item, error } => {
                        if last_warn.is_none_or(|t| t.elapsed() >= WARN_EVERY) {
                            last_warn = Some(Instant::now());
                            tracing::warn!(
                                "Avahi could not resolve {} (this and further failures are counted; \
                                 avahi-daemon limits the objects per client): {error}",
                                sanitize(&item.2)
                            );
                        }
                        let (actives, open) = tracker.resolver_failed(&item, &error, &t.shared);
                        for a in actives {
                            let _ = req_tx.send(Req::Free(a.proxy, a.conn));
                        }
                        if !open {
                            break 'main End::Stop;
                        }
                    }
                },
                Some(Ok(msg)) = found.next() => {
                    if msg.header().member().is_none_or(|m| m.as_str() != "Found") {
                        continue;
                    }
                    // Resolvers are matched by what the signal says, not by object
                    // path: the signal can arrive before the creator task has handed
                    // the new resolver back, and other tasks' resolvers share the
                    // stream.
                    let Some(f) = parse_found(&msg) else { continue };
                    if !tracker.found(f, &t.shared) {
                        break End::Stop;
                    }
                }
                else => break End::Stop,
            }
        };
        // Stop reading signals first: nothing below needs them, and unread
        // streams must not be left to fill up while method calls are awaited.
        drop(items);
        drop(found);
        drop(req_tx);
        match end {
            End::Stop => {
                let _ = tokio::time::timeout(CLEANUP_WAIT, creator).await;
                while let Ok(Created::Ok { proxy, .. }) = created_rx.try_recv() {
                    let _ = proxy.free().await;
                }
                let items: Vec<Item> = tracker.known.keys().cloned().collect();
                for item in items {
                    let (actives, _) = tracker.item_remove(&item, &t.shared);
                    for a in actives {
                        let _ = a.proxy.free().await;
                    }
                }
                let _ = browser.free().await;
            }
            End::Owner => {
                // The daemon and every object in it are gone: do not talk to
                // them (a call to the name could start a new daemon). Report
                // all that was known as vanished; it comes back when the daemon
                // does.
                creator.abort();
                let items: Vec<Item> = tracker.known.keys().cloned().collect();
                for item in items {
                    let (actives, _) = tracker.item_remove(&item, &t.shared);
                    drop(actives);
                }
                drop(browser);
            }
        }
        end
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ServiceInstance;

    fn found(name: &str, ap: i32, port: u16) -> Found {
        Found {
            interface: 2,
            protocol: PROTO_INET,
            name: name.into(),
            ty: ServiceKind::Arc.type_label().into(),
            host: "h.local".into(),
            aprotocol: ap,
            addr: "10.0.0.1".parse().unwrap(),
            port,
            txt: vec![],
        }
    }

    fn item(name: &str) -> Item {
        (2, PROTO_INET, name.into())
    }

    fn drain(rx: &mut mpsc::Receiver<BrowseEvent>) -> Vec<BrowseEvent> {
        let mut v = Vec::new();
        while let Ok(e) = rx.try_recv() {
            v.push(e);
        }
        v
    }

    /// `Found` is processed before the resolver object is handed back, then
    /// the item is removed: nothing may be left behind, for any family.
    #[test]
    fn item_removed_before_its_resolver_is_handed_back_leaves_no_ghost() {
        let (shared, view, mut rx) = BrowseShared::new(10, 64);
        let mut t: Tracker<()> = Tracker::new(ServiceKind::Arc, 10, vec![PROTO_INET, PROTO_INET6]);
        assert_eq!(t.item_new(&item("a")), NewItem::Accepted);
        // Both families answered; neither resolver object is in `known` yet.
        assert!(t.found(found("a", PROTO_INET, 1), &shared));
        assert!(t.found(found("a", PROTO_INET6, 1), &shared));
        assert_eq!(view.snapshot().len(), 1);
        let (actives, open) = t.item_remove(&item("a"), &shared);
        assert!(actives.is_empty() && open);
        assert!(
            view.snapshot().is_empty(),
            "ghost entry: {:?}",
            view.snapshot()
        );
        let ev = drain(&mut rx);
        assert!(
            matches!(ev.last(), Some(BrowseEvent::Disappeared(_))),
            "{ev:?}"
        );
        // A late `Found` of the removed item is ignored.
        assert!(t.found(found("a", PROTO_INET, 2), &shared));
        assert!(view.snapshot().is_empty());
    }

    #[test]
    fn found_for_other_kinds_and_untracked_items_is_ignored() {
        let (shared, view, _rx) = BrowseShared::new(10, 64);
        let mut t: Tracker<()> = Tracker::new(ServiceKind::Arc, 10, vec![PROTO_INET]);
        t.item_new(&item("a"));
        let mut other = found("a", PROTO_INET, 1);
        other.ty = ServiceKind::Cmc.type_label().into();
        t.found(other, &shared);
        t.found(found("zzz", PROTO_INET, 1), &shared);
        assert!(view.snapshot().is_empty());
    }

    #[test]
    fn a_failed_resolver_releases_the_slot_and_is_counted() {
        let (shared, view, _rx) = BrowseShared::new(10, 64);
        let mut t: Tracker<()> = Tracker::new(ServiceKind::Arc, 2, vec![PROTO_INET]);
        assert_eq!(t.item_new(&item("a")), NewItem::Accepted);
        assert_eq!(t.item_new(&item("b")), NewItem::Accepted);
        assert_eq!(t.item_new(&item("c")), NewItem::Refused);
        t.resolver_failed(&item("a"), "x", &shared);
        assert_eq!(view.failures(), 1);
        // The slot is free again: the failed item can be retried; the cap holds.
        assert_eq!(t.item_new(&item("a")), NewItem::Accepted);
        assert_eq!(t.item_new(&item("c")), NewItem::Refused);
        let _: Vec<ServiceInstance> = view.snapshot();
    }
}
