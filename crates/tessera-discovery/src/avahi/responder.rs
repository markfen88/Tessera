//! The responder actor of the Avahi backend (see the module docs of
//! [`super`]).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::proxies::{
    EntryGroup, GROUP_COLLISION, GROUP_ESTABLISHED, GROUP_FAILURE, PROTO_INET, PROTO_INET6,
    PUBLISH_NO_REVERSE, SERVER_RUNNING, ServerProxy,
};
use super::{Cmd, Owner, bus_err, ifindexes, protocol, signal_rule, signal_stream};
use crate::backoff::ConflictLimiter;
use crate::config::DiscoveryConfig;
use crate::error::Error;
use crate::model::{ConflictScope, DeviceServices, ResponderEvent, ServiceId, ServiceSpec};
use crate::provisional::DOMAIN;

/// How often a name is re-chosen before giving up.
const MAX_RENAMES: u32 = 32;
/// How long `start` waits for the server to reach `RUNNING`.
const SERVER_WAIT: Duration = Duration::from_secs(15);

/// Identifies the group a state change belongs to. `gen` is unique per group
/// so signals of groups that were already replaced can be ignored.
#[derive(Debug, Clone)]
pub(crate) struct Tag {
    device: String,
    /// `None` for the address group of the device.
    service: Option<ServiceId>,
    generation: u64,
}

#[derive(Debug)]
struct Group {
    proxy: EntryGroup,
    generation: u64,
}

impl Group {
    /// Frees the group on the daemon (withdraws its records, sending
    /// goodbyes).
    async fn free(self) {
        if let Err(e) = self.proxy.free().await {
            tracing::debug!("freeing an Avahi entry group failed: {e}");
        }
    }
}

#[derive(Debug)]
struct Svc {
    spec: ServiceSpec,
    /// Instance name chosen after a collision.
    assigned: Option<String>,
    group: Option<Group>,
    announced: bool,
}

impl Svc {
    fn instance(&self) -> &str {
        self.assigned.as_deref().unwrap_or(&self.spec.instance)
    }
}

#[derive(Debug)]
struct Dev {
    spec: DeviceServices,
    /// Host label chosen after a collision.
    host_assigned: Option<String>,
    addr_group: Option<Group>,
    svcs: Vec<Svc>,
    /// Interface indexes the device was published on.
    published_idxs: Vec<i32>,
    /// Addresses the host name was published with (sorted).
    published_addrs: Vec<(i32, IpAddr)>,
}

impl Dev {
    fn host_label(&self) -> &str {
        self.host_assigned.as_deref().unwrap_or(&self.spec.hostname)
    }
    fn host_name(&self) -> String {
        format!("{}.{}", self.host_label(), DOMAIN.trim_end_matches('.'))
    }
}

/// Why publishing failed.
#[derive(Debug)]
enum PubErr {
    Collision,
    Other(Error),
}

fn classify(e: zbus::Error) -> PubErr {
    if let zbus::Error::MethodError(name, _, _) = &e {
        if name.as_str().ends_with("CollisionError") {
            return PubErr::Collision;
        }
    }
    PubErr::Other(bus_err(e))
}

pub(crate) type Spawned = (
    mpsc::Sender<Cmd>,
    mpsc::Receiver<ResponderEvent>,
    Arc<AtomicU64>,
    JoinHandle<()>,
);

/// Starts the actor; returns once the server is running.
pub(crate) async fn spawn(
    conn: zbus::Connection,
    cfg: Arc<DiscoveryConfig>,
    mut owner_rx: watch::Receiver<Owner>,
) -> Result<Spawned, Error> {
    let server = ServerProxy::builder(&conn)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|e| Error::AvahiUnavailable(e.to_string()))?;
    // Subscribe first so a state change between the two calls is not lost.
    let mut states = server
        .receive_state_changed()
        .await
        .map_err(|e| Error::AvahiUnavailable(e.to_string()))?;
    let mut state = server
        .get_state()
        .await
        .map_err(|e| Error::AvahiUnavailable(e.to_string()))?;
    if state != SERVER_RUNNING {
        let wait = async {
            while let Some(sig) = states.next().await {
                if let Ok(args) = sig.args() {
                    if args.state == SERVER_RUNNING {
                        return true;
                    }
                }
            }
            false
        };
        match tokio::time::timeout(SERVER_WAIT, wait).await {
            Ok(true) => state = SERVER_RUNNING,
            _ => {
                return Err(Error::AvahiUnavailable(format!(
                    "avahi-daemon is not running (server state {state})"
                )));
            }
        }
    }
    let (cmd_tx, mut cmd_rx) = mpsc::channel(64);
    // Signals from the daemon go through an unbounded channel: the tasks that
    // forward them must never wait for the actor, which may be inside a long
    // D-Bus call (zbus stops reading the connection while a stream is full, so
    // waiting here would deadlock that very call). The number of signals is
    // bounded by our own groups.
    let (sig_tx, mut sig_rx) = mpsc::unbounded_channel::<Cmd>();
    let (events_tx, events_rx) = mpsc::channel(cfg.event_capacity.max(1));
    let dropped = Arc::new(AtomicU64::new(0));
    // One match rule for the state changes of all our entry groups (the bus
    // limits the rules per connection; a rule per group would cap us at a few
    // hundred). Created before any group exists.
    let mut group_states = signal_stream(
        &conn,
        signal_rule("org.freedesktop.Avahi.EntryGroup", None)?,
    )
    .await?;
    let group_fwd = sig_tx.clone();
    let group_watcher = tokio::spawn(async move {
        while let Some(Ok(msg)) = group_states.next().await {
            if msg
                .header()
                .member()
                .is_none_or(|m| m.as_str() != "StateChanged")
            {
                continue;
            }
            let Some(path) = msg.header().path().map(ToString::to_string) else {
                continue;
            };
            let Ok((state, _error)) = msg.body().deserialize::<(i32, String)>() else {
                continue;
            };
            if group_fwd.send(Cmd::GroupSignal(path, state)).is_err() {
                break;
            }
        }
    });
    let fwd = sig_tx;
    let server_watcher = tokio::spawn(async move {
        while let Some(sig) = states.next().await {
            if let Ok(args) = sig.args() {
                if fwd.send(Cmd::Server(args.state)).is_err() {
                    break;
                }
            }
        }
    });
    let mut actor = Actor {
        conn,
        server,
        cfg,
        devices: HashMap::new(),
        events: events_tx,
        dropped: Arc::clone(&dropped),
        running: state == SERVER_RUNNING,
        next_generation: 0,
        server_watcher,
        group_watcher,
        paths: HashMap::new(),
        limiter: ConflictLimiter::default(),
        seen_owner: *owner_rx.borrow(),
        recheck_error: None,
    };
    let interval = actor.cfg.address_recheck;
    let handle = tokio::spawn(async move {
        let mut recheck = tokio::time::interval(if interval.is_zero() {
            Duration::from_secs(3600)
        } else {
            interval
        });
        recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    if !actor.handle(cmd).await {
                        break;
                    }
                }
                Some(cmd) = sig_rx.recv() => {
                    actor.handle(cmd).await;
                }
                Ok(()) = owner_rx.changed() => {
                    let o = *owner_rx.borrow_and_update();
                    actor.on_owner(o).await;
                }
                _ = recheck.tick(), if !interval.is_zero() => actor.recheck().await,
            }
        }
        actor.free_all().await;
    });
    Ok((cmd_tx, events_rx, dropped, handle))
}

struct Actor {
    conn: zbus::Connection,
    server: ServerProxy<'static>,
    cfg: Arc<DiscoveryConfig>,
    devices: HashMap<String, Dev>,
    events: mpsc::Sender<ResponderEvent>,
    dropped: Arc<AtomicU64>,
    running: bool,
    next_generation: u64,
    /// Forwards the server's state changes; stopped with the actor.
    server_watcher: JoinHandle<()>,
    /// Forwards the state changes of all groups; stopped with the actor.
    group_watcher: JoinHandle<()>,
    /// Object path of every group we created -> what it belongs to.
    paths: HashMap<String, Tag>,
    /// RFC 6762 section 8.1 rate limit for name conflicts.
    limiter: ConflictLimiter,
    /// The last owner state acted on.
    seen_owner: Owner,
    /// The last interface problem reported by `recheck`.
    recheck_error: Option<String>,
}

impl Drop for Actor {
    fn drop(&mut self) {
        self.server_watcher.abort();
        self.group_watcher.abort();
    }
}

fn same(a: &ServiceSpec, b: &ServiceSpec) -> bool {
    a.kind == b.kind && a.instance.eq_ignore_ascii_case(&b.instance)
}

impl Actor {
    fn emit(&self, e: ResponderEvent) {
        if self.events.try_send(e).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Returns false when the actor should stop.
    async fn handle(&mut self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Register(dev, reply) => {
                let _ = reply.send(self.register(dev).await);
            }
            Cmd::Update(dev, reply) => {
                let _ = reply.send(self.update(dev).await);
            }
            Cmd::Withdraw(key, reply) => {
                let _ = reply.send(self.withdraw(&key).await);
            }
            Cmd::Shutdown(done) => {
                self.free_all().await;
                let _ = done.send(());
                return false;
            }
            Cmd::GroupSignal(path, state) => {
                if let Some(tag) = self.paths.get(&path).cloned() {
                    self.on_group_state(tag, state).await;
                }
            }
            Cmd::Server(state) => self.on_server_state(state).await,
        }
        true
    }

    async fn free_all(&mut self) {
        let keys: Vec<String> = self.devices.keys().cloned().collect();
        for k in keys {
            let _ = self.withdraw(&k).await;
        }
    }

    // --- publishing ---------------------------------------------------------

    fn next_gen(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    /// Creates an entry group; its state changes arrive through the shared
    /// signal stream and are mapped back by object path.
    async fn new_group(&mut self, tag: Tag) -> Result<Group, Error> {
        let path = self.server.entry_group_new().await.map_err(bus_err)?;
        let generation = tag.generation;
        let proxy = EntryGroup::new(&self.conn, path);
        self.paths.insert(proxy.path().to_owned(), tag);
        Ok(Group { proxy, generation })
    }

    /// Addresses to publish for the host of `dev`: the explicit list, or those
    /// of the configured (or all non-loopback) interfaces.
    fn host_addresses(&self, dev: &DeviceServices) -> Vec<(i32, IpAddr)> {
        let fam = self.cfg.ip_families;
        let keep = |a: &IpAddr| (a.is_ipv4() && fam.v4()) || (a.is_ipv6() && fam.v6());
        if !dev.addresses.is_empty() {
            // One record per address and configured interface index (never
            // UNSPEC: that would publish on interfaces we were not told to use).
            let idxs = ifindexes(&self.cfg).unwrap_or_default();
            return dev
                .addresses
                .iter()
                .filter(|a| keep(a))
                .flat_map(|a| idxs.iter().map(move |i| (*i, *a)))
                .collect();
        }
        local_addresses(&self.cfg)
            .into_iter()
            .filter(|(_, a)| keep(a))
            .collect()
    }

    async fn publish_addresses(
        &mut self,
        dev: &DeviceServices,
        host: &str,
    ) -> Result<Group, PubErr> {
        let addrs = self.host_addresses(dev);
        if addrs.is_empty() {
            return Err(PubErr::Other(Error::Backend {
                backend: crate::config::BackendKind::Avahi,
                message: "no address to publish for the host name".into(),
            }));
        }
        let generation = self.next_gen();
        let g = self
            .new_group(Tag {
                device: dev.device.clone(),
                service: None,
                generation,
            })
            .await
            .map_err(PubErr::Other)?;
        for (idx, addr) in addrs {
            let proto = if addr.is_ipv4() {
                PROTO_INET
            } else {
                PROTO_INET6
            };
            if let Err(e) = g
                .proxy
                .add_address(idx, proto, PUBLISH_NO_REVERSE, host, &addr.to_string())
                .await
            {
                g.free().await;
                return Err(classify(e));
            }
        }
        if let Err(e) = g.proxy.commit().await {
            g.free().await;
            return Err(classify(e));
        }
        Ok(g)
    }

    async fn publish_service(
        &mut self,
        device: &str,
        spec: &ServiceSpec,
        instance: &str,
        host: &str,
    ) -> Result<Group, PubErr> {
        let idxs = ifindexes(&self.cfg).map_err(PubErr::Other)?;
        let proto = protocol(self.cfg.ip_families);
        let generation = self.next_gen();
        let g = self
            .new_group(Tag {
                device: device.to_owned(),
                service: Some(spec.id()),
                generation,
            })
            .await
            .map_err(PubErr::Other)?;
        let txt = spec.txt.to_strings();
        for idx in idxs {
            if let Err(e) = g
                .proxy
                .add_service(
                    idx,
                    proto,
                    0,
                    instance,
                    spec.kind.type_label(),
                    "",
                    host,
                    spec.port,
                    &txt,
                )
                .await
            {
                g.free().await;
                return Err(classify(e));
            }
        }
        if let Err(e) = g.proxy.commit().await {
            g.free().await;
            return Err(classify(e));
        }
        Ok(g)
    }

    /// (Re-)publishes one service, renaming it while the name is taken.
    async fn ensure_service(&mut self, device: &str, id: &ServiceId) -> Result<(), Error> {
        for _ in 0..MAX_RENAMES {
            let Some((spec, instance, host)) = self.snapshot(device, id) else {
                return Ok(()); // withdrawn meanwhile
            };
            match self.publish_service(device, &spec, &instance, &host).await {
                Ok(g) => {
                    if let Some(svc) = self.svc_mut(device, id) {
                        svc.group = Some(g);
                        svc.announced = false;
                    } else {
                        g.free().await;
                    }
                    return Ok(());
                }
                Err(PubErr::Collision) => self.rename_service(device, id).await?,
                Err(PubErr::Other(e)) => return Err(e),
            }
        }
        Err(Error::Backend {
            backend: crate::config::BackendKind::Avahi,
            message: format!("no free name found for {id}"),
        })
    }

    fn snapshot(&self, device: &str, id: &ServiceId) -> Option<(ServiceSpec, String, String)> {
        let dev = self.devices.get(device)?;
        let svc = dev.svcs.iter().find(|s| s.spec.id() == *id)?;
        Some((svc.spec.clone(), svc.instance().to_owned(), dev.host_name()))
    }

    fn svc_mut(&mut self, device: &str, id: &ServiceId) -> Option<&mut Svc> {
        self.devices
            .get_mut(device)?
            .svcs
            .iter_mut()
            .find(|s| s.spec.id() == *id)
    }

    /// Picks the next alternative name and reports the conflict.
    /// Applies the conflict rate limit: after too many conflicts in a short
    /// time, report it and wait before probing again.
    async fn throttle(&mut self, device: &str) {
        if let Some(wait) = self.limiter.record(Instant::now()) {
            self.fail(
                device,
                Error::Backend {
                    backend: crate::config::BackendKind::Avahi,
                    message: format!(
                        "too many name conflicts; waiting {}s before probing again",
                        wait.as_secs()
                    ),
                },
            );
            tokio::time::sleep(wait).await;
        }
    }

    async fn rename_service(&mut self, device: &str, id: &ServiceId) -> Result<(), Error> {
        self.throttle(device).await;
        let Some((_, current, _)) = self.snapshot(device, id) else {
            return Ok(());
        };
        let alt = self
            .server
            .get_alternative_service_name(&current)
            .await
            .map_err(bus_err)?;
        if let Some(svc) = self.svc_mut(device, id) {
            svc.assigned = Some(alt.clone());
        }
        self.emit(ResponderEvent::Conflict {
            device: device.to_owned(),
            scope: ConflictScope::Instance,
            id: Some(id.clone()),
            requested: id.instance.clone(),
            assigned: alt,
        });
        Ok(())
    }

    /// Publishes the address group, renaming the host while it is taken.
    async fn ensure_addresses(&mut self, device: &str) -> Result<(), Error> {
        for _ in 0..MAX_RENAMES {
            let Some(dev) = self.devices.get(device) else {
                return Ok(());
            };
            let (spec, host) = (dev.spec.clone(), dev.host_name());
            match self.publish_addresses(&spec, &host).await {
                Ok(g) => {
                    let mut addrs = self.host_addresses(&spec);
                    addrs.sort();
                    if let Some(dev) = self.devices.get_mut(device) {
                        dev.published_addrs = addrs;
                        dev.addr_group = Some(g);
                    } else {
                        g.free().await;
                    }
                    return Ok(());
                }
                Err(PubErr::Collision) => self.rename_host(device).await?,
                Err(PubErr::Other(e)) => return Err(e),
            }
        }
        Err(Error::Backend {
            backend: crate::config::BackendKind::Avahi,
            message: format!("no free host name found for device {device}"),
        })
    }

    async fn rename_host(&mut self, device: &str) -> Result<(), Error> {
        self.throttle(device).await;
        let Some(dev) = self.devices.get(device) else {
            return Ok(());
        };
        let current = dev.host_label().to_owned();
        let requested = dev.spec.hostname.clone();
        let first = dev.svcs.first().map(|s| s.spec.id());
        let alt = self
            .server
            .get_alternative_host_name(&current)
            .await
            .map_err(bus_err)?;
        if let Some(dev) = self.devices.get_mut(device) {
            dev.host_assigned = Some(alt.clone());
        }
        self.emit(ResponderEvent::Conflict {
            device: device.to_owned(),
            scope: ConflictScope::Host,
            id: first,
            requested,
            assigned: alt,
        });
        Ok(())
    }

    /// Publishes everything of a device from scratch.
    async fn publish_device(&mut self, device: &str) -> Result<(), Error> {
        let idxs = ifindexes(&self.cfg)?;
        if let Some(dev) = self.devices.get_mut(device) {
            dev.published_idxs = idxs;
        }
        self.ensure_addresses(device).await?;
        let ids: Vec<ServiceId> = self
            .devices
            .get(device)
            .map(|d| d.svcs.iter().map(|s| s.spec.id()).collect())
            .unwrap_or_default();
        for id in ids {
            self.ensure_service(device, &id).await?;
        }
        Ok(())
    }

    /// Frees all groups of a device (keeps the state).
    async fn unpublish_device(&mut self, device: &str) {
        let Some(dev) = self.devices.get_mut(device) else {
            return;
        };
        let mut groups: Vec<Group> = dev.addr_group.take().into_iter().collect();
        for s in &mut dev.svcs {
            groups.extend(s.group.take());
            s.announced = false;
        }
        for g in groups {
            g.free().await;
        }
    }

    // --- commands -------------------------------------------------------------

    async fn register(&mut self, spec: DeviceServices) -> Result<(), Error> {
        if self.devices.contains_key(&spec.device) {
            return Err(Error::AlreadyRegistered(spec.device));
        }
        ifindexes(&self.cfg)?;
        let key = spec.device.clone();
        let svcs = spec
            .services
            .iter()
            .cloned()
            .map(|spec| Svc {
                spec,
                assigned: None,
                group: None,
                announced: false,
            })
            .collect();
        self.devices.insert(
            key.clone(),
            Dev {
                spec,
                host_assigned: None,
                addr_group: None,
                svcs,
                published_idxs: Vec::new(),
                published_addrs: Vec::new(),
            },
        );
        if self.running {
            if let Err(e) = self.publish_device(&key).await {
                self.unpublish_device(&key).await;
                self.devices.remove(&key);
                return Err(e);
            }
        }
        Ok(())
    }

    async fn withdraw(&mut self, key: &str) -> Result<(), Error> {
        if !self.devices.contains_key(key) {
            return Err(Error::NotRegistered(key.to_owned()));
        }
        self.unpublish_device(key).await;
        if let Some(dev) = self.devices.remove(key) {
            for s in dev.svcs {
                self.emit(ResponderEvent::Withdrawn {
                    device: key.to_owned(),
                    id: s.spec.id(),
                });
            }
        }
        Ok(())
    }

    async fn update(&mut self, new: DeviceServices) -> Result<(), Error> {
        let key = new.device.clone();
        let Some(dev) = self.devices.get(&key) else {
            return Err(Error::NotRegistered(key));
        };
        if !dev.spec.hostname.eq_ignore_ascii_case(&new.hostname) {
            return Err(Error::UnsupportedUpdate {
                device: key,
                reason: "the hostname cannot change in place",
            });
        }
        if dev.spec.addresses != new.addresses {
            return Err(Error::UnsupportedUpdate {
                device: key,
                reason: "the addresses cannot change in place",
            });
        }
        // Withdraw services that are gone.
        let gone: Vec<ServiceId> = dev
            .svcs
            .iter()
            .filter(|o| !new.services.iter().any(|s| same(&o.spec, s)))
            .map(|o| o.spec.id())
            .collect();
        for id in gone {
            let removed = self.devices.get_mut(&key).and_then(|d| {
                let i = d.svcs.iter().position(|s| s.spec.id() == id)?;
                Some(d.svcs.remove(i))
            });
            if let Some(mut svc) = removed {
                if let Some(g) = svc.group.take() {
                    g.free().await;
                }
                self.emit(ResponderEvent::Withdrawn {
                    device: key.clone(),
                    id,
                });
            }
        }
        // Changed and new services.
        for s in &new.services {
            let existing = self
                .devices
                .get(&key)
                .and_then(|d| d.svcs.iter().find(|o| same(&o.spec, s)))
                .map(|o| (o.spec.id(), o.spec.port == s.port, o.spec.txt == s.txt));
            match existing {
                None => {
                    if let Some(d) = self.devices.get_mut(&key) {
                        d.svcs.push(Svc {
                            spec: s.clone(),
                            assigned: None,
                            group: None,
                            announced: false,
                        });
                    }
                    if self.running {
                        self.ensure_service(&key, &s.id()).await?;
                    }
                }
                Some((id, same_port, same_txt)) => {
                    if same_port && same_txt {
                        continue;
                    }
                    if same_port {
                        self.update_txt(&key, &id, s).await?;
                    } else {
                        let old = self.svc_mut(&key, &id).and_then(|v| v.group.take());
                        if let Some(g) = old {
                            g.free().await;
                        }
                        if let Some(v) = self.svc_mut(&key, &id) {
                            v.spec = s.clone();
                        }
                        if self.running {
                            self.ensure_service(&key, &id).await?;
                        }
                    }
                }
            }
        }
        if let Some(d) = self.devices.get_mut(&key) {
            d.spec = new;
        }
        Ok(())
    }

    /// TXT-only change: updated in place under the assigned name.
    async fn update_txt(
        &mut self,
        device: &str,
        id: &ServiceId,
        new: &ServiceSpec,
    ) -> Result<(), Error> {
        let idxs = ifindexes(&self.cfg)?;
        let proto = protocol(self.cfg.ip_families);
        let txt = new.txt.to_strings();
        let Some(svc) = self.svc_mut(device, id) else {
            return Ok(());
        };
        svc.spec = new.clone();
        let instance = svc.instance().to_owned();
        if let Some(g) = &svc.group {
            for idx in idxs {
                g.proxy
                    .update_service_txt(idx, proto, 0, &instance, new.kind.type_label(), "", &txt)
                    .await
                    .map_err(bus_err)?;
            }
        }
        Ok(())
    }

    // --- signals ----------------------------------------------------------------

    async fn on_group_state(&mut self, tag: Tag, state: i32) {
        let Some(dev) = self.devices.get_mut(&tag.device) else {
            return;
        };
        match &tag.service {
            Some(id) => {
                let Some(svc) = dev.svcs.iter_mut().find(|s| s.spec.id() == *id) else {
                    return;
                };
                if svc.group.as_ref().map(|g| g.generation) != Some(tag.generation) {
                    return; // stale
                }
                match state {
                    GROUP_ESTABLISHED if !svc.announced => {
                        svc.announced = true;
                        let ev = ResponderEvent::Announced {
                            device: tag.device.clone(),
                            id: id.clone(),
                            instance: svc.instance().to_owned(),
                        };
                        self.emit(ev);
                    }
                    GROUP_COLLISION => {
                        let old = svc.group.take();
                        if let Some(g) = old {
                            g.free().await;
                        }
                        let r = match self.rename_service(&tag.device, id).await {
                            Ok(()) => self.ensure_service(&tag.device, id).await,
                            Err(e) => Err(e),
                        };
                        if let Err(e) = r {
                            self.fail(&tag.device, e);
                        }
                    }
                    GROUP_FAILURE => self.fail(
                        &tag.device,
                        Error::Backend {
                            backend: crate::config::BackendKind::Avahi,
                            message: format!("entry group for {id} failed"),
                        },
                    ),
                    _ => {}
                }
            }
            None => {
                if dev.addr_group.as_ref().map(|g| g.generation) != Some(tag.generation) {
                    return;
                }
                if state == GROUP_COLLISION {
                    // The host name is taken: new host label, publish everything again.
                    self.unpublish_device(&tag.device).await;
                    let r = match self.rename_host(&tag.device).await {
                        Ok(()) => self.publish_device(&tag.device).await,
                        Err(e) => Err(e),
                    };
                    if let Err(e) = r {
                        self.fail(&tag.device, e);
                    }
                } else if state == GROUP_FAILURE {
                    self.fail(
                        &tag.device,
                        Error::Backend {
                            backend: crate::config::BackendKind::Avahi,
                            message: "address entry group failed".into(),
                        },
                    );
                }
            }
        }
    }

    fn fail(&self, device: &str, e: Error) {
        tracing::warn!(device, "Avahi: {e}");
        self.emit(ResponderEvent::Failed {
            device: device.to_owned(),
            message: e.to_string(),
        });
    }

    /// Drops every group of a device without talking to the daemon (it is
    /// gone, or the objects are stale).
    fn forget_device(&mut self, device: &str) {
        if let Some(dev) = self.devices.get_mut(device) {
            dev.addr_group = None;
            for s in &mut dev.svcs {
                s.group = None;
                s.announced = false;
            }
        }
    }

    /// `avahi-daemon` left the bus or came back (possibly restarted so fast
    /// that only the new owner was seen).
    async fn on_owner(&mut self, o: Owner) {
        let new_owner = o.present && o.generation != self.seen_owner.generation;
        self.seen_owner = o;
        if !o.present || new_owner {
            let was_running = self.running;
            self.running = false;
            let keys: Vec<String> = self.devices.keys().cloned().collect();
            for k in &keys {
                self.forget_device(k);
            }
            if was_running {
                for k in &keys {
                    self.fail(
                        k,
                        Error::Backend {
                            backend: crate::config::BackendKind::Avahi,
                            message: "avahi-daemon left the system bus; the services are not advertised until it is back".into(),
                        },
                    );
                }
            }
        }
        if !o.present {
            return;
        }
        // Wait for the (new) server to run, then publish everything again.
        let deadline = Instant::now() + SERVER_WAIT;
        loop {
            if matches!(self.server.get_state().await, Ok(SERVER_RUNNING)) {
                break;
            }
            if Instant::now() > deadline {
                let keys: Vec<String> = self.devices.keys().cloned().collect();
                for k in keys {
                    self.fail(
                        &k,
                        Error::AvahiUnavailable("avahi-daemon did not become ready".into()),
                    );
                }
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        self.running = true;
        let keys: Vec<String> = self.devices.keys().cloned().collect();
        for k in keys {
            if let Err(e) = self.publish_device(&k).await {
                self.fail(&k, e);
            }
        }
    }

    /// Looks for changes of the interfaces and their addresses (there is no
    /// netlink watcher; this runs every `address_recheck`). A changed set of
    /// interfaces republishes the device; changed addresses republish the
    /// host name's address records.
    async fn recheck(&mut self) {
        if !self.running {
            return;
        }
        let idxs = match ifindexes(&self.cfg) {
            Ok(i) => i,
            Err(e) => {
                let msg = e.to_string();
                if self.recheck_error.as_deref() != Some(&msg) {
                    self.recheck_error = Some(msg);
                    let keys: Vec<String> = self.devices.keys().cloned().collect();
                    for k in keys {
                        self.fail(
                            &k,
                            Error::Backend {
                                backend: crate::config::BackendKind::Avahi,
                                message: format!("{e}"),
                            },
                        );
                    }
                }
                return;
            }
        };
        self.recheck_error = None;
        // Forget the paths of groups that are gone.
        let live: std::collections::HashSet<String> = self
            .devices
            .values()
            .flat_map(|d| {
                d.addr_group
                    .iter()
                    .chain(d.svcs.iter().filter_map(|s| s.group.as_ref()))
            })
            .map(|g| g.proxy.path().to_owned())
            .collect();
        self.paths.retain(|p, _| live.contains(p));
        let keys: Vec<String> = self.devices.keys().cloned().collect();
        for k in keys {
            let Some(dev) = self.devices.get(&k) else {
                continue;
            };
            let result = if dev.published_idxs != idxs {
                self.unpublish_device(&k).await;
                self.publish_device(&k).await
            } else if dev.spec.addresses.is_empty() {
                let mut now = self.host_addresses(&dev.spec);
                now.sort();
                if now == dev.published_addrs {
                    continue;
                }
                let old = self.devices.get_mut(&k).and_then(|d| d.addr_group.take());
                if let Some(g) = old {
                    g.free().await;
                }
                self.ensure_addresses(&k).await
            } else {
                continue;
            };
            if let Err(e) = result {
                self.fail(&k, e);
            }
        }
    }

    async fn on_server_state(&mut self, state: i32) {
        if state == SERVER_RUNNING {
            if self.running {
                return;
            }
            self.running = true;
            let keys: Vec<String> = self.devices.keys().cloned().collect();
            for k in keys {
                self.unpublish_device(&k).await;
                if let Err(e) = self.publish_device(&k).await {
                    self.fail(&k, e);
                }
            }
        } else if self.running {
            // Registering / collision / failure: Avahi drops our entries.
            self.running = false;
            let keys: Vec<String> = self.devices.keys().cloned().collect();
            for k in keys {
                self.unpublish_device(&k).await;
            }
        }
    }
}

/// Addresses of the interfaces in use (configured, or the default one).
fn local_addresses(cfg: &DiscoveryConfig) -> Vec<(i32, IpAddr)> {
    use nix::net::if_::InterfaceFlags;
    let mut out = Vec::new();
    let Ok(names) = crate::ifaces::resolve_interfaces(cfg) else {
        return out;
    };
    let Ok(addrs) = nix::ifaddrs::getifaddrs() else {
        return out;
    };
    for ifa in addrs {
        if !ifa.flags.contains(InterfaceFlags::IFF_UP) {
            continue;
        }
        if !names.contains(&ifa.interface_name) {
            continue;
        }
        let Some(sa) = ifa.address else { continue };
        let ip = if let Some(v4) = sa.as_sockaddr_in() {
            IpAddr::V4(v4.ip())
        } else if let Some(v6) = sa.as_sockaddr_in6() {
            IpAddr::V6(v6.ip())
        } else {
            continue;
        };
        let Some(idx) = nix::net::if_::if_nametoindex(ifa.interface_name.as_str())
            .ok()
            .and_then(|i| i32::try_from(i).ok())
        else {
            continue;
        };
        out.push((idx, ip));
    }
    out
}
