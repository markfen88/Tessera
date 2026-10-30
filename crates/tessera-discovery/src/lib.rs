//! mDNS/DNS-SD advertisement and browsing.
//!
//! This crate is the network side of discovery: a [`Responder`] registers,
//! updates and withdraws the service instances of logical devices, and a
//! [`Browser`] reports instances appearing, changing and disappearing. Both
//! are backend-neutral; behind them sits either the built-in `mdns-sd`
//! responder or Avahi over D-Bus, chosen once per [`Discovery`] by
//! [`Mode`] (see [`select`] for the rules and why the two never run together).
//!
//! Input is plain data ([`DeviceServices`]) that the daemon fills from its own
//! state; the crate depends on no state types. Everything wire-facing (service
//! type names, TXT keys and default values, default ports) lives in
//! [`provisional`] and is **provisional** until lab captures confirm it.
//!
//! The control plane is async (tokio). Pure data code ([`txt`], [`advert`],
//! [`model`], the browse cache) is synchronous and has no I/O.
//!
//! # Known limits and deliberate omissions
//!
//! Backends:
//! * Record TTLs cannot be set: `mdns-sd` has no public setter and Avahi's
//!   `AddService` has no TTL argument. The spec's 4500 s for the ARC record is
//!   not applied.
//! * Empty TXT strings cannot be emitted by either backend, and the built-in
//!   *browser* (`mdns-sd`) stops reading a TXT record at the first empty
//!   string, losing every key after it. The CMC record of the reference has two
//!   empty strings (spec 3.2, `guess`); they are not modelled.
//! * Conflicts are resolved by the backend renaming, and the scheme differs:
//!   built-in gives `name (2)` and host `name-2`, Avahi gives `name #2`. The new
//!   name is reported in [`ResponderEvent::Conflict`].
//! * Built-in: `mdns-sd` sends a goodbye for the shared host address with every
//!   unregistered service, so withdrawing one service of a device can make
//!   peers drop its other services for a moment. [`Responder::update`]
//!   announces the remaining services again afterwards and spaces the
//!   announcements of one device by 1.1 s (the daemon multicasts a record at
//!   most once per second, RFC 6762 section 6), so an update can take about
//!   that long. Avahi has neither problem.
//! * Avahi hands TXT strings out in reverse order; the browser reverses them
//!   back, so the order seen is the wire order.
//! * Without configured interfaces both backends use the first wired interface
//!   that has carrier (ADR 0010; the reference likewise uses one interface):
//!   not loopback, wireless or virtual, not down. With none, starting fails
//!   with [`Error::NoDefaultInterface`]. Explicit lists are used as given.
//!   The Avahi backend re-checks interfaces and their addresses every
//!   [`DiscoveryConfig::address_recheck`] (there is no netlink watcher) and
//!   republishes what changed; `mdns-sd` follows address changes by itself.
//! * `avahi-daemon` stopping or restarting: the Avahi backend reports
//!   [`ResponderEvent::Failed`] for every device, reports everything browsed as
//!   [`BrowseEvent::Disappeared`], and when a daemon is back republishes and
//!   browses again.
//! * Avahi limits (Debian 13, avahi 0.8): the bus allows 512 match rules per
//!   connection (the backend uses a handful), and the daemon about 1024 objects
//!   per client: roughly 1020 advertised services per [`Discovery`], and about
//!   330 resolved instances per connection. Each service-kind/interface browse
//!   session owns up to 8 connections including its browser connection (at
//!   most 32 per interface for the daemon's four service kinds). What still
//!   does not fit is counted
//!   ([`Browser::resolve_failures`]); an advertisement beyond the limit fails
//!   with `TooManyObjectsError` and is rolled back.
//! * A resolver failure removes that item's observations and leaves discovery
//!   reconciling until a fresh item announcement or backend session retries it.
//!   If a peer returns while its PTR remains cached, Avahi may not announce a
//!   new item; recovery can require restarting discovery. With
//!   [`IpFamilies::Both`], failure in either address family removes both
//!   families for the item. The daemon's inventory uses IPv4 only.
//! * Early resolver signals, pending creations and retired resolver paths are
//!   bounded by the configured instance cap times address-family count. On
//!   saturation, only the affected kind/interface browse session reconnects.
//!   Its sources are withdrawn until re-enumerated, and changed endpoint
//!   references expire. Aggregate browser readiness becomes reconciling, so
//!   the daemon marks all remote inventory incomplete; recovery has no fixed
//!   duration. Advertising and other sessions stay active. Retired paths cannot
//!   crowd out a new resolver's first signal. A creation returning a retired
//!   path also reconnects that session, without assuming retired paths are never
//!   reused. Simultaneously active resolver paths are still matched by path alone.
//! * Dropping a [`Discovery`] without [`Discovery::shutdown`] stops the
//!   backend; with Avahi the services disappear when its connection closes. After
//!   `shutdown` or the drop, every [`Browser::recv`] returns `None`.
//! * With the built-in backend another process on UDP 5353 (for example
//!   Avahi) is shared with via `SO_REUSEADDR`, which makes two responders; see
//!   [`DiscoveryConfig::port_conflict`]. [`Discovery::avahi_appeared_later`]
//!   tells if Avahi showed up while the built-in responder runs.
//! * Re-announcing the remaining services after a partial withdrawal (built-in)
//!   is done in chunks of 16 with 20 ms between them, so a device with hundreds
//!   of channels is announced as a stream, not a burst.
//!
//! Slow consumers: backends never wait for a reader (ADR 0004). Responder events
//! and browse events are dropped, and counted, when their stream is full;
//! a [`BrowseEvent::Overflow`] marks the gap and [`Browser::snapshot`] gives the
//! true state.
//!
//! Not implemented (spec section 4, controller side): resolving a known name
//! directly without browsing, and the `in-addr.local` probe for multicast
//! addresses in use. [`ServiceInstance`] carries no per-interface information.
//! Hostname and address list cannot change in [`Responder::update`] (withdraw and
//! register again).

pub mod advert;
pub mod config;
pub mod error;
pub mod model;
pub mod provisional;
pub mod select;
pub mod txt;

mod avahi;
mod backoff;
mod builtin;
mod cache;
mod ifaces;
mod names;
mod sink;

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;

#[doc(hidden)]
pub use config::TestHooks;
pub use config::{BackendKind, DiscoveryConfig, IpFamilies, Limits, Mode, PortConflict};
pub use error::{Error, ValidationError};
#[doc(hidden)]
pub use ifaces::{MdnsSocket, own_mdns_sockets};
pub use model::{
    BrowseEvent, ConflictScope, DeviceServices, PublishedService, ResponderEvent, ServiceId,
    ServiceInstance, ServiceKind, ServiceObservation, ServiceSpec,
};
pub use select::{BusProbe, ProbeError, Selection, SystemBus, select_backend};
pub use txt::{TxtEntry, TxtError, TxtLimits, TxtRecord};

/// The backend in use.
#[derive(Debug)]
enum Inner {
    Builtin(Arc<builtin::Builtin>),
    Avahi(avahi::Avahi),
}

/// A running discovery stack: one backend, any number of devices and browsers.
#[derive(Debug)]
pub struct Discovery {
    inner: Arc<Inner>,
    selection: Selection,
    /// Set when Avahi took its bus name while the built-in responder runs.
    avahi_appeared: Arc<std::sync::atomic::AtomicBool>,
    watcher: Option<AbortOnDrop>,
}

/// Aborts its task when dropped.
#[derive(Debug)]
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Discovery {
    /// Selects a backend per `config.mode` using the real system bus and
    /// starts it.
    ///
    /// # Errors
    /// See [`select_backend`]; also backend start-up failures. A failure to
    /// start Avahi after it was selected is an error, never a fallback to the
    /// built-in responder.
    pub async fn start(config: DiscoveryConfig) -> Result<Self, Error> {
        Self::start_with(config, &SystemBus).await
    }

    /// Like [`Discovery::start`] with an explicit bus probe (for tests).
    ///
    /// # Errors
    /// As for [`Discovery::start`].
    pub async fn start_with(config: DiscoveryConfig, probe: &impl BusProbe) -> Result<Self, Error> {
        let cfg = Arc::new(config);
        let cfg2 = Arc::clone(&cfg);
        Self::start_inner(cfg, probe, move || avahi::Avahi::start(cfg2)).await
    }

    async fn start_inner<F, Fut>(
        cfg: Arc<DiscoveryConfig>,
        probe: &impl BusProbe,
        avahi_start: F,
    ) -> Result<Self, Error>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<avahi::Avahi, Error>>,
    {
        let selection = select_backend(cfg.mode, probe).await?;
        let avahi_appeared = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (inner, watcher) = match selection.kind {
            BackendKind::Builtin => {
                let b = Inner::Builtin(Arc::new(builtin::Builtin::start(cfg)?));
                let w = tokio::spawn(select::watch_avahi_appears(Arc::clone(&avahi_appeared)));
                (b, Some(AbortOnDrop(w)))
            }
            BackendKind::Avahi => (Inner::Avahi(avahi_start().await?), None),
        };
        Ok(Self {
            inner: Arc::new(inner),
            selection,
            avahi_appeared,
            watcher,
        })
    }

    /// Which backend was selected.
    #[must_use]
    pub fn backend(&self) -> BackendKind {
        self.selection.kind
    }

    /// The selection result, including any warning.
    #[must_use]
    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    /// True if the built-in backend is in use and `avahi-daemon` has appeared
    /// on the system bus since (a warning is logged when it does): two
    /// responders now share UDP port 5353.
    #[must_use]
    pub fn avahi_appeared_later(&self) -> bool {
        self.watcher.is_some() && self.avahi_appeared.load(Ordering::SeqCst)
    }

    /// A handle to the responder.
    #[must_use]
    pub fn responder(&self) -> Responder {
        Responder {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Starts browsing for `kinds` (all kinds if empty). Dropping the
    /// [`Browser`] stops the browse.
    ///
    /// # Errors
    /// Backend failures.
    pub async fn browse(&self, kinds: &[ServiceKind]) -> Result<Browser, Error> {
        let kinds: &[ServiceKind] = if kinds.is_empty() {
            &ServiceKind::ALL
        } else {
            kinds
        };
        let (rx, handle) = match &*self.inner {
            Inner::Builtin(b) => b.browse(kinds).await?,
            Inner::Avahi(a) => a.browse(kinds).await?,
        };
        Ok(Browser { rx, handle })
    }

    /// Withdraws everything and stops the backend.
    pub async fn shutdown(self) {
        match &*self.inner {
            Inner::Builtin(b) => b.shutdown().await,
            Inner::Avahi(a) => a.shutdown().await,
        }
    }
}

/// Registers, updates and withdraws the services of logical devices.
/// Cheap to clone.
#[derive(Debug, Clone)]
pub struct Responder {
    inner: Arc<Inner>,
}

impl Responder {
    /// Revision of authoritative advertisement state, independent of event loss.
    #[must_use]
    pub fn revision(&self) -> u64 {
        match &*self.inner {
            Inner::Builtin(b) => b.revision(),
            Inner::Avahi(a) => a.revision(),
        }
    }

    /// Lifetime diagnostic count; loss never removes authoritative state.
    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        match &*self.inner {
            Inner::Builtin(b) => b.dropped_events(),
            Inner::Avahi(a) => a.dropped_events(),
        }
    }

    /// Actual established advertisements, including conflict-assigned names.
    /// This recovers state even when announcement notifications were lost.
    ///
    /// # Errors
    /// The backend stopped before answering.
    pub async fn published(&self) -> Result<Vec<model::PublishedService>, Error> {
        match &*self.inner {
            Inner::Builtin(b) => Ok(b.published()),
            Inner::Avahi(a) => a.published().await,
        }
    }

    /// Registers a new device. Returns when the services were handed to the
    /// backend; probing and announcing finish later and are reported as
    /// [`ResponderEvent::Announced`] (or [`ResponderEvent::Conflict`]).
    ///
    /// # Errors
    /// [`Error::Invalid`] for bad data, [`Error::AlreadyRegistered`], or a
    /// backend failure (nothing stays registered then).
    pub async fn register(&self, device: DeviceServices) -> Result<(), Error> {
        match &*self.inner {
            Inner::Builtin(b) => b.register(device).await,
            Inner::Avahi(a) => a.register(device).await,
        }
    }

    /// Replaces the services of a registered device with `device`: services
    /// that disappeared are withdrawn, new ones registered, and changed ones
    /// (TXT, port) updated in place. Services keep a name assigned after a
    /// conflict. The hostname and the address list cannot change.
    ///
    /// # Errors
    /// [`Error::NotRegistered`], [`Error::UnsupportedUpdate`], invalid data or
    /// a backend failure.
    pub async fn update(&self, device: DeviceServices) -> Result<(), Error> {
        match &*self.inner {
            Inner::Builtin(b) => b.update(device).await,
            Inner::Avahi(a) => a.update(device).await,
        }
    }

    /// Withdraws all services of a device (goodbye packets are sent before
    /// this returns, as far as the backend allows).
    ///
    /// # Errors
    /// [`Error::NotRegistered`] or a backend failure.
    pub async fn withdraw(&self, device: &str) -> Result<(), Error> {
        match &*self.inner {
            Inner::Builtin(b) => b.withdraw(device).await,
            Inner::Avahi(a) => a.withdraw(device).await,
        }
    }

    /// The event stream (announced, conflict, withdrawn, failed). It can be
    /// taken once; later calls return `None`. Events are dropped, and counted
    /// in [`ResponderEvents::dropped`], if the stream is not read.
    #[must_use]
    pub fn events(&self) -> Option<ResponderEvents> {
        match &*self.inner {
            Inner::Builtin(b) => b.take_events(),
            Inner::Avahi(a) => a.take_events(),
        }
    }
}

/// Stream of [`ResponderEvent`]s.
#[derive(Debug)]
pub struct ResponderEvents {
    rx: mpsc::Receiver<ResponderEvent>,
    dropped: Arc<AtomicU64>,
}

impl ResponderEvents {
    /// Waits for the next event; `None` once the backend has stopped.
    pub async fn recv(&mut self) -> Option<ResponderEvent> {
        self.rx.recv().await
    }

    /// The next event if one is ready.
    pub fn try_recv(&mut self) -> Option<ResponderEvent> {
        self.rx.try_recv().ok()
    }

    /// Events lost because the stream was full or unread.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Stops a browse when dropped.
pub(crate) struct BrowseHandle {
    view: sink::BrowseView,
    on_drop: Option<Box<dyn FnOnce() + Send>>,
    availability: Option<Box<dyn Fn() -> (bool, u64) + Send + Sync>>,
}

impl BrowseHandle {
    pub(crate) fn new(view: sink::BrowseView, on_drop: Box<dyn FnOnce() + Send>) -> Self {
        Self {
            view,
            on_drop: Some(on_drop),
            availability: None,
        }
    }

    pub(crate) fn with_availability(
        mut self,
        availability: impl Fn() -> (bool, u64) + Send + Sync + 'static,
    ) -> Self {
        self.availability = Some(Box::new(availability));
        self
    }
}

impl std::fmt::Debug for BrowseHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowseHandle").finish_non_exhaustive()
    }
}

impl Drop for BrowseHandle {
    fn drop(&mut self) {
        if let Some(f) = self.on_drop.take() {
            f();
        }
    }
}

/// Backend availability and reconciliation of the available observation cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BrowseReadiness {
    Unavailable,
    Reconciling,
    Ready,
}

/// Stream of [`BrowseEvent`]s. Dropping it stops browsing.
/// Readiness is independent of channel closure.
#[derive(Debug)]
pub struct Browser {
    rx: mpsc::Receiver<BrowseEvent>,
    handle: BrowseHandle,
}

impl Browser {
    /// Cheap change marker for debounced full snapshots. TTL refreshes do not
    /// change it. Lifecycle and reconciliation changes do.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.handle.view.revision()
    }

    /// Whether the current backend session has reconciled its available
    /// cache. This never proves discovery of the entire network.
    #[must_use]
    pub fn readiness(&self) -> BrowseReadiness {
        let (available, generation) = self.handle.availability.as_ref().map_or((true, 0), |f| f());
        if self.is_closed() || !available {
            BrowseReadiness::Unavailable
        } else if self.handle.view.reconciled(generation) {
            BrowseReadiness::Ready
        } else {
            BrowseReadiness::Reconciling
        }
    }
    /// The backend producers have stopped. A drained closed browser cannot
    /// provide fresh inventory; its last snapshot is retained for inspection.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.rx.is_closed()
    }

    /// Waits for the next event; `None` once the backend has stopped.
    pub async fn recv(&mut self) -> Option<BrowseEvent> {
        if let Some(ev) = self.try_recv() {
            return Some(ev);
        }
        self.rx.recv().await
    }

    /// The next event if one is ready. After events were lost this returns
    /// [`BrowseEvent::Overflow`] once the queue is drained, even if nothing new
    /// arrives.
    pub fn try_recv(&mut self) -> Option<BrowseEvent> {
        match self.rx.try_recv() {
            Ok(ev) => Some(ev),
            Err(_) => self.handle.view.take_overflow(),
        }
    }

    /// Instances ignored because more than
    /// [`DiscoveryConfig::max_instances_per_kind`] were seen.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.handle.view.ignored()
    }

    /// Events dropped because the stream was not read fast enough. The
    /// backends never wait for the reader (ADR 0004); after a loss the stream
    /// carries a [`BrowseEvent::Overflow`] and [`Browser::snapshot`] gives the
    /// true state.
    #[must_use]
    pub fn lost_events(&self) -> u64 {
        self.handle.view.lost()
    }

    /// Resolution failures and Avahi session-recovery events. This includes
    /// resolver creation/failure signals, saturated bookkeeping and returned
    /// retired object paths. A session recovery withdraws that session's
    /// observations. This lifetime diagnostic is not a completeness verdict.
    #[must_use]
    pub fn resolve_failures(&self) -> u64 {
        self.handle.view.failures()
    }

    /// The most recent resolution failure (see [`Browser::resolve_failures`]).
    #[must_use]
    pub fn last_resolve_failure(&self) -> Option<String> {
        self.handle.view.last_failure()
    }

    /// All instances currently known, sorted by kind and name. Use it after a
    /// [`BrowseEvent::Overflow`] to resynchronise.
    #[must_use]
    pub fn snapshot(&self) -> Vec<ServiceInstance> {
        self.handle.view.snapshot()
    }

    /// Every source view currently known, with its own TXT and interface
    /// provenance. Unlike snapshot, this never merges different sources.
    #[must_use]
    pub fn observations(&self) -> Vec<ServiceObservation> {
        self.handle.view.observations()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_tracks_owner_loss_and_reconciliation_without_channel_closure() {
        use std::sync::Mutex;
        let (producer, view, rx) = sink::BrowseShared::new(10, 2);
        producer.session(0, None);
        let owner = Arc::new(Mutex::new((true, 1)));
        let observed = Arc::clone(&owner);
        let handle = BrowseHandle::new(view, Box::new(|| {}))
            .with_availability(move || *observed.lock().unwrap());
        let browser = Browser { rx, handle };
        assert_eq!(browser.readiness(), BrowseReadiness::Reconciling);
        producer.session(0, Some(1));
        assert_eq!(browser.readiness(), BrowseReadiness::Ready);
        *owner.lock().unwrap() = (false, 2);
        assert!(!browser.is_closed());
        assert_eq!(browser.readiness(), BrowseReadiness::Unavailable);
        *owner.lock().unwrap() = (true, 3);
        assert_eq!(browser.readiness(), BrowseReadiness::Reconciling);
        producer.session(0, Some(3));
        assert_eq!(browser.readiness(), BrowseReadiness::Ready);
    }

    struct FakeAvahiBus;
    impl BusProbe for FakeAvahiBus {
        async fn avahi_present(&self) -> Result<bool, ProbeError> {
            Ok(true)
        }
    }

    /// Avahi is on the bus but unusable: starting must fail, not fall back to
    /// the built-in responder (which would create a second responder).
    #[tokio::test(flavor = "current_thread")]
    async fn avahi_failure_does_not_fall_back_to_builtin() {
        let cfg = Arc::new(DiscoveryConfig::default());
        let r = Discovery::start_inner(cfg, &FakeAvahiBus, || async {
            Err(Error::AvahiUnavailable("connect refused".into()))
        })
        .await;
        assert!(matches!(r, Err(Error::AvahiUnavailable(_))), "{r:?}");
    }
}
