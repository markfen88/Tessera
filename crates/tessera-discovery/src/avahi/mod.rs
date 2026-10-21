//! The Avahi backend: `avahi-daemon` driven over the D-Bus system bus with
//! `zbus`.
//!
//! Responder side: one actor task owns all state. Every service instance has
//! its own entry group (so it can be withdrawn, renamed or re-added on its
//! own) and every device has one more group publishing the address records of
//! its host name (`AddAddress`, without reverse records, so the machine's own
//! name is not claimed). Name collisions come back either as an error from
//! `AddService` (the name is taken by another local entry) or as a
//! `Collision` state of the group (the name is taken on the network); both
//! lead to `GetAlternativeServiceName`, a [`ResponderEvent::Conflict`] and a
//! re-registration under the new name. After that the assigned name is used
//! for every later operation.
//!
//! Browse side: a `ServiceBrowser` per kind (and interface) plus a
//! `ServiceResolver` per found instance, whose `Found` signal repeats when the
//! records change; that is what turns into [`BrowseEvent::Updated`].
//!
//! Avahi never falls back: if the daemon vanishes the backend reports
//! failures, it does not start a second responder.

mod browse;
mod proxies;
mod responder;

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::config::{BackendKind, DiscoveryConfig, IpFamilies};
use crate::error::Error;
use crate::model::{BrowseEvent, DeviceServices, ResponderEvent, ServiceKind};
use crate::{BrowseHandle, ResponderEvents};

use proxies::{PROTO_INET, PROTO_INET6, UNSPEC};

fn bus_err(e: impl std::fmt::Display) -> Error {
    Error::Backend {
        backend: BackendKind::Avahi,
        message: e.to_string(),
    }
}

/// `AvahiProtocol` to use for browsing and publishing.
fn protocol(f: IpFamilies) -> i32 {
    match f {
        IpFamilies::V4 => PROTO_INET,
        IpFamilies::V6 => PROTO_INET6,
        IpFamilies::Both => UNSPEC,
    }
}

/// Interface indexes to publish and browse on: the configured interface names,
/// or, if none are configured, every interface that is up, multicast-capable
/// and not loopback (so loopback is used only if listed, ADR 0010). The
/// default list is taken when the call is made; interfaces that appear later
/// are not picked up.
fn ifindexes(cfg: &DiscoveryConfig) -> Result<Vec<i32>, Error> {
    crate::ifaces::resolve_interfaces(cfg)?
        .iter()
        .map(|n| {
            nix::net::if_::if_nametoindex(n.as_str())
                .ok()
                .and_then(|i| i32::try_from(i).ok())
                .ok_or_else(|| Error::UnknownInterface(n.clone()))
        })
        .collect()
}

/// Whether `avahi-daemon` currently owns its bus name, and how often that
/// changed (so a quick stop and start is noticed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Owner {
    pub present: bool,
    pub generation: u64,
}

/// Follows `NameOwnerChanged` for the Avahi name. The subscription is made
/// before this returns, so no change after `start` is missed.
async fn watch_owner(
    conn: &zbus::Connection,
) -> Result<(watch::Receiver<Owner>, JoinHandle<()>), Error> {
    use futures_util::StreamExt;
    let dbus = zbus::fdo::DBusProxy::new(conn).await.map_err(bus_err)?;
    let mut stream = dbus
        .receive_name_owner_changed_with_args(&[(0, crate::select::AVAHI_BUS_NAME)])
        .await
        .map_err(bus_err)?;
    let (tx, rx) = watch::channel(Owner {
        present: true,
        generation: 0,
    });
    let task = tokio::spawn(async move {
        let _dbus = dbus;
        while let Some(sig) = stream.next().await {
            if let Ok(args) = sig.args() {
                let present = args.new_owner().is_some();
                tx.send_modify(|o| {
                    o.present = present;
                    o.generation += 1;
                });
            }
        }
    });
    Ok((rx, task))
}

const AVAHI: &str = "org.freedesktop.Avahi";

/// A match rule for signals of one Avahi interface, optionally limited to a
/// path namespace. Path-less on purpose (see [`proxies::Object`]).
pub(crate) fn signal_rule(
    interface: &str,
    namespace: Option<&str>,
) -> Result<zbus::MatchRule<'static>, Error> {
    let b = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(AVAHI)
        .map_err(bus_err)?
        .interface(interface.to_owned())
        .map_err(bus_err)?;
    let b = match namespace {
        Some(ns) => b.path_namespace(ns.to_owned()).map_err(bus_err)?,
        None => b,
    };
    Ok(b.build())
}

pub(crate) async fn signal_stream(
    conn: &zbus::Connection,
    rule: zbus::MatchRule<'static>,
) -> Result<zbus::MessageStream, Error> {
    zbus::MessageStream::for_match_rule(rule, conn, None)
        .await
        .map_err(bus_err)
}

type Reply = oneshot::Sender<Result<(), Error>>;

/// Messages to the responder actor.
#[derive(Debug)]
pub(crate) enum Cmd {
    Register(DeviceServices, Reply),
    Update(DeviceServices, Reply),
    Withdraw(String, Reply),
    Shutdown(oneshot::Sender<()>),
    /// An entry group (by object path) changed state.
    GroupSignal(String, i32),
    /// The server changed state.
    Server(i32),
}

/// The Avahi backend.
#[derive(Debug)]
pub(crate) struct Avahi {
    cfg: Arc<DiscoveryConfig>,
    /// Connection of the browsers. Separate from the responder's, so a slow
    /// or busy browse cannot hold up the responder (zbus stops reading a
    /// connection while one of its streams is full).
    browse_conn: zbus::Connection,
    cmd: mpsc::Sender<Cmd>,
    events_rx: Mutex<Option<mpsc::Receiver<ResponderEvent>>>,
    events_dropped: Arc<AtomicU64>,
    actor: Mutex<Option<JoinHandle<()>>>,
    owner: watch::Receiver<Owner>,
    owner_task: JoinHandle<()>,
    /// Ends every browse task (shutdown or drop).
    stop_all: watch::Sender<bool>,
}

impl Avahi {
    /// Connects to the system bus and Avahi and waits for the server to run.
    pub(crate) async fn start(cfg: Arc<DiscoveryConfig>) -> Result<Self, Error> {
        let conn = zbus::Connection::system()
            .await
            .map_err(|e| Error::AvahiUnavailable(format!("system bus: {e}")))?;
        let browse_conn = zbus::Connection::system()
            .await
            .map_err(|e| Error::AvahiUnavailable(format!("system bus: {e}")))?;
        let (owner, owner_task) = watch_owner(&conn).await?;
        let (cmd, events_rx, events_dropped, actor) =
            responder::spawn(conn, Arc::clone(&cfg), owner.clone()).await?;
        let (stop_all, _) = watch::channel(false);
        Ok(Self {
            cfg,
            browse_conn,
            cmd,
            events_rx: Mutex::new(Some(events_rx)),
            events_dropped,
            actor: Mutex::new(Some(actor)),
            owner,
            owner_task,
            stop_all,
        })
    }

    pub(crate) fn take_events(&self) -> Option<ResponderEvents> {
        crate::builtin::lock(&self.events_rx)
            .take()
            .map(|rx| ResponderEvents {
                rx,
                dropped: Arc::clone(&self.events_dropped),
            })
    }

    async fn call(&self, make: impl FnOnce(Reply) -> Cmd) -> Result<(), Error> {
        let (tx, rx) = oneshot::channel();
        self.cmd.send(make(tx)).await.map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)?
    }

    pub(crate) async fn register(&self, dev: DeviceServices) -> Result<(), Error> {
        dev.validate(&self.cfg.limits)?;
        self.call(|r| Cmd::Register(dev, r)).await
    }

    pub(crate) async fn update(&self, dev: DeviceServices) -> Result<(), Error> {
        dev.validate(&self.cfg.limits)?;
        self.call(|r| Cmd::Update(dev, r)).await
    }

    pub(crate) async fn withdraw(&self, device: &str) -> Result<(), Error> {
        let device = device.to_owned();
        self.call(|r| Cmd::Withdraw(device, r)).await
    }

    pub(crate) async fn browse(
        &self,
        kinds: &[ServiceKind],
    ) -> Result<(mpsc::Receiver<BrowseEvent>, BrowseHandle), Error> {
        browse::start(
            &self.browse_conn,
            &self.cfg,
            kinds,
            self.owner.clone(),
            self.stop_all.subscribe(),
        )
        .await
    }

    pub(crate) async fn shutdown(&self) {
        // Browsers: their tasks end, which ends the Browsers' streams.
        let _ = self.stop_all.send(true);
        let (tx, rx) = oneshot::channel();
        if self.cmd.send(Cmd::Shutdown(tx)).await.is_ok() {
            let _ = rx.await;
        }
        let handle = crate::builtin::lock(&self.actor).take();
        if let Some(h) = handle {
            let _ = h.await;
        }
        self.owner_task.abort();
    }
}

impl Drop for Avahi {
    /// Dropping without `shutdown` must not leave services advertised: the
    /// actor (which owns every entry group and its watcher task) is stopped,
    /// so the connection is closed and Avahi removes everything this client
    /// registered.
    fn drop(&mut self) {
        let _ = self.stop_all.send(true);
        self.owner_task.abort();
        if let Some(h) = crate::builtin::lock(&self.actor).take() {
            h.abort();
        }
    }
}
