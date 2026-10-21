//! The built-in backend, on top of `mdns-sd`.
//!
//! One `ServiceDaemon` (one thread, multicast sockets bound per interface)
//! serves both the responder and the browsers of a [`crate::Discovery`].
//! `mdns-sd` probes names and renames on conflict by itself; we learn about it
//! from the daemon's monitor channel and surface it as
//! [`ResponderEvent::Conflict`].
//!
//! Limits of `mdns-sd` that show through (see the crate docs): record TTLs
//! cannot be set, and empty TXT strings cannot be represented.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use mdns_sd::{
    DaemonEvent, IfKind, RRType, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo, TxtProperty,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::cache::{BrowseCache, SourceView};
use crate::config::{BackendKind, DiscoveryConfig, PortConflict};
use crate::error::Error;
use crate::ifaces;
use crate::model::{
    BrowseEvent, ConflictScope, DeviceServices, ResponderEvent, ServiceKind, ServiceSpec,
};
use crate::names::{full_name, parse_full_name};
use crate::provisional::DOMAIN;
use crate::sink::BrowseShared;
use crate::txt::TxtRecord;
use crate::{BrowseHandle, ResponderEvents};

const UNREGISTER_WAIT: Duration = Duration::from_secs(2);
/// Minimum time between two announcements of one device (see `update`).
const MIN_ANNOUNCE_GAP: Duration = Duration::from_millis(1100);
/// Services are handed to the daemon in chunks of this size, with a short
/// pause in between, so re-announcing a large device is a stream and not a burst.
const ANNOUNCE_CHUNK: usize = 16;
const ANNOUNCE_PAUSE: Duration = Duration::from_millis(20);

fn backend_err(e: impl std::fmt::Display) -> Error {
    Error::Backend {
        backend: BackendKind::Builtin,
        message: e.to_string(),
    }
}

#[derive(Debug)]
struct Svc {
    spec: ServiceSpec,
    /// Lower-case full name as given to the daemon; the daemon keys the
    /// service by it for its whole life, also after a rename. This is
    /// `mdns-sd`'s own key (Unicode lower-casing, unlike DNS, which folds ASCII
    /// only), reproduced here so that `unregister` finds the service.
    original: String,
    /// Instance name chosen after a conflict.
    assigned: Option<String>,
    announced: bool,
}

impl Svc {
    fn new(spec: ServiceSpec) -> Self {
        let original = full_name(spec.kind, &spec.instance).to_lowercase();
        Self {
            spec,
            original,
            assigned: None,
            announced: false,
        }
    }

    fn assigned_full(&self) -> Option<String> {
        self.assigned
            .as_ref()
            .map(|i| full_name(self.spec.kind, i).to_lowercase())
    }

    fn matches_name(&self, full_lower: &str) -> bool {
        self.original == full_lower || self.assigned_full().is_some_and(|a| a == full_lower)
    }
}

#[derive(Debug)]
struct Dev {
    spec: DeviceServices,
    svcs: Vec<Svc>,
    host_assigned: Option<String>,
    /// When services of this device were last handed to the daemon.
    last_announce: Instant,
}

#[derive(Debug, Default)]
struct Shared {
    devices: HashMap<String, Dev>,
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The built-in backend.
pub(crate) struct Builtin {
    daemon: ServiceDaemon,
    cfg: Arc<DiscoveryConfig>,
    shared: Arc<Mutex<Shared>>,
    events_tx: mpsc::Sender<ResponderEvent>,
    events_rx: Mutex<Option<mpsc::Receiver<ResponderEvent>>>,
    events_dropped: Arc<AtomicU64>,
    monitor: JoinHandle<()>,
    /// The interfaces in use (configured, or the default one).
    interfaces: Vec<String>,
    /// Serialises register, update, withdraw and shutdown. `Responder` is
    /// `Clone`, and `update` waits (goodbyes, announcement spacing) between
    /// its steps, so overlapping operations would interleave.
    ops: tokio::sync::Mutex<()>,
    kind_browses: Arc<KindBrowses>,
    next_subscriber: AtomicU64,
}

impl std::fmt::Debug for Builtin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Builtin")
            .field("interfaces", &self.cfg.interfaces)
            .finish_non_exhaustive()
    }
}

impl Drop for Builtin {
    fn drop(&mut self) {
        self.monitor.abort();
        // Also releases the producers, which ends every Browser's stream.
        for (_, kb) in lock(&self.kind_browses).drain() {
            kb.task.abort();
        }
        // Best effort when `shutdown` was not awaited: queue the goodbyes and
        // stop the daemon thread (it would otherwise run on, still answering).
        for dev in lock(&self.shared).devices.values() {
            for svc in &dev.svcs {
                let _ = self.daemon.unregister(&svc.original);
            }
        }
        let _ = self.daemon.shutdown();
    }
}

fn warn_missing_interfaces(names: &[String]) {
    for n in names {
        if nix::net::if_::if_nametoindex(n.as_str()).is_err() {
            tracing::warn!(interface = %n, "configured interface does not exist (yet)");
        }
    }
}

impl Builtin {
    /// Starts the daemon thread and the monitor task. Must run inside a
    /// tokio runtime.
    pub(crate) fn start(cfg: Arc<DiscoveryConfig>) -> Result<Self, Error> {
        let interfaces = ifaces::resolve_interfaces(&cfg)?;
        check_port_conflict(&cfg, &interfaces)?;
        let daemon = ServiceDaemon::new().map_err(backend_err)?;
        // Bind only what was configured (ADR 0010): switch everything off,
        // then enable the named interfaces.
        warn_missing_interfaces(&interfaces);
        daemon.disable_interface(IfKind::All).map_err(backend_err)?;
        for name in &interfaces {
            daemon
                .enable_interface(IfKind::Name(name.clone()))
                .map_err(backend_err)?;
        }
        if !cfg.ip_families.v6() {
            daemon
                .disable_interface(IfKind::IPv6)
                .map_err(backend_err)?;
        }
        if !cfg.ip_families.v4() {
            daemon
                .disable_interface(IfKind::IPv4)
                .map_err(backend_err)?;
        }
        daemon
            .set_multicast_loop_v4(cfg.multicast_loopback)
            .map_err(backend_err)?;
        daemon
            .set_multicast_loop_v6(cfg.multicast_loopback)
            .map_err(backend_err)?;

        let (events_tx, events_rx) = mpsc::channel(cfg.event_capacity.max(1));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let events_dropped = Arc::new(AtomicU64::new(0));
        let mon_rx = daemon.monitor().map_err(backend_err)?;
        let monitor = tokio::spawn(monitor_task(
            mon_rx,
            Arc::clone(&shared),
            events_tx.clone(),
            Arc::clone(&events_dropped),
        ));
        Ok(Self {
            daemon,
            cfg,
            shared,
            events_tx,
            events_rx: Mutex::new(Some(events_rx)),
            events_dropped,
            monitor,
            interfaces,
            ops: tokio::sync::Mutex::new(()),
            kind_browses: Arc::new(Mutex::new(HashMap::new())),
            next_subscriber: AtomicU64::new(0),
        })
    }

    pub(crate) fn take_events(&self) -> Option<ResponderEvents> {
        lock(&self.events_rx).take().map(|rx| ResponderEvents {
            rx,
            dropped: Arc::clone(&self.events_dropped),
        })
    }

    fn info(&self, dev: &DeviceServices, s: &ServiceSpec) -> Result<ServiceInfo, Error> {
        let props: Vec<TxtProperty> = s
            .txt
            .entries()
            .iter()
            .map(|e| match &e.value {
                Some(v) => TxtProperty::from((e.key.as_str(), v.as_slice())),
                None => TxtProperty::from(e.key.as_str()),
            })
            .collect();
        let mut info = if dev.addresses.is_empty() {
            ServiceInfo::new(
                &s.kind.type_domain(),
                &s.instance,
                &dev.host_fqdn(),
                (),
                s.port,
                props,
            )
            .map_err(backend_err)?
            .enable_addr_auto()
        } else {
            ServiceInfo::new(
                &s.kind.type_domain(),
                &s.instance,
                &dev.host_fqdn(),
                dev.addresses.as_slice(),
                s.port,
                props,
            )
            .map_err(backend_err)?
        };
        info.set_interfaces(
            self.interfaces
                .iter()
                .map(|n| IfKind::Name(n.clone()))
                .collect(),
        );
        Ok(info)
    }

    /// Runs `op` as its own task holding the operations lock. If the caller
    /// drops its future (a timeout, a cancelled request) the operation still
    /// runs to the end, so bookkeeping and the daemon never disagree.
    async fn run_op<F, Fut, T>(self: &Arc<Self>, op: F) -> Result<T, Error>
    where
        F: FnOnce(Arc<Self>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T, Error>> + Send + 'static,
        T: Send + 'static,
    {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let guard = Arc::clone(&this);
            let _op = guard.ops.lock().await;
            op(this).await
        })
        .await
        .map_err(|e| backend_err(format!("operation aborted: {e}")))?
    }

    pub(crate) async fn register(self: &Arc<Self>, dev: DeviceServices) -> Result<(), Error> {
        self.run_op(move |this| async move { this.register_locked(dev) })
            .await
    }

    fn register_locked(&self, dev: DeviceServices) -> Result<(), Error> {
        dev.validate(&self.cfg.limits)?;
        let mut sh = lock(&self.shared);
        if sh.devices.contains_key(&dev.device) {
            return Err(Error::AlreadyRegistered(dev.device));
        }
        let infos = dev
            .services
            .iter()
            .map(|s| self.info(&dev, s))
            .collect::<Result<Vec<_>, _>>()?;
        let mut done: Vec<String> = Vec::new();
        for info in infos {
            let name = info.get_fullname().to_lowercase();
            if let Err(e) = self.daemon.register(info) {
                // Roll back what was registered so far.
                for n in done {
                    let _ = self.daemon.unregister(&n);
                }
                return Err(backend_err(e));
            }
            done.push(name);
        }
        let svcs = dev.services.iter().cloned().map(Svc::new).collect();
        sh.devices.insert(
            dev.device.clone(),
            Dev {
                spec: dev,
                svcs,
                host_assigned: None,
                last_announce: Instant::now(),
            },
        );
        Ok(())
    }

    pub(crate) async fn update(self: &Arc<Self>, new: DeviceServices) -> Result<(), Error> {
        self.run_op(move |this| async move { this.update_locked(new).await })
            .await
    }

    async fn update_locked(&self, new: DeviceServices) -> Result<(), Error> {
        new.validate(&self.cfg.limits)?;
        let resends_before = self.unregister_resends().await;
        let mut events = Vec::new();
        let mut waits = Vec::new();
        let pending;
        let last;
        let key = new.device.clone();
        {
            let mut sh = lock(&self.shared);
            let dev = sh
                .devices
                .get_mut(&new.device)
                .ok_or_else(|| Error::NotRegistered(new.device.clone()))?;
            last = dev.last_announce;
            if !dev.spec.hostname.eq_ignore_ascii_case(&new.hostname) {
                return Err(Error::UnsupportedUpdate {
                    device: new.device,
                    reason: "the hostname cannot change in place",
                });
            }
            if dev.spec.addresses != new.addresses {
                return Err(Error::UnsupportedUpdate {
                    device: new.device,
                    reason: "the addresses cannot change in place",
                });
            }
            // Build every ServiceInfo first so an error leaves things as they were.
            // mdns-sd puts a goodbye (TTL 0) for the shared host address on the
            // wire with every unregistered service, which makes peers drop the
            // remaining services of the device. After a withdrawal the
            // remaining services are therefore announced again.
            let removed_any = dev
                .svcs
                .iter()
                .any(|o| !new.services.iter().any(|s| same_service(&o.spec, s)));
            let mut to_register = Vec::new();
            for s in &new.services {
                let old = dev.svcs.iter().find(|o| same_service(&o.spec, s));
                if removed_any || old.is_none_or(|o| o.spec.port != s.port || o.spec.txt != s.txt) {
                    to_register.push(self.info(&new, s)?);
                }
            }
            // Withdraw services that are gone.
            let mut kept = Vec::new();
            for svc in dev.svcs.drain(..) {
                if new.services.iter().any(|s| same_service(&svc.spec, s)) {
                    kept.push(svc);
                } else {
                    match self.daemon.unregister(&svc.original) {
                        Ok(rx) => waits.push(rx),
                        Err(e) => tracing::warn!("unregister failed: {e}"),
                    }
                    events.push(ResponderEvent::Withdrawn {
                        device: new.device.clone(),
                        id: svc.spec.id(),
                    });
                }
            }
            dev.svcs = kept;
            // Bring the bookkeeping in line with `new`.
            let mut svcs = Vec::with_capacity(new.services.len());
            for s in &new.services {
                match dev.svcs.iter().position(|o| same_service(&o.spec, s)) {
                    Some(i) => {
                        let mut o = dev.svcs.swap_remove(i);
                        if o.spec.port != s.port || o.spec.txt != s.txt {
                            o.announced = false;
                        }
                        o.spec = s.clone();
                        svcs.push(o);
                    }
                    None => svcs.push(Svc::new(s.clone())),
                }
            }
            dev.svcs = svcs;
            dev.spec = new;
            pending = to_register;
        }
        // Goodbyes first: re-announcing right behind them confuses caches.
        let withdrew = !waits.is_empty();
        wait_unregistered(waits).await;
        if withdrew {
            // `mdns-sd` repeats every goodbye about 120 ms later. A
            // re-announcement that comes before the repeat is wiped out by it
            // (the repeat carries the shared host address with TTL 0), and peers
            // then drop the remaining services. Under load the repeat can be
            // late, so wait for it instead of for a fixed time.
            self.settle_goodbyes(resends_before).await;
        }
        // The withdrawals are done: report them whatever happens next.
        self.emit(events);
        if let Some(h) = &self.cfg.test_hooks {
            h.checkpoint().await;
        }
        if !pending.is_empty() {
            // mdns-sd multicasts a given record at most once per second
            // (RFC 6762 section 6) and silently drops the rest, which leaves
            // peers with half an update. Space announcements of one device.
            tokio::time::sleep_until(tokio::time::Instant::from_std(last + MIN_ANNOUNCE_GAP)).await;
            for (n, info) in pending.into_iter().enumerate() {
                if n > 0 && n % ANNOUNCE_CHUNK == 0 {
                    tokio::time::sleep(ANNOUNCE_PAUSE).await;
                }
                self.daemon.register(info).map_err(backend_err)?;
            }
            if let Some(d) = lock(&self.shared).devices.get_mut(&key) {
                d.last_announce = Instant::now();
            }
        }
        Ok(())
    }

    /// The daemon's count of repeated goodbyes sent so far.
    async fn unregister_resends(&self) -> i64 {
        let Ok(rx) = self.daemon.get_metrics() else {
            return 0;
        };
        tokio::time::timeout(Duration::from_secs(1), rx.recv_async())
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(|m| m.get("unregister-resend").copied())
            .unwrap_or(0)
    }

    /// Waits until the goodbye repeats of the services just unregistered have
    /// been sent: the counter rose above `before` and stopped rising. Gives up
    /// after a few seconds (no packet is sent for a service without addresses).
    async fn settle_goodbyes(&self, before: i64) {
        let start = Instant::now();
        let mut last = before;
        let mut stable = 0;
        while start.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let n = self.unregister_resends().await;
            if n > before {
                stable = if n == last { stable + 1 } else { 0 };
                if stable >= 2 {
                    return;
                }
            } else if start.elapsed() > Duration::from_millis(1500) {
                return;
            }
            last = n;
        }
    }

    pub(crate) async fn withdraw(self: &Arc<Self>, device: &str) -> Result<(), Error> {
        let device = device.to_owned();
        self.run_op(move |this| async move { this.withdraw_locked(&device).await })
            .await
    }

    async fn withdraw_locked(&self, device: &str) -> Result<(), Error> {
        let dev = lock(&self.shared)
            .devices
            .remove(device)
            .ok_or_else(|| Error::NotRegistered(device.to_owned()))?;
        let mut waits = Vec::new();
        let mut events = Vec::new();
        for svc in &dev.svcs {
            match self.daemon.unregister(&svc.original) {
                Ok(rx) => waits.push(rx),
                Err(e) => tracing::warn!("unregister failed: {e}"),
            }
            events.push(ResponderEvent::Withdrawn {
                device: device.to_owned(),
                id: svc.spec.id(),
            });
        }
        wait_unregistered(waits).await;
        self.emit(events);
        Ok(())
    }

    fn emit(&self, events: Vec<ResponderEvent>) {
        for e in events {
            send_event(&self.events_tx, &self.events_dropped, e);
        }
    }

    /// `mdns-sd` keeps one listener per service type (a second `browse` for
    /// the same type replaces the first, and `stop_browse` ends it for
    /// everybody). So there is exactly one daemon browse per kind, and every
    /// [`crate::Browser`] is a subscriber of it; a subscriber that joins late
    /// is brought up to date from what the browse already knows.
    pub(crate) async fn browse(
        &self,
        kinds: &[ServiceKind],
    ) -> Result<(mpsc::Receiver<BrowseEvent>, BrowseHandle), Error> {
        let (shared, view, rx) =
            BrowseShared::new(self.cfg.max_instances_per_kind, self.cfg.event_capacity);
        let id = self.next_subscriber.fetch_add(1, Ordering::Relaxed);
        let mut joined: Vec<ServiceKind> = Vec::new();
        for &kind in kinds {
            if joined.contains(&kind) {
                continue;
            }
            if let Err(e) = self.join(kind, id, &shared) {
                for k in joined {
                    leave(&self.kind_browses, &self.daemon, k, id);
                }
                return Err(e);
            }
            joined.push(kind);
        }
        let (browses, daemon) = (Arc::clone(&self.kind_browses), self.daemon.clone());
        drop(shared);
        let handle = BrowseHandle::new(
            view,
            Box::new(move || {
                for k in joined {
                    leave(&browses, &daemon, k, id);
                }
            }),
        );
        Ok((rx, handle))
    }

    fn join(&self, kind: ServiceKind, id: u64, shared: &BrowseShared) -> Result<(), Error> {
        let mut map = lock(&self.kind_browses);
        if let Some(kb) = map.get_mut(&kind) {
            for rs in kb.known.values() {
                shared.update(|c| resolved_to_cache(c, kind, rs));
            }
            kb.subs.push((id, shared.clone()));
            return Ok(());
        }
        let events = self
            .daemon
            .browse(&kind.type_domain())
            .map_err(backend_err)?;
        let task = tokio::spawn(kind_task(
            events,
            kind,
            Arc::clone(&self.kind_browses),
            self.cfg.max_instances_per_kind,
        ));
        map.insert(
            kind,
            KindBrowse {
                subs: vec![(id, shared.clone())],
                known: HashMap::new(),
                task,
            },
        );
        Ok(())
    }

    pub(crate) async fn shutdown(&self) {
        let _op = self.ops.lock().await;
        let devices: Vec<String> = lock(&self.shared).devices.keys().cloned().collect();
        for d in devices {
            let _ = self.withdraw_locked(&d).await;
        }
        if let Ok(rx) = self.daemon.shutdown() {
            let _ = tokio::time::timeout(UNREGISTER_WAIT, rx.recv_async()).await;
        }
        // Ends every Browser's stream: the producers are dropped here.
        let ended: Vec<KindBrowse> = lock(&self.kind_browses).drain().map(|(_, kb)| kb).collect();
        for kb in ended {
            kb.task.abort();
        }
    }
}

fn same_service(a: &ServiceSpec, b: &ServiceSpec) -> bool {
    a.kind == b.kind && a.instance.eq_ignore_ascii_case(&b.instance)
}

async fn wait_unregistered(waits: Vec<mdns_sd::Receiver<mdns_sd::UnregisterStatus>>) {
    for rx in waits {
        // Without this the goodbye may not be on the wire before a process exits.
        let _ = tokio::time::timeout(UNREGISTER_WAIT, rx.recv_async()).await;
    }
}

fn send_event(tx: &mpsc::Sender<ResponderEvent>, dropped: &AtomicU64, e: ResponderEvent) {
    if tx.try_send(e).is_err() {
        // Nobody reads, or the reader is far behind: never stall the backend.
        dropped.fetch_add(1, Ordering::Relaxed);
    }
}

async fn monitor_task(
    rx: mdns_sd::Receiver<DaemonEvent>,
    shared: Arc<Mutex<Shared>>,
    tx: mpsc::Sender<ResponderEvent>,
    dropped: Arc<AtomicU64>,
) {
    while let Ok(ev) = rx.recv_async().await {
        let out = {
            let mut sh = lock(&shared);
            handle_daemon_event(&mut sh, &ev)
        };
        for e in out {
            send_event(&tx, &dropped, e);
        }
    }
}

fn handle_daemon_event(sh: &mut Shared, ev: &DaemonEvent) -> Vec<ResponderEvent> {
    let mut out = Vec::new();
    match ev {
        DaemonEvent::Announce(full, _) => {
            let full = full.to_lowercase();
            for (key, dev) in &mut sh.devices {
                for svc in &mut dev.svcs {
                    if !svc.announced && svc.matches_name(&full) {
                        svc.announced = true;
                        out.push(ResponderEvent::Announced {
                            device: key.clone(),
                            id: svc.spec.id(),
                            instance: svc
                                .assigned
                                .clone()
                                .unwrap_or_else(|| svc.spec.instance.clone()),
                        });
                    }
                }
            }
        }
        DaemonEvent::NameChange(nc) => {
            let original = nc.original.to_lowercase();
            if matches!(nc.rr_type, RRType::A | RRType::AAAA) {
                for (key, dev) in &mut sh.devices {
                    if dev.spec.host_fqdn().to_lowercase() != original {
                        continue;
                    }
                    let new_host = nc
                        .new_name
                        .strip_suffix(&format!(".{DOMAIN}"))
                        .unwrap_or(&nc.new_name)
                        .to_owned();
                    if dev.host_assigned.as_deref() != Some(new_host.as_str()) {
                        dev.host_assigned = Some(new_host.clone());
                        out.push(ResponderEvent::Conflict {
                            device: key.clone(),
                            scope: ConflictScope::Host,
                            id: dev.svcs.first().map(|s| s.spec.id()),
                            requested: dev.spec.hostname.clone(),
                            assigned: new_host,
                        });
                    }
                }
            } else if let Some(new_id) = parse_full_name(&nc.new_name) {
                for (key, dev) in &mut sh.devices {
                    for svc in &mut dev.svcs {
                        if svc.original == original
                            && svc.assigned.as_deref() != Some(new_id.instance.as_str())
                        {
                            svc.assigned = Some(new_id.instance.clone());
                            out.push(ResponderEvent::Conflict {
                                device: key.clone(),
                                scope: ConflictScope::Instance,
                                id: Some(svc.spec.id()),
                                requested: svc.spec.instance.clone(),
                                assigned: new_id.instance.clone(),
                            });
                        }
                    }
                }
            }
        }
        DaemonEvent::Error(e) => {
            tracing::warn!("mDNS daemon error: {e}");
            for key in sh.devices.keys() {
                out.push(ResponderEvent::Failed {
                    device: key.clone(),
                    message: e.to_string(),
                });
            }
        }
        _ => {}
    }
    out
}

/// One daemon browse of one kind, shared by all subscribers.
#[derive(Debug)]
struct KindBrowse {
    subs: Vec<(u64, BrowseShared)>,
    /// Resolved services by lower-case full name, to bring late subscribers up
    /// to date. Capped like the caches.
    known: HashMap<String, mdns_sd::ResolvedService>,
    task: JoinHandle<()>,
}

type KindBrowses = Mutex<HashMap<ServiceKind, KindBrowse>>;

/// Removes subscriber `id`; the last one ends the daemon browse.
fn leave(map: &KindBrowses, daemon: &ServiceDaemon, kind: ServiceKind, id: u64) {
    let mut m = lock(map);
    let Some(kb) = m.get_mut(&kind) else { return };
    kb.subs.retain(|(i, _)| *i != id);
    if kb.subs.is_empty() {
        if let Some(kb) = m.remove(&kind) {
            kb.task.abort();
            let _ = daemon.stop_browse(&kind.type_domain());
        }
    }
}

fn apply(c: &mut BrowseCache, kind: ServiceKind, ev: &ServiceEvent) -> Option<BrowseEvent> {
    match ev {
        ServiceEvent::ServiceResolved(rs) => resolved_to_cache(c, kind, rs),
        ServiceEvent::ServiceRemoved(_, full) => parse_full_name(full)
            .filter(|id| id.kind == kind)
            .and_then(|id| c.on_removed(&id, SOURCE)),
        _ => None,
    }
}

/// Drains the daemon's browse channel and fans the events out. It must never
/// wait for a consumer of a [`crate::Browser`]: the daemon thread blocks on a
/// full channel and it also answers for the responder.
async fn kind_task(
    rx: mdns_sd::Receiver<ServiceEvent>,
    kind: ServiceKind,
    map: Arc<KindBrowses>,
    cap: usize,
) {
    while let Ok(ev) = rx.recv_async().await {
        let mut m = lock(&map);
        let Some(kb) = m.get_mut(&kind) else { return };
        match &ev {
            ServiceEvent::ServiceResolved(rs) if rs.is_valid() => {
                let key = rs.fullname.to_lowercase();
                if kb.known.len() < cap || kb.known.contains_key(&key) {
                    kb.known.insert(key, (**rs).clone());
                }
            }
            ServiceEvent::ServiceRemoved(_, full) => {
                kb.known.remove(&full.to_lowercase());
            }
            _ => {}
        }
        kb.subs.retain(|(_, sh)| sh.update(|c| apply(c, kind, &ev)));
    }
    // The daemon is gone (shut down): end every Browser's stream.
    lock(&map).remove(&kind);
}

/// Warns about, or refuses, another process on UDP port 5353 (two responders
/// answer every query twice; sockets can share the port with `SO_REUSEADDR`,
/// so this is not fatal by default).
fn check_port_conflict(cfg: &DiscoveryConfig, interfaces: &[String]) -> Result<(), Error> {
    let foreign = ifaces::foreign_mdns_sockets(&ifaces::interface_addresses(interfaces));
    if foreign.is_empty() {
        return Ok(());
    }
    let what = foreign
        .iter()
        .map(|s| format!("{} (uid {})", s.local, s.uid))
        .collect::<Vec<_>>()
        .join(", ");
    match cfg.port_conflict {
        PortConflict::Refuse => Err(Error::PortInUse(what)),
        PortConflict::Warn => {
            tracing::warn!(
                "another process has UDP port 5353 on our interfaces: {what}; \
                 the built-in responder will share it and both will answer"
            );
            Ok(())
        }
    }
}

const SOURCE: &str = "mdns-sd";

fn resolved_to_cache(
    c: &mut BrowseCache,
    kind: ServiceKind,
    rs: &mdns_sd::ResolvedService,
) -> Option<BrowseEvent> {
    if !rs.is_valid() {
        return None;
    }
    let id = parse_full_name(&rs.fullname).filter(|id| id.kind == kind)?;
    let addresses = rs
        .addresses
        .iter()
        .map(|a| match a {
            ScopedIp::V4(v) => IpAddr::V4(*v.addr()),
            ScopedIp::V6(v) => IpAddr::V6(*v.addr()),
            _ => IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        })
        .filter(|a| !a.is_unspecified())
        .collect();
    let strings: Vec<Vec<u8>> = rs
        .txt_properties
        .iter()
        .map(|p| {
            let mut s = p.key().as_bytes().to_vec();
            if let Some(v) = p.val() {
                s.push(b'=');
                s.extend_from_slice(v);
            }
            s
        })
        .collect();
    let txt: TxtRecord = TxtRecord::from_strings(strings.iter().map(Vec::as_slice)).record;
    c.on_resolved(
        id,
        SOURCE.to_owned(),
        SourceView {
            host: rs.host.clone(),
            port: rs.port,
            addresses,
            txt,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ServiceSpec;

    fn dev_with(spec_instance: &str) -> Shared {
        let spec = ServiceSpec {
            kind: ServiceKind::Arc,
            instance: spec_instance.into(),
            port: 4440,
            txt: TxtRecord::new(),
        };
        let d = DeviceServices {
            device: "d".into(),
            hostname: "studio-a".into(),
            addresses: vec![],
            services: vec![spec.clone()],
        };
        let mut sh = Shared::default();
        sh.devices.insert(
            "d".into(),
            Dev {
                spec: d,
                svcs: vec![Svc::new(spec)],
                host_assigned: None,
                last_announce: Instant::now(),
            },
        );
        sh
    }

    #[test]
    fn announce_is_reported_once() {
        let mut sh = dev_with("Studio.A");
        let ev = DaemonEvent::Announce("studio\\.a._netaudio-arc._udp.local.".into(), "x".into());
        let out = handle_daemon_event(&mut sh, &ev);
        assert_eq!(out.len(), 1);
        assert!(handle_daemon_event(&mut sh, &ev).is_empty());
    }
}
