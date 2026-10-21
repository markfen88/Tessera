//! The daemon behind the portal: requests over `control.sock`, and the event
//! hub that keeps the latest events for the pages and fans them out to API
//! clients.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tessera_control_types::{Event, Reply, RequestBody};
use tesseractl::client::{Client, ClientError, DEFAULT_MAX_RESPONSE_BYTES};
use tokio::sync::broadcast;

/// What the portal needs from the daemon. Blocking; the HTTP side calls it
/// on the blocking pool.
pub trait Backend: Send + Sync + std::fmt::Debug + 'static {
    /// One request, one reply.
    ///
    /// # Errors
    /// Transport errors, or the daemon's refusal.
    fn request(&self, body: RequestBody) -> Result<Reply, ClientError>;

    /// Subscribe to events: calls `each` for every reply until it returns
    /// `false` or the connection ends.
    ///
    /// # Errors
    /// The subscription could not be opened, or the connection failed.
    fn events(&self, each: &mut dyn FnMut(Reply) -> bool) -> Result<(), ClientError>;
}

/// The real daemon, one connection per request.
#[derive(Debug, Clone)]
pub struct SocketBackend {
    path: PathBuf,
    timeout: Duration,
}

impl SocketBackend {
    /// A backend for the socket at `path`.
    #[must_use]
    pub fn new(path: PathBuf, timeout: Duration) -> Self {
        Self { path, timeout }
    }
}

impl Backend for SocketBackend {
    fn request(&self, body: RequestBody) -> Result<Reply, ClientError> {
        Client::connect(&self.path, self.timeout, DEFAULT_MAX_RESPONSE_BYTES)?.request(body)
    }

    fn events(&self, each: &mut dyn FnMut(Reply) -> bool) -> Result<(), ClientError> {
        let mut c = Client::connect(&self.path, self.timeout, DEFAULT_MAX_RESPONSE_BYTES)?;
        c.send(RequestBody::SubscribeEvents)?;
        c.set_read_timeout(None)?;
        loop {
            let r = c.read_response()?.result.map_err(ClientError::Command)?;
            if !each(r) {
                return Ok(());
            }
        }
    }
}

/// One entry of the event log.
#[derive(Debug, Clone, PartialEq)]
pub struct Logged {
    /// When the portal received it.
    pub at: SystemTime,
    /// The event, or `None` for "events were lost here".
    pub event: Option<Event>,
}

/// What API subscribers receive.
#[derive(Debug, Clone, PartialEq)]
pub enum HubMsg {
    /// An event.
    Event(Event),
    /// Events were lost (lag, reconnect): re-read the status.
    Resync,
}

/// Keeps the latest events and fans them out.
#[derive(Debug, Clone)]
pub struct EventHub {
    log: Arc<Mutex<VecDeque<Logged>>>,
    tx: broadcast::Sender<HubMsg>,
    cap: usize,
}

impl EventHub {
    /// A hub keeping the last `cap` events.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            log: Arc::new(Mutex::new(VecDeque::with_capacity(cap))),
            tx,
            cap: cap.max(1),
        }
    }

    /// Record and publish.
    pub fn push(&self, msg: HubMsg) {
        let event = match &msg {
            HubMsg::Event(e) => Some(e.clone()),
            HubMsg::Resync => None,
        };
        if let Ok(mut log) = self.log.lock() {
            // One "events may be missing" line per gap, not one per retry.
            let repeat = event.is_none() && log.back().is_some_and(|l| l.event.is_none());
            if repeat {
                drop(log);
                let _ = self.tx.send(msg);
                return;
            }
            if log.len() == self.cap {
                log.pop_front();
            }
            log.push_back(Logged {
                at: SystemTime::now(),
                event,
            });
        }
        let _ = self.tx.send(msg);
    }

    /// The kept events, oldest first.
    #[must_use]
    pub fn recent(&self) -> Vec<Logged> {
        self.log
            .lock()
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// A receiver for new messages.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<HubMsg> {
        self.tx.subscribe()
    }

    /// Follow the daemon's events on a thread of its own, reconnecting after
    /// `retry` whenever the connection ends.
    pub fn follow(&self, backend: Arc<dyn Backend>, retry: Duration) {
        let hub = self.clone();
        let spawned = std::thread::Builder::new()
            .name("tessera-web-events".into())
            .spawn(move || {
                loop {
                    let r = backend.events(&mut |reply| {
                        match reply {
                            Reply::Event(e) => hub.push(HubMsg::Event(e)),
                            Reply::Lagged { .. } => hub.push(HubMsg::Resync),
                            _ => {}
                        }
                        true
                    });
                    match r {
                        // Never connected: nothing was missed since the last gap.
                        Err(ClientError::Connect { .. }) => {}
                        Err(e) => {
                            tracing::debug!("event subscription ended: {e}");
                            hub.push(HubMsg::Resync);
                        }
                        Ok(()) => hub.push(HubMsg::Resync),
                    }
                    std::thread::sleep(retry);
                }
            });
        if let Err(e) = spawned {
            tracing::error!("cannot start the event thread: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_keeps_the_newest_entries() {
        let hub = EventHub::new(2);
        let mut rx = hub.subscribe();
        hub.push(HubMsg::Resync);
        hub.push(HubMsg::Event(Event::ConfigReloaded));
        hub.push(HubMsg::Event(Event::ConfigReloaded));
        let log = hub.recent();
        assert_eq!(log.len(), 2);
        assert!(log.iter().all(|l| l.event == Some(Event::ConfigReloaded)));
        assert_eq!(rx.try_recv().unwrap(), HubMsg::Resync);
    }

    #[test]
    fn repeated_gaps_are_logged_once_but_still_sent() {
        let hub = EventHub::new(10);
        let mut rx = hub.subscribe();
        for _ in 0..5 {
            hub.push(HubMsg::Resync);
        }
        assert_eq!(hub.recent().len(), 1);
        for _ in 0..5 {
            assert_eq!(rx.try_recv().unwrap(), HubMsg::Resync);
        }
        hub.push(HubMsg::Event(Event::ConfigReloaded));
        hub.push(HubMsg::Resync);
        assert_eq!(hub.recent().len(), 3);
    }
}
