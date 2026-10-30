//! Counters of the media engine, shared between the real-time worker (which
//! only does relaxed atomic adds) and the control plane (which reads them for
//! `Query::Metrics`). ADR 0009: RT threads never log; everything they have to
//! say is a counter or a `Copy` event.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use tessera_control_types::{
    DeviceKey, Direction, EngineMetrics, FlowMetrics, RxFlowMetrics, TxFlowMetrics,
};
use tessera_media::{Counter, RxMetrics, TxMetrics};

/// Engine-wide counters. The fields mirror [`EngineMetrics`], which documents
/// each of them; `rx_errors` and `retire_leaks` are diagnostic extras.
#[derive(Debug, Default)]
#[allow(missing_docs)]
pub struct EngineCounters {
    pub running: AtomicU64,
    pub ticks: Counter,
    pub tick_skips: Counter,
    pub max_wake_lateness_us: Counter,
    pub max_tick_work_us: Counter,
    pub rx_datagrams: Counter,
    pub rx_batches: Counter,
    pub rx_filtered: Counter,
    pub rx_truncated: Counter,
    pub rx_batch_limit_hits: Counter,
    pub rx_errors: Counter,
    pub tx_datagrams: Counter,
    pub tx_batches: Counter,
    pub tx_send_failures: Counter,
    pub tx_suppressed_ticks: Counter,
    pub wake_signals: Counter,
    pub sessions: AtomicU64,
    pub flows: AtomicU64,
    pub rt_degraded: AtomicU64,
    pub mlock_failed: AtomicU64,
    pub events_dropped: Counter,
    pub retire_leaks: Counter,
    pub rx_transit_hist: TransitHist,
    pub rx_transit_max_ns: Counter,
    pub socket_buffer_min_bytes: AtomicU64,
    pub socket_buffers_capped: Counter,
    pub wake_lateness_hist: WakeHist,
}

/// Buckets of the wake-up lateness histogram.
pub const WAKE_BUCKETS: usize = 9;
/// Upper edges (exclusive, microseconds) of the first eight buckets.
pub const WAKE_EDGES_US: [u64; WAKE_BUCKETS - 1] = [25, 50, 100, 250, 500, 1000, 2000, 4000];

/// The bucket of a wake-up lateness in microseconds.
#[must_use]
pub fn wake_bucket(lateness_us: u64) -> usize {
    WAKE_EDGES_US
        .iter()
        .position(|e| lateness_us < *e)
        .unwrap_or(WAKE_BUCKETS - 1)
}

/// The wake-up lateness histogram.
#[derive(Debug, Default)]
pub struct WakeHist(pub [Counter; WAKE_BUCKETS]);

/// The transit histogram (arrays this long have no `Default`).
#[derive(Debug)]
pub struct TransitHist(pub [Counter; TRANSIT_BUCKETS]);

impl Default for TransitHist {
    fn default() -> Self {
        Self(std::array::from_fn(|_| Counter::new()))
    }
}

/// Buckets of the transit histogram (see [`EngineMetrics::rx_transit_hist`]).
pub const TRANSIT_BUCKETS: usize = 42;

/// The histogram bucket of a transit time in nanoseconds.
#[must_use]
pub fn transit_bucket(transit_ns: i64) -> usize {
    if transit_ns < 0 {
        0
    } else {
        usize::try_from(transit_ns / 50_000 + 1)
            .unwrap_or(TRANSIT_BUCKETS - 1)
            .min(TRANSIT_BUCKETS - 1)
    }
}

impl EngineCounters {
    /// Record the transit of one received datagram.
    pub fn record_transit(&self, transit_ns: i64) {
        if let Some(c) = self.rx_transit_hist.0.get(transit_bucket(transit_ns)) {
            c.inc();
        }
        self.rx_transit_max_ns
            .record_max(u64::try_from(transit_ns).unwrap_or(0));
    }
}

impl EngineCounters {
    /// A copy in the public metrics shape.
    #[must_use]
    pub fn snapshot(&self) -> EngineMetrics {
        let mut m = EngineMetrics::new();
        m.running = self.running.load(Relaxed);
        m.ticks = self.ticks.get();
        m.tick_skips = self.tick_skips.get();
        m.max_wake_lateness_us = self.max_wake_lateness_us.get();
        m.max_tick_work_us = self.max_tick_work_us.get();
        m.rx_datagrams = self.rx_datagrams.get();
        m.rx_batches = self.rx_batches.get();
        m.rx_filtered = self.rx_filtered.get();
        m.rx_truncated = self.rx_truncated.get();
        m.rx_batch_limit_hits = self.rx_batch_limit_hits.get();
        m.tx_datagrams = self.tx_datagrams.get();
        m.tx_batches = self.tx_batches.get();
        m.tx_send_failures = self.tx_send_failures.get();
        m.tx_suppressed_ticks = self.tx_suppressed_ticks.get();
        m.wake_signals = self.wake_signals.get();
        m.sessions = self.sessions.load(Relaxed);
        m.flows = self.flows.load(Relaxed);
        m.rt_degraded = self.rt_degraded.load(Relaxed);
        m.mlock_failed = self.mlock_failed.load(Relaxed);
        m.events_dropped = self.events_dropped.get();
        m.rx_errors = self.rx_errors.get();
        m.retire_leaks = self.retire_leaks.get();
        m.rx_transit_hist = self.rx_transit_hist.0.iter().map(Counter::get).collect();
        m.rx_transit_max_us = self.rx_transit_max_ns.get() / 1_000;
        m.socket_buffer_min_bytes = self.socket_buffer_min_bytes.load(Relaxed);
        m.socket_buffers_capped = self.socket_buffers_capped.get();
        m.wake_lateness_hist = self.wake_lateness_hist.0.iter().map(Counter::get).collect();
        m
    }
}

/// Counters of one rx flow kept by the daemon (outside the flow engine).
#[derive(Debug, Default)]
pub struct RxFlowCounters {
    /// Receiver keepalives sent.
    pub keepalives_sent: Counter,
    /// Receiver keepalives the kernel refused (dropped, never retried).
    pub keepalive_failures: Counter,
}

/// One registered flow, as the control plane sees it.
#[derive(Debug)]
struct FlowEntry {
    id: String,
    device: String,
    direction: Direction,
    /// The device channels the flow carries in wire order, 1-based (as
    /// subscriptions number them); 0 for a position no channel takes.
    channels: Vec<u16>,
    /// Registration number: a flow rebuilt under the same id is a new one.
    serial: u64,
    started: Instant,
    rx: Option<Arc<RxMetrics>>,
    rx_counters: Option<Arc<RxFlowCounters>>,
    tx: Option<Arc<TxMetrics>>,
    send_failures: Option<Arc<Counter>>,
}

/// Everything the metrics query reads: the engine counters and the live
/// flows. The registry is written only by the media control thread.
#[derive(Debug, Default)]
pub struct MediaStats {
    /// Engine counters.
    pub engine: EngineCounters,
    flows: Mutex<Vec<FlowEntry>>,
    next_serial: AtomicU64,
}

/// What the subscription reporter reads of one rx flow: see
/// [`MediaStats::rx_activity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxActivity {
    /// Registration number, unique for the life of the process.
    pub serial: u64,
    /// Device key.
    pub device: String,
    /// Device channels the flow carries, 1-based.
    pub channels: Vec<u16>,
    /// Datagrams accepted so far.
    pub packets: u64,
    /// When the flow was registered.
    pub started: Instant,
}

fn us(ns: u64) -> u64 {
    ns / 1_000
}

impl MediaStats {
    /// Register an rx flow. `channels` are the device channels it carries,
    /// 0-based as in the flow plan (`plan::UNUSED` for a position no channel
    /// takes).
    pub fn add_rx(
        &self,
        id: &str,
        device: &str,
        channels: &[u32],
        m: Arc<RxMetrics>,
        c: Arc<RxFlowCounters>,
    ) {
        // Kept in wire order (unused positions as 0): peaks are per position.
        let channels = channels
            .iter()
            .map(|c| {
                c.checked_add(1)
                    .and_then(|c| u16::try_from(c).ok())
                    .unwrap_or(0)
            })
            .collect();
        let serial = self.next_serial.fetch_add(1, Relaxed);
        self.flows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(FlowEntry {
                id: id.to_owned(),
                device: device.to_owned(),
                direction: Direction::Rx,
                channels,
                serial,
                started: Instant::now(),
                rx: Some(m),
                rx_counters: Some(c),
                tx: None,
                send_failures: None,
            });
    }

    /// Register a tx flow. `channels` are the device channels in wire order,
    /// 0-based as in the flow plan (`plan::UNUSED` for a silent position).
    pub fn add_tx(
        &self,
        id: &str,
        device: &str,
        channels: &[u32],
        m: Arc<TxMetrics>,
        send_failures: Arc<Counter>,
    ) {
        // Kept in wire order (silent positions as 0): peaks are per position.
        let channels = channels
            .iter()
            .map(|c| {
                c.checked_add(1)
                    .and_then(|c| u16::try_from(c).ok())
                    .unwrap_or(0)
            })
            .collect();
        self.flows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(FlowEntry {
                id: id.to_owned(),
                device: device.to_owned(),
                direction: Direction::Tx,
                channels,
                serial: self.next_serial.fetch_add(1, Relaxed),
                started: Instant::now(),
                rx: None,
                rx_counters: None,
                tx: Some(m),
                send_failures: Some(send_failures),
            });
    }

    /// The live rx flows with their packet counts, for the subscription
    /// reporter (control plane; a relaxed load per flow).
    #[must_use]
    pub fn rx_activity(&self) -> Vec<RxActivity> {
        self.flows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(|e| {
                Some(RxActivity {
                    serial: e.serial,
                    device: e.device.clone(),
                    channels: e.channels.iter().copied().filter(|c| *c != 0).collect(),
                    packets: e.rx.as_ref()?.packets.get(),
                    started: e.started,
                })
            })
            .collect()
    }

    /// Datagrams the rx flow `id` has accepted so far, if it runs.
    #[must_use]
    pub fn rx_packets(&self, id: &str) -> Option<u64> {
        self.flows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|e| e.id == id)
            .and_then(|e| Some(e.rx.as_ref()?.packets.get()))
    }

    /// Per rx flow id, the largest packet lateness since the previous call,
    /// in nanoseconds. Taking resets it: the device-info heartbeat is the
    /// one reader (`conmon.md` §6, section 0x8003).
    #[must_use]
    pub fn take_rx_lateness(&self) -> Vec<(String, u64)> {
        self.flows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(|e| Some((e.id.clone(), e.rx.as_ref()?.take_max_lateness_ns())))
            .collect()
    }

    /// Per device and direction, each channel's peak (absolute left-justified
    /// sample) since the previous call, the largest over the flows carrying
    /// it. Taking resets the flows' peaks: the heartbeat is the one reader.
    #[must_use]
    pub fn take_peaks(&self) -> Vec<(String, Direction, u16, u64)> {
        let mut out: Vec<(String, Direction, u16, u64)> = Vec::new();
        let flows = self.flows.lock().unwrap_or_else(PoisonError::into_inner);
        for e in flows.iter() {
            let peaks: Vec<u64> = match (&e.rx, &e.tx) {
                (Some(m), _) => m.take_peaks().collect(),
                (_, Some(m)) => m.take_peaks().collect(),
                _ => continue,
            };
            for (ch, peak) in e.channels.iter().zip(peaks).filter(|(ch, _)| **ch != 0) {
                match out
                    .iter_mut()
                    .find(|(d, dir, c, _)| *d == e.device && *dir == e.direction && c == ch)
                {
                    Some(slot) => slot.3 = slot.3.max(peak),
                    None => out.push((e.device.clone(), e.direction, *ch, peak)),
                }
            }
        }
        out
    }

    /// Per tx flow id, the monotonic time (ns) its receiver was last heard,
    /// 0 if never (`tessera_media::TxMetrics::last_keepalive_ns`).
    #[must_use]
    pub fn tx_keepalives(&self) -> Vec<(String, u64)> {
        self.flows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(|e| Some((e.id.clone(), e.tx.as_ref()?.last_keepalive_ns.get())))
            .collect()
    }

    /// Per rx flow id, packets that arrived after their playout deadline since
    /// the flow was created (`metering.md` §2.2).
    #[must_use]
    pub fn rx_late_packets(&self) -> Vec<(String, u64)> {
        self.flows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(|e| Some((e.id.clone(), e.rx.as_ref()?.late_packets.get())))
            .collect()
    }

    /// Forget a flow.
    pub fn remove(&self, id: &str) {
        self.flows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|f| f.id != id);
    }

    /// Per-flow counters in the public shape, in registration order.
    #[must_use]
    pub fn flows(&self) -> Vec<FlowMetrics> {
        self.flows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|e| {
                let mut f =
                    FlowMetrics::new(e.id.clone(), DeviceKey(e.device.clone()), e.direction);
                if let Some(m) = &e.rx {
                    let s = m.snapshot();
                    f.packets = s.packets;
                    f.frames = s.frames;
                    f.bytes = s.bytes;
                    let mut r = RxFlowMetrics::new();
                    r.malformed_packets = s.malformed_packets;
                    r.early_packets = s.early_packets;
                    r.late_packets = s.late_packets;
                    r.late_frames = s.late_frames;
                    r.gap_events = s.gap_events;
                    r.gap_frames = s.gap_frames;
                    r.out_of_order_packets = s.out_of_order_packets;
                    r.discontinuities = s.discontinuities;
                    r.epoch_resets = s.epoch_resets;
                    r.hole_samples = s.hole_samples;
                    r.unwritten_samples = s.unwritten_samples;
                    r.overrun_samples = s.overrun_samples;
                    r.valid_samples = s.valid_samples;
                    r.epoch_mismatch_samples = s.epoch_mismatch_samples;
                    r.max_lateness_us = us(s.max_lateness_ns);
                    r.lateness_hist = s.lateness_hist.to_vec();
                    if let Some(c) = &e.rx_counters {
                        r.keepalives_sent = c.keepalives_sent.get();
                        r.keepalive_failures = c.keepalive_failures.get();
                    }
                    f.rx = Some(r);
                }
                if let Some(m) = &e.tx {
                    let s = m.snapshot();
                    f.packets = s.packets;
                    f.frames = s.frames;
                    f.bytes = s.bytes;
                    let mut t = TxFlowMetrics::new();
                    t.underrun_samples = s.underrun_samples;
                    t.late_skips = s.late_skips;
                    t.skipped_packets = s.skipped_packets;
                    t.resyncs = s.resyncs;
                    t.max_send_lag_us = us(s.max_send_lag_ns);
                    t.send_failures = e.send_failures.as_ref().map_or(0, |c| c.get());
                    f.tx = Some(t);
                }
                f
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rx_activity_numbers_channels_from_one_and_follows_the_registry() {
        let s = MediaStats::default();
        let m = Arc::new(RxMetrics::default());
        s.add_rx("a", "main", &[0, 1, 5], Arc::clone(&m), Arc::default());
        s.add_tx(
            "t",
            "main",
            &[0],
            Arc::new(TxMetrics::default()),
            Arc::new(Counter::new()),
        );
        let v = s.rx_activity();
        assert_eq!(v.len(), 1, "tx flows are not listed");
        assert_eq!(v[0].channels, vec![1, 2, 6], "0-based plan, 1-based here");
        assert_eq!(v[0].device, "main");
        assert_eq!(v[0].packets, 0);
        m.packets.add(7);
        assert_eq!(s.rx_activity()[0].packets, 7);
        // Registered again under the same id: a new flow.
        s.remove("a");
        assert!(s.rx_activity().is_empty());
        s.add_rx("a", "main", &[0], m, Arc::default());
        assert_ne!(s.rx_activity()[0].serial, v[0].serial);
    }

    #[test]
    fn unused_positions_carry_no_channel_and_keep_peaks_in_place() {
        // A multicast listener taking position 2 of 3 into device channel 4.
        let s = MediaStats::default();
        let m = RxMetrics {
            peaks: (0..3).map(|_| Counter::default()).collect(),
            ..RxMetrics::default()
        };
        m.peaks[0].record_max(9);
        m.peaks[1].record_max(5);
        let m = Arc::new(m);
        let unused = crate::media::plan::UNUSED;
        s.add_rx(
            "g",
            "main",
            &[unused, 3, unused],
            Arc::clone(&m),
            Arc::default(),
        );
        assert_eq!(s.rx_activity()[0].channels, vec![4]);
        assert_eq!(
            s.take_peaks(),
            vec![("main".to_owned(), Direction::Rx, 4, 5)]
        );
        assert_eq!(s.rx_packets("g"), Some(0));
        m.packets.add(3);
        assert_eq!(s.rx_packets("g"), Some(3));
        assert_eq!(s.rx_packets("missing"), None);
    }
}
