//! Browsing through Avahi: a service browser per kind and interface, a
//! resolver per found instance and address family. A resolver's `Found`
//! signal repeats when the records change, which becomes an update event.
//!
//! Properties of the D-Bus side that shape this code:
//!
//! * Avahi starts emitting signals as soon as a browser or resolver object
//!   exists, possibly before a client could subscribe to that object's path.
//!   The `ItemNew`/`ItemRemove` stream is therefore created from a path-less
//!   match rule *before* the browser object is created. Each resolver pool
//!   connection subscribes to `Found` before creating any resolver.
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
    for (kind_index, &kind) in kinds.iter().enumerate() {
        for (interface_index, &interface) in idxs.iter().enumerate() {
            let slot = kind_index * idxs.len() + interface_index;
            shared.session(slot, None);
            let task = Arc::new(Task {
                server: server.clone(),
                kind,
                interface,
                protocol: protocol(cfg.ip_families),
                aprotocols: aprotocols(cfg.ip_families),
                cap: cfg.max_instances_per_kind,
                shared: shared.clone(),
                slot,
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
    )
    .with_availability(move || {
        let state = *owner.borrow();
        (state.present, state.generation)
    });
    Ok((rx, handle))
}

struct Task {
    server: ServerProxy<'static>,
    kind: ServiceKind,
    interface: i32,
    protocol: i32,
    aprotocols: Vec<i32>,
    cap: usize,
    shared: BrowseShared,
    slot: usize,
}

struct Opened {
    task: Arc<Task>,
    browser: ServiceBrowser,
    items: MessageStream,
    found: MessageStream,
    resolvers: PoolConn,
}

impl Task {
    async fn open(self: &Arc<Self>) -> Result<Opened, Error> {
        // Resolver objects belong to this session's connections. Closing them
        // also reclaims a resolver whose New reply was cancelled during retry.
        let (resolvers, found) = PoolConn::open().await?;
        // Item stream first, browser object second (see the module docs).
        let items = signal_stream(&resolvers.conn, signal_rule(BROWSER_IFACE, None)?).await?;
        let path = resolvers
            .server
            .service_browser_new(self.interface, self.protocol, self.kind.type_label(), "", 0)
            .await
            .map_err(bus_err)?;
        let browser = ServiceBrowser::new(&resolvers.conn, path);
        Ok(Opened {
            task: Arc::clone(self),
            browser,
            items,
            found,
            resolvers,
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
            match opened.session(&mut stop, &mut shutdown, &mut owner).await {
                End::Stop => return,
                End::Retry => {
                    tokio::select! {
                        _ = stop.changed() => return,
                        _ = shutdown.changed() => return,
                        () = tokio::time::sleep(Duration::from_secs(1)) => {}
                    }
                }
                End::Owner => {}
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
    Accepted(u64),
    /// Already tracked.
    Known,
    /// The cap is reached.
    Refused,
}

struct Known<A> {
    generation: u64,
    active: Vec<A>,
}

/// The items one session knows about, with whatever object `A` is held for
/// each resolver. Sans-IO: the cache side effects go to `shared`.
struct Tracker<A> {
    kind: ServiceKind,
    cap: usize,
    aprotocols: Vec<i32>,
    known: HashMap<Item, Known<A>>,
    generation: u64,
    pending: std::collections::HashSet<Item>,
    unresolved: std::collections::HashSet<Item>,
    unresolved_overflow: bool,
}

impl<A> Tracker<A> {
    fn new(kind: ServiceKind, cap: usize, aprotocols: Vec<i32>) -> Self {
        Self {
            kind,
            cap,
            aprotocols,
            known: HashMap::new(),
            generation: 0,
            pending: std::collections::HashSet::new(),
            unresolved: std::collections::HashSet::new(),
            unresolved_overflow: false,
        }
    }

    fn item_new(&mut self, item: &Item) -> NewItem {
        if self.known.contains_key(item) {
            NewItem::Known
        } else if self.known.len() >= self.cap {
            self.note_unresolved(item);
            NewItem::Refused
        } else {
            self.unresolved.remove(item);
            self.pending.insert(item.clone());
            self.generation += 1;
            self.known.insert(
                item.clone(),
                Known {
                    generation: self.generation,
                    active: Vec::new(),
                },
            );
            NewItem::Accepted(self.generation)
        }
    }

    /// A removed item can reappear while its old resolver is still being
    /// created. Only the request for the current appearance may attach.
    fn attach(&mut self, item: &Item, generation: u64, active: A) -> Result<(), A> {
        match self.known.get_mut(item) {
            Some(known) if known.generation == generation => {
                known.active.push(active);
                Ok(())
            }
            _ => Err(active),
        }
    }

    fn note_unresolved(&mut self, item: &Item) {
        if self.unresolved.len() < self.cap || self.unresolved.contains(item) {
            self.unresolved.insert(item.clone());
        } else {
            // Bounded bookkeeping: recovery from a saturated failure set
            // requires a new backend session, not an unbounded LAN map.
            self.unresolved_overflow = true;
        }
    }

    fn reconciled(&self) -> bool {
        self.pending.is_empty() && self.unresolved.is_empty() && !self.unresolved_overflow
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
        let actives = self
            .known
            .remove(item)
            .map(|k| k.active)
            .unwrap_or_default();
        self.pending.remove(item);
        self.unresolved.remove(item);
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
        generation: u64,
        error: &str,
        shared: &BrowseShared,
    ) -> (Vec<A>, bool) {
        if self
            .known
            .get(item)
            .is_none_or(|k| k.generation != generation)
        {
            return (Vec::new(), true);
        }
        shared.note_failure(error);
        let result = self.item_remove(item, shared);
        self.note_unresolved(item);
        result
    }

    fn resolver_path_failed(
        &mut self,
        path: &str,
        error: &str,
        shared: &BrowseShared,
        path_of: impl Fn(&A) -> &str,
    ) -> Option<(Vec<A>, bool)> {
        let (item, generation) = self.known.iter().find_map(|(item, known)| {
            known
                .active
                .iter()
                .any(|a| path_of(a) == path)
                .then(|| (item.clone(), known.generation))
        })?;
        Some(self.resolver_failed(&item, generation, error, shared))
    }

    /// Match signals to an attached resolver, never just a reusable item name.
    /// Before attachment the session buffers signals until the creation result
    /// supplies the appearance generation.
    fn found(
        &mut self,
        path: &str,
        f: &Found,
        shared: &BrowseShared,
        path_of: impl Fn(&A) -> &str,
    ) -> Option<bool> {
        let item = (f.interface, f.protocol, f.name.clone());
        if f.ty != self.kind.type_label()
            || !self
                .known
                .get(&item)
                .is_some_and(|k| k.active.iter().any(|a| path_of(a) == path))
        {
            return None;
        }
        self.pending.remove(&item);
        let source = Self::source(&item, f.aprotocol);
        let id = ServiceId::new(self.kind, f.name.clone());
        let view = SourceView {
            interface: u32::try_from(f.interface)
                .ok()
                .and_then(|index| nix::net::if_::if_indextoname(index).ok())
                .map(|name| name.to_string_lossy().into_owned()),
            host: format!("{}.", f.host.trim_end_matches('.')),
            port: f.port,
            addresses: BTreeSet::from([f.addr]),
            // Avahi hands the strings out in reverse wire order.
            txt: TxtRecord::from_strings(f.txt.iter().rev().map(Vec::as_slice)).record,
        };
        Some(shared.update(|c| c.on_resolved(id, source, view)))
    }

    fn signal(
        &mut self,
        path: &str,
        signal: &ResolverSignal,
        shared: &BrowseShared,
        path_of: impl Fn(&A) -> &str,
    ) -> Option<(Vec<A>, bool)> {
        match signal {
            ResolverSignal::Found(f) => self
                .found(path, f, shared, path_of)
                .map(|open| (Vec::new(), open)),
            ResolverSignal::Failed(error) => {
                self.resolver_path_failed(path, error, shared, path_of)
            }
        }
    }
}

#[derive(Debug)]
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

#[derive(Debug)]
enum ResolverSignal {
    Found(Found),
    Failed(String),
}

/// Early signals and retired paths are bounded by the configured resolver
/// capacity. Retired paths stay excluded for the rest of this session: Free
/// completion does not mean its already-queued signals have been consumed.
struct EarlySignals {
    limit: usize,
    signals: HashMap<String, ResolverSignal>,
    retired: std::collections::HashSet<String>,
}

impl EarlySignals {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            signals: HashMap::new(),
            retired: std::collections::HashSet::new(),
        }
    }

    fn retire(&mut self, path: &str) -> bool {
        self.signals.remove(path);
        if self.retired.contains(path) {
            return true;
        }
        if self.retired.len() == self.limit {
            return false;
        }
        self.retired.insert(path.to_owned());
        true
    }

    /// Do not depend on the backend keeping object paths unique. Reusing a
    /// retired path requires a fresh session before this creation can attach.
    fn take(
        &mut self,
        path: &str,
        creating: usize,
    ) -> Result<Option<ResolverSignal>, &'static str> {
        if self.retired.contains(path) {
            return Err("resolver creation reused a retired object path");
        }
        let signal = self.signals.remove(path);
        self.creation_finished(creating);
        Ok(signal)
    }

    fn creation_finished(&mut self, creating: usize) {
        if creating == 0 {
            self.signals.clear();
        }
    }

    /// False requests a fresh session rather than silently losing a valid
    /// initial Found. Outstanding creations obey the same capacity bound.
    fn record(&mut self, path: String, signal: ResolverSignal, creating: usize) -> bool {
        if creating == 0 || self.retired.contains(&path) {
            return true;
        }
        if !self.signals.contains_key(&path) && self.signals.len() >= creating.min(self.limit) {
            return false;
        }
        if !matches!(self.signals.get(&path), Some(ResolverSignal::Failed(_))) {
            self.signals.insert(path, signal);
        }
        true
    }
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
        generation: u64,
        domain: String,
        aprotocol: i32,
    },
    Free(ServiceResolver, usize),
}

enum Created {
    Ok {
        item: Item,
        generation: u64,
        proxy: ServiceResolver,
        conn: usize,
    },
    Failed {
        item: Item,
        generation: u64,
        error: String,
    },
    /// The `Found` stream of a connection added to the pool. Sent before the
    /// first resolver of that connection.
    Stream {
        stream: MessageStream,
        connection: zbus::Connection,
    },
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

impl PoolConn {
    async fn open() -> Result<(Self, MessageStream), Error> {
        let conn = zbus::Connection::system().await.map_err(bus_err)?;
        let stream = signal_stream(&conn, signal_rule(RESOLVER_IFACE, None)?).await?;
        let server = server_proxy(&conn).await?;
        Ok((
            Self {
                conn,
                server,
                full: false,
            },
            stream,
        ))
    }
}

async fn cancel_creator(creator: tokio::task::JoinHandle<()>) {
    creator.abort();
    // Join before draining results: no producer can enqueue a late resolver.
    let _ = creator.await;
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
                generation,
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
                        let opened = PoolConn::open().await;
                        match opened {
                            Ok((c, stream)) => {
                                let _ = created.send(Created::Stream {
                                    stream,
                                    connection: c.conn.clone(),
                                });
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
                                generation,
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
                    let _ = created.send(Created::Failed {
                        item,
                        generation,
                        error,
                    });
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
}

fn item_sig(msg: &Message) -> Option<ItemSig> {
    let (interface, protocol, name, _ty, domain, _flags): (i32, i32, String, String, String, u32) =
        msg.body().deserialize().ok()?;
    Some(ItemSig {
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
    Retry,
}

/// Failure signals carry an error string, not an ItemNew/Found tuple.
/// A browser failure retries its session; a resolver failure invalidates only
/// the item owned by that resolver path.
fn failure_error(msg: &Message) -> Option<String> {
    if msg
        .header()
        .member()
        .is_none_or(|member| member.as_str() != "Failure")
    {
        return None;
    }
    Some(
        msg.body()
            .deserialize::<String>()
            .unwrap_or_else(|_| "malformed Avahi failure signal".into()),
    )
}

fn browser_failure(msg: &Message, shared: &BrowseShared, slot: usize) -> bool {
    let Some(error) = failure_error(msg) else {
        return false;
    };
    shared.note_failure(&error);
    shared.session(slot, None);
    true
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
            resolvers,
        } = self;
        // The `Found` streams of all pool connections, merged.
        let mut found = futures_util::stream::SelectAll::new();
        found.push(first_found);
        let browser_path = browser.path().to_string();
        let session_owner = *owner.borrow_and_update();
        t.shared.begin_reconciliation(session_owner.generation);
        t.shared.session(t.slot, None);
        let mut all_for_now = false;
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (created_tx, mut created_rx) = mpsc::unbounded_channel();
        let mut resolver_connections = vec![resolvers.conn.clone()];
        let creator = tokio::spawn(creator(resolvers, t.kind, req_rx, created_tx));
        let mut tracker: Tracker<Active> = Tracker::new(t.kind, t.cap, t.aprotocols.clone());
        let mut last_warn: Option<Instant> = None;
        let mut creating = 0_usize;
        // A resolver can signal before its New reply reaches the creator.
        // Keep at most one early signal per outstanding creation; each
        // session owns its connections, so other sessions cannot populate it.
        let mut early_signals = EarlySignals::new(t.cap.saturating_mul(t.aprotocols.len()));
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
                    if msg.header().path().is_none_or(|p| p.as_str() != browser_path) {
                        continue;
                    }
                    let member = msg.header().member().map(ToString::to_string).unwrap_or_default();
                    if browser_failure(&msg, &t.shared, t.slot) {
                        break End::Retry;
                    }
                    if member == "AllForNow" {
                        all_for_now = true;
                        t.shared.session(t.slot, tracker.reconciled().then_some(session_owner.generation));
                        continue;
                    }
                    let Some(sig) = item_sig(&msg) else { continue };
                    let item = (sig.interface, sig.protocol, sig.name.clone());
                    match member.as_str() {
                        "ItemNew" => match tracker.item_new(&item) {
                            NewItem::Accepted(generation) => {
                                for &ap in &t.aprotocols {
                                    if creating == early_signals.limit {
                                        t.shared.note_failure("resolver creation backlog reached capacity");
                                        break 'main End::Retry;
                                    }
                                    if req_tx.send(Req::Resolve {
                                        item: item.clone(),
                                        generation,
                                        domain: sig.domain.clone(),
                                        aprotocol: ap,
                                    }).is_ok() {
                                        creating += 1;
                                    }
                                }
                            }
                            NewItem::Known => {}
                            NewItem::Refused => t.shared.note_refused(),
                        },
                        "ItemRemove" => {
                            let (actives, open) = tracker.item_remove(&item, &t.shared);
                            for a in actives {
                                if !early_signals.retire(a.proxy.path()) {
                                    t.shared.note_failure("retired resolver tracking reached capacity");
                                    break 'main End::Retry;
                                }
                                let _ = req_tx.send(Req::Free(a.proxy, a.conn));
                            }
                            if !open {
                                break 'main End::Stop;
                            }
                        }
                        _ => {}
                    }
                    t.shared.session(t.slot, (all_for_now && tracker.reconciled()).then_some(session_owner.generation));
                }
                Some(c) = created_rx.recv() => match c {
                    Created::Stream { stream, connection } => {
                        resolver_connections.push(connection);
                        found.push(stream);
                    }
                    Created::Ok { item, generation, proxy, conn } => {
                        creating = creating.saturating_sub(1);
                        let path = proxy.path().to_owned();
                        let early = match early_signals.take(&path, creating) {
                            Ok(signal) => signal,
                            Err(error) => {
                                t.shared.note_failure(error);
                                break 'main End::Retry;
                            }
                        };
                        // Reusing the item tuple does not reuse its appearance.
                        match tracker.attach(&item, generation, Active { proxy, conn }) {
                            Ok(()) => {
                                if let Some((actives, open)) = early.as_ref().and_then(|signal| {
                                    tracker.signal(&path, signal, &t.shared, |a| a.proxy.path())
                                })
                                {
                                    for a in actives {
                                        if !early_signals.retire(a.proxy.path()) {
                                            t.shared.note_failure("retired resolver tracking reached capacity");
                                            break 'main End::Retry;
                                        }
                                        let _ = req_tx.send(Req::Free(a.proxy, a.conn));
                                    }
                                    if !open { break 'main End::Stop; }
                                }
                                t.shared.session(t.slot, (all_for_now && tracker.reconciled()).then_some(session_owner.generation));
                            }
                            Err(a) => {
                                if !early_signals.retire(a.proxy.path()) {
                                    t.shared.note_failure("retired resolver tracking reached capacity");
                                    break 'main End::Retry;
                                }
                                let _ = req_tx.send(Req::Free(a.proxy, a.conn));
                            }
                        }
                    }
                    Created::Failed { item, generation, error } => {
                        creating = creating.saturating_sub(1);
                        early_signals.creation_finished(creating);
                        if last_warn.is_none_or(|t| t.elapsed() >= WARN_EVERY) {
                            last_warn = Some(Instant::now());
                            tracing::warn!(
                                "Avahi could not resolve {} (this and further failures are counted; \
                                 avahi-daemon limits the objects per client): {error}",
                                sanitize(&item.2)
                            );
                        }
                        let (actives, open) = tracker.resolver_failed(&item, generation, &error, &t.shared);
                        for a in actives {
                            if !early_signals.retire(a.proxy.path()) {
                                t.shared.note_failure("retired resolver tracking reached capacity");
                                break 'main End::Retry;
                            }
                            let _ = req_tx.send(Req::Free(a.proxy, a.conn));
                        }
                        if !open {
                            break 'main End::Stop;
                        }
                        t.shared.session(t.slot, (all_for_now && tracker.reconciled()).then_some(session_owner.generation));
                    }
                },
                Some(Ok(msg)) = found.next() => {
                    let Some(path) = msg.header().path().map(ToString::to_string) else { continue };
                    let signal = if let Some(error) = failure_error(&msg) {
                        ResolverSignal::Failed(error)
                    } else if msg.header().member().is_some_and(|m| m.as_str() == "Found") {
                        let Some(f) = parse_found(&msg) else { continue };
                        ResolverSignal::Found(f)
                    } else { continue };
                    match tracker.signal(&path, &signal, &t.shared, |a| a.proxy.path()) {
                        Some((actives, open)) => {
                            for a in actives {
                                if !early_signals.retire(a.proxy.path()) {
                                    t.shared.note_failure("retired resolver tracking reached capacity");
                                    break 'main End::Retry;
                                }
                                let _ = req_tx.send(Req::Free(a.proxy, a.conn));
                            }
                            if !open { break 'main End::Stop; }
                            t.shared.session(t.slot, (all_for_now && tracker.reconciled()).then_some(session_owner.generation));
                        }
                        None => {
                            if !early_signals.record(path, signal, creating) {
                                t.shared.note_failure("early resolver signal tracking reached capacity");
                                break 'main End::Retry;
                            }
                        }
                    }
                }
                else => break End::Stop,
            }
        };
        t.shared.session(t.slot, None);
        // Stop reading signals first: nothing below needs them, and unread
        // streams must not be left to fill up while method calls are awaited.
        drop(items);
        drop(found);
        drop(req_tx);
        cancel_creator(creator).await;
        // Drain every result variant. A Stream or Failed result must not hide
        // later connections/resolvers. Closing the owned connections below
        // reclaims queued, active and in-flight daemon objects alike.
        while let Some(result) = created_rx.recv().await {
            if let Created::Stream { connection, .. } = result {
                resolver_connections.push(connection);
            }
        }
        for connection in resolver_connections {
            let _ = connection.close().await;
        }
        // Closing a session connection reclaims its browser and every resolver,
        // including New calls cancelled before their reply. Do not issue Free
        // against a vanished owner, which could activate a replacement daemon.
        let items: Vec<Item> = tracker.known.keys().cloned().collect();
        for item in items {
            let (actives, _) = tracker.item_remove(&item, &t.shared);
            drop(actives);
        }
        drop(browser);
        end
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ServiceInstance;

    #[tokio::test]
    async fn retry_cancels_and_joins_pending_resolver_creation() {
        struct OnDrop(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                let _ = self.0.take().unwrap().send(());
            }
        }
        let (started, ready) = tokio::sync::oneshot::channel();
        let (cancelled, dropped) = tokio::sync::oneshot::channel();
        let creator = tokio::spawn(async move {
            let _on_drop = OnDrop(Some(cancelled));
            let _ = started.send(());
            // Model a resolver New call that never replies. Cleanup must not
            // detach it and allow it to create objects after the next retry.
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        tokio::time::timeout(Duration::from_millis(100), cancel_creator(creator))
            .await
            .expect("cleanup must cancel stalled creation");
        dropped.await.expect("cancellation must be joined");
    }

    #[test]
    fn browser_failure_invalidates_its_open_ready_session() {
        let (shared, view, rx) = BrowseShared::new(10, 4);
        shared.session(0, Some(7));
        assert!(view.reconciled(7));
        let failure = Message::signal("/Client1/Service1", BROWSER_IFACE, "Failure")
            .unwrap()
            .build(&"org.freedesktop.Avahi.DisconnectedError")
            .unwrap();
        assert!(browser_failure(&failure, &shared, 0));
        assert!(!view.reconciled(7));
        assert_eq!(view.failures(), 1);
        assert!(!rx.is_closed(), "failure is independent of channel closure");
    }

    #[test]
    fn resolver_failure_preserves_other_items_and_other_sessions() {
        let (shared, view, _rx) = BrowseShared::new(10, 16);
        let (other_shared, other_view, _other_rx) = BrowseShared::new(10, 16);
        let mut tracker: Tracker<&str> = Tracker::new(ServiceKind::Arc, 10, vec![PROTO_INET]);
        let mut other: Tracker<&str> = Tracker::new(ServiceKind::Arc, 10, vec![PROTO_INET]);
        for (name, path) in [("a", "/Client1/A"), ("b", "/Client1/B")] {
            let generation = accept(&mut tracker, name);
            tracker.attach(&item(name), generation, path).unwrap();
            tracker
                .found(path, &found(name, PROTO_INET, 1), &shared, |p| p)
                .unwrap();
        }
        let generation = accept(&mut other, "c");
        other.attach(&item("c"), generation, "/Client2/C").unwrap();
        other
            .found(
                "/Client2/C",
                &found("c", PROTO_INET, 1),
                &other_shared,
                |p| p,
            )
            .unwrap();
        other_shared.session(0, Some(7));
        let before = view.observations();
        let other_before = other_view.observations();
        let failure = Message::signal("/Client1/A", RESOLVER_IFACE, "Failure")
            .unwrap()
            .build(&"org.freedesktop.Avahi.TimeoutError")
            .unwrap();
        let error = failure_error(&failure).unwrap();
        let (freed, open) = tracker
            .resolver_path_failed("/Client1/A", &error, &shared, |p| p)
            .unwrap();
        assert!(open);
        assert_eq!(freed, vec!["/Client1/A"]);
        assert_eq!(view.failures(), 1);
        assert_eq!(
            view.observations(),
            before
                .into_iter()
                .filter(|o| o.service.id.instance == "b")
                .collect::<Vec<_>>()
        );
        assert!(!tracker.reconciled(), "the failed item remains unresolved");
        assert!(
            tracker
                .resolver_path_failed("/Client1/A", &error, &shared, |p| p)
                .is_none()
        );
        assert!(
            other
                .resolver_path_failed("/Client1/A", &error, &other_shared, |p| p)
                .is_none()
        );
        assert_eq!(other_view.observations(), other_before);
        assert_eq!(other_view.failures(), 0);
        assert!(other_view.reconciled(7));
        assert!(other.reconciled());
    }

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

    fn accept<A>(tracker: &mut Tracker<A>, name: &str) -> u64 {
        let NewItem::Accepted(generation) = tracker.item_new(&item(name)) else {
            panic!("expected a new appearance");
        };
        generation
    }

    /// Early signals cannot contribute cache entries before their resolver
    /// is attached. Removing the appearance before attachment leaves no ghost.
    #[test]
    fn item_removed_before_its_resolver_is_handed_back_leaves_no_ghost() {
        let (shared, view, _rx) = BrowseShared::new(10, 64);
        let mut t: Tracker<&str> =
            Tracker::new(ServiceKind::Arc, 10, vec![PROTO_INET, PROTO_INET6]);
        let generation = accept(&mut t, "a");
        for (ap, path) in [(PROTO_INET, "/Client1/V4"), (PROTO_INET6, "/Client1/V6")] {
            assert!(t.found(path, &found("a", ap, 1), &shared, |p| p).is_none());
        }
        assert!(view.snapshot().is_empty());
        let (actives, open) = t.item_remove(&item("a"), &shared);
        assert!(actives.is_empty() && open);
        assert_eq!(
            t.attach(&item("a"), generation, "/Client1/V4"),
            Err("/Client1/V4")
        );
        assert!(
            t.found("/Client1/V4", &found("a", PROTO_INET, 2), &shared, |p| p)
                .is_none()
        );
        assert!(view.snapshot().is_empty());
    }

    #[test]
    fn found_for_other_kinds_and_untracked_items_is_ignored() {
        let (shared, view, _rx) = BrowseShared::new(10, 64);
        let mut t: Tracker<&str> = Tracker::new(ServiceKind::Arc, 10, vec![PROTO_INET]);
        let generation = accept(&mut t, "a");
        t.attach(&item("a"), generation, "/Client1/A").unwrap();
        let mut other = found("a", PROTO_INET, 1);
        other.ty = ServiceKind::Cmc.type_label().into();
        assert!(t.found("/Client1/A", &other, &shared, |p| p).is_none());
        assert!(
            t.found("/Client1/A", &found("zzz", PROTO_INET, 1), &shared, |p| p)
                .is_none()
        );
        assert!(view.snapshot().is_empty());
    }

    #[test]
    fn a_failed_resolver_releases_the_slot_and_is_counted() {
        let (shared, view, _rx) = BrowseShared::new(10, 64);
        let mut t: Tracker<()> = Tracker::new(ServiceKind::Arc, 2, vec![PROTO_INET]);
        let first = accept(&mut t, "a");
        let second = accept(&mut t, "b");
        assert_eq!(t.item_new(&item("c")), NewItem::Refused);
        t.resolver_failed(&item("a"), first, "x", &shared);
        assert_eq!(view.failures(), 1);
        // The slot is free again: the failed item can be retried; the cap holds.
        accept(&mut t, "a");
        assert_eq!(t.item_new(&item("c")), NewItem::Refused);
        // A late failure after removal must not restore unresolved state or
        // increment diagnostics for an item that no longer exists.
        t.item_remove(&item("b"), &shared);
        t.resolver_failed(&item("b"), second, "late", &shared);
        assert_eq!(view.failures(), 1);
        assert!(!t.unresolved.contains(&item("b")));
        let _: Vec<ServiceInstance> = view.snapshot();
    }

    #[test]
    fn removed_and_readded_item_rejects_old_results_and_resolver_signals() {
        let (shared, view, _rx) = BrowseShared::new(10, 64);
        let mut t: Tracker<&str> = Tracker::new(ServiceKind::Arc, 10, vec![PROTO_INET]);
        let old = accept(&mut t, "a");
        t.attach(&item("a"), old, "/Client1/Old").unwrap();
        assert!(
            t.found("/Client1/Old", &found("a", PROTO_INET, 1), &shared, |p| p)
                .unwrap()
        );
        t.item_remove(&item("a"), &shared);
        let current = accept(&mut t, "a");
        assert_ne!(old, current);
        assert_eq!(t.item_new(&item("a")), NewItem::Known);
        // A signal can precede the new resolver's creation result. It stays
        // untrusted until that result proves which appearance owns the path.
        let early = ResolverSignal::Found(found("a", PROTO_INET, 2));
        assert!(t.signal("/Client1/New", &early, &shared, |p| p).is_none());
        assert!(view.snapshot().is_empty());
        assert_eq!(
            t.attach(&item("a"), old, "/Client1/Late"),
            Err("/Client1/Late")
        );
        let (freed, open) = t.resolver_failed(&item("a"), old, "old creation failed", &shared);
        assert!(freed.is_empty() && open);
        assert_eq!(view.failures(), 0);
        assert!(!t.reconciled(), "replacement still waits for its resolver");
        t.attach(&item("a"), current, "/Client1/New").unwrap();
        let (freed, open) = t.signal("/Client1/New", &early, &shared, |p| p).unwrap();
        assert!(freed.is_empty() && open);
        assert!(t.reconciled());
        let healthy = view.observations();
        assert_eq!(healthy.len(), 1);
        assert_eq!(healthy[0].service.port, 2);
        // Late success/failure from the old appearance cannot remove or
        // overwrite a now healthy replacement, even with an identical tuple.
        assert_eq!(
            t.attach(&item("a"), old, "/Client1/AnotherLate"),
            Err("/Client1/AnotherLate")
        );
        t.resolver_failed(&item("a"), old, "late failure", &shared);
        for path in ["/Client1/Old", "/Client1/Late", "/Client1/AnotherLate"] {
            assert!(
                t.signal(
                    path,
                    &ResolverSignal::Found(found("a", PROTO_INET, 99)),
                    &shared,
                    |p| p
                )
                .is_none()
            );
            assert!(
                t.signal(
                    path,
                    &ResolverSignal::Failed("late signal".into()),
                    &shared,
                    |p| p
                )
                .is_none()
            );
        }
        assert_eq!(view.observations(), healthy);
        assert_eq!(view.failures(), 0);
        assert!(t.reconciled());
        // Current signals still work and affect only their own appearance.
        let (freed, open) = t
            .signal(
                "/Client1/New",
                &ResolverSignal::Failed("current failure".into()),
                &shared,
                |p| p,
            )
            .unwrap();
        assert!(open);
        assert_eq!(freed, vec!["/Client1/New"]);
        assert!(view.snapshot().is_empty());
        assert_eq!(view.failures(), 1);
        assert!(!t.reconciled());
    }

    #[test]
    fn retired_signals_cannot_steal_the_replacements_only_early_slot() {
        for failed in [false, true] {
            for old_first in [false, true] {
                let (shared, view, _rx) = BrowseShared::new(1, 64);
                let mut tracker: Tracker<&str> =
                    Tracker::new(ServiceKind::Arc, 1, vec![PROTO_INET]);
                let mut early = EarlySignals::new(1);
                let old = accept(&mut tracker, "a");
                tracker.attach(&item("a"), old, "/Client1/Old").unwrap();
                let (retired, open) = tracker.item_remove(&item("a"), &shared);
                assert!(open);
                for path in retired {
                    assert!(early.retire(path));
                }
                let current = accept(&mut tracker, "a");
                let old_signal = if failed {
                    ResolverSignal::Failed("retired failure".into())
                } else {
                    ResolverSignal::Found(found("a", PROTO_INET, 99))
                };
                let current_signal = ResolverSignal::Found(found("a", PROTO_INET, 2));
                let ordered = if old_first {
                    [
                        ("/Client1/Old", old_signal),
                        ("/Client1/New", current_signal),
                    ]
                } else {
                    [
                        ("/Client1/New", current_signal),
                        ("/Client1/Old", old_signal),
                    ]
                };
                // Exercise the actual dispatcher admission helper while exactly
                // one replacement creation is pending, before its Ok result.
                for (path, signal) in ordered {
                    assert!(tracker.signal(path, &signal, &shared, |p| p).is_none());
                    assert!(early.record(path.into(), signal, 1));
                }
                assert_eq!(early.signals.len(), 1);
                let signal = early.take("/Client1/New", 0).unwrap().unwrap();
                tracker.attach(&item("a"), current, "/Client1/New").unwrap();
                let (freed, open) = tracker
                    .signal("/Client1/New", &signal, &shared, |p| p)
                    .unwrap();
                assert!(freed.is_empty() && open);
                assert!(tracker.reconciled());
                assert_eq!(view.observations()[0].service.port, 2);
                assert_eq!(view.failures(), 0);
                assert!(early.signals.is_empty());
            }
        }
    }

    #[test]
    fn early_signal_storage_never_silently_evicts_at_capacity() {
        let mut early = EarlySignals::new(1);
        assert!(early.record(
            "/Client1/New".into(),
            ResolverSignal::Failed("first".into()),
            1
        ));
        assert!(early.record(
            "/Client1/New".into(),
            ResolverSignal::Found(found("a", PROTO_INET, 1)),
            1
        ));
        assert!(!early.record(
            "/Client1/Unexpected".into(),
            ResolverSignal::Found(found("b", PROTO_INET, 1)),
            1
        ));
        assert_eq!(early.signals.len(), 1);
        assert!(
            matches!(early.take("/Client1/New", 0), Ok(Some(ResolverSignal::Failed(error))) if error == "first")
        );
        assert!(early.retire("/Client1/Old"));
        assert!(early.retire("/Client1/Old"));
        assert!(!early.retire("/Client1/Older"));
        assert_eq!(early.retired.len(), 1);
        assert!(early.record(
            "/Client1/Old".into(),
            ResolverSignal::Failed("late".into()),
            1
        ));
        assert!(early.signals.is_empty());
    }

    #[test]
    fn creation_reusing_a_retired_path_requires_a_fresh_session() {
        let mut early = EarlySignals::new(2);
        assert!(early.retire("/Client1/Reused"));
        assert!(early.record(
            "/Client1/Reused".into(),
            ResolverSignal::Found(found("a", PROTO_INET, 2)),
            1
        ));
        assert!(early.signals.is_empty());
        // The production Created::Ok branch calls take before attachment and
        // converts this error into session cleanup/retry, never a pending item.
        assert_eq!(
            early.take("/Client1/Reused", 0).unwrap_err(),
            "resolver creation reused a retired object path"
        );
    }
}
