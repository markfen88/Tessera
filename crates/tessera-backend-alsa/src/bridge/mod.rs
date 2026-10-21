//! The in-daemon hardware-card bridge (ADR 0006, 0007, 0004).
//!
//! A local ALSA card has its own clock, independent of the media clock. The
//! bridge moves audio between the card and audio-ipc rings across that
//! boundary and compensates the drift: an asynchronous resampler
//! (`tessera-resample`) whose ratio a PI loop (`tessera-media`) trims on the
//! position error between the ring and the media clock (ADR 0006). The loop
//! runs on one dedicated thread; the card is the master of its cadence.
//!
//! * **Playback** (network to card): the bridge reads the RX ring through the
//!   resampler at `media_now - effective_latency - guard` and writes the card
//!   one chunk at a time. With nothing attached, or nothing written by the
//!   daemon (no subscription), the card gets digital silence, continuously.
//! * **Capture** (card to network): the card's chunks go through the
//!   resampler and are written to the TX ring `lead` frames ahead of the media
//!   clock (ADR 0006: a TX producer writes ahead of `now`).
//! * **Clock**: the daemon's clock overlay when it is fresh, otherwise a
//!   free-running local clock so the card never stops ([`BridgeClock`]).
//!
//! # Threading and the RT rules
//!
//! [`Bridge::step`] and [`Bridge::run`] belong on a dedicated `SCHED_FIFO`
//! thread (ADR 0004: "backend bridge", default priority 75; the daemon sets
//! the policy). They do not allocate, lock or log; the only system calls are
//! the card's `poll`/`read`/`write`/`avail`/`delay` and `clock_gettime`.
//! Everything that allocates (opening cards, building resamplers, attaching
//! rings) happens on the control thread, which talks to the loop through
//! [`BridgeHandle`] over lock-free SPSC queues; replaced objects come back
//! through the same queues and are dropped on the control thread.
//!
//! # Limits
//!
//! The resampler's nominal ratio is 1: the card and the device must run at
//! the same nominal rate (the card is opened at the device's rate, and rings
//! of another rate are refused). Only the drift is compensated.

mod alsa_card;
mod capture;
mod card;
mod clock;
mod drift;
mod playback;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use tessera_audio_ipc::{RingConsumer, RingProducer};
use tessera_media::{PiConfig, PiController, Quality, ResampleError};

pub use alsa_card::{AlsaCard, CardParams};
pub use card::{CardError, CardPcm};
pub use clock::{BridgeClock, BridgeNow, LocalClock, Source};

use crate::clock::Clock;
use capture::Capture;
use playback::Playback;

/// Largest relative resampling ratio (+-0.5 %); the PI loop is limited to
/// [`MAX_CORRECTION`] inside it.
pub const MAX_RELATIVE_RATIO: f64 = 1.005;
/// Largest trim the PI loop may apply: 3000 ppm, far above any crystal
/// tolerance.
pub const MAX_CORRECTION: f64 = 3e-3;
/// Time constant of the error low-pass, seconds.
pub const ERROR_FILTER_TAU: f64 = 0.4;

/// What can go wrong.
#[derive(Debug)]
pub enum BridgeError {
    /// The card failed in a way that cannot be recovered; for a hot-unplug
    /// this is [`CardError::Disconnected`].
    Card(CardError),
    /// Opening or configuring a card failed.
    Open(String),
    /// Invalid configuration.
    Config(String),
    /// The resampler could not be built.
    Resample(ResampleError),
    /// The control queue is full; retry later.
    Busy,
    /// A ring does not fit the bridge (rate, channels).
    Mismatch(String),
    /// The change needs a bridge built afresh (a live retune whose transient
    /// does not fit the guard, lead and re-anchor threshold the bridge was
    /// sized for): close it and build a new one with the new tuning.
    NeedsReopen(String),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::Card(e) => write!(f, "card: {e}"),
            BridgeError::Open(m) => write!(f, "cannot open the card: {m}"),
            BridgeError::Config(m) => write!(f, "bridge configuration: {m}"),
            BridgeError::Resample(e) => write!(f, "resampler: {e:?}"),
            BridgeError::Busy => write!(f, "the bridge control queue is full"),
            BridgeError::Mismatch(m) => write!(f, "ring does not fit the bridge: {m}"),
            BridgeError::NeedsReopen(m) => write!(f, "needs a reopen: {m}"),
        }
    }
}

impl std::error::Error for BridgeError {}

impl From<CardError> for BridgeError {
    fn from(e: CardError) -> Self {
        BridgeError::Card(e)
    }
}

impl From<ResampleError> for BridgeError {
    fn from(e: ResampleError) -> Self {
        BridgeError::Resample(e)
    }
}

/// The platform default resampler quality (ADR 0006): `cubic` on ARM,
/// `sinc-fast` elsewhere.
pub fn default_quality() -> Quality {
    if cfg!(any(target_arch = "arm", target_arch = "aarch64")) {
        Quality::Cubic
    } else {
        Quality::SincFast
    }
}

/// The largest position error, in frames, the drift loop is expected to let
/// build up: a critically damped PI loop with time constant `tau` meeting a
/// clock offset `delta` at once (the first start) peaks at `delta * tau / e`,
/// about `0.37 delta tau` seconds; with the offset at the clamp and a little
/// margin, `0.4 * max_correction * tau * rate`. The default guard (playback),
/// lead (capture) and re-anchor thresholds are sized to hold it, so a slow
/// loop with short periods cannot read audio not yet delivered or write it
/// too late. Measured in `tests/bridge_sim.rs`: 0.84 chunks of 10 ms at
/// 190 ppm with a 120 s loop, against a bound of 0.96.
#[must_use]
pub fn transient_frames(rate_hz: u32, bandwidth_hz: f64, max_correction: f64) -> u64 {
    let tau = 1.0 / (2.0 * std::f64::consts::PI * bandwidth_hz);
    let b = 0.4 * max_correction * tau * f64::from(rate_hz);
    if b.is_finite() && b >= 0.0 {
        b.ceil().min(1e12) as u64
    } else {
        1 << 40
    }
}

/// Bridge configuration; everything has a default.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// Resampler quality (`Off` disables drift compensation: the position
    /// error is then removed by re-anchoring, with an audible slip).
    pub quality: Quality,
    /// Frames per resampler chunk; `None`: the card's period.
    pub chunk_frames: Option<usize>,
    /// Playback: how far behind `media_now - effective_latency` the ring is
    /// read, in frames; `None`: two chunks plus 64, or the loop's
    /// [`transient_frames`] plus one chunk plus 64 if that is more.
    pub rx_guard_frames: Option<u32>,
    /// Capture: how far ahead of the media clock the newest captured frame is
    /// placed in the TX ring, in frames; `None`: three chunks, or
    /// [`transient_frames`] plus two chunks if that is more. That covers the
    /// chunk the resampler holds back, the chunk being captured, the wake-up
    /// jitter and the loop's transient, so data is in the ring before the
    /// media clock reaches it.
    pub tx_lead_frames: Option<u32>,
    /// Natural frequency of the PI loops, Hz.
    pub loop_bandwidth_hz: f64,
    /// Largest correction the PI loops may apply, as a fraction (`2e-4` =
    /// 200 ppm), feed-forward included. At most [`MAX_CORRECTION`].
    pub max_correction: f64,
    /// A clock overlay silent this long is a hung daemon: the bridge falls
    /// back to its local clock. Zero never falls back.
    pub overlay_stale: Duration,
    /// Periods of audio the playback card is kept filled with (written as
    /// silence before the start). The card needs a buffer of at least one
    /// more period.
    pub prefill_periods: usize,
    /// Most chunks handled per direction in one `step`; bounds the work a
    /// late wake-up can cause (ADR 0004: never burst to catch up).
    pub max_chunks_per_step: usize,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            quality: default_quality(),
            chunk_frames: None,
            rx_guard_frames: None,
            tx_lead_frames: None,
            loop_bandwidth_hz: 0.05,
            max_correction: MAX_CORRECTION,
            overlay_stale: Duration::from_secs(2),
            prefill_periods: 3,
            max_chunks_per_step: 4,
        }
    }
}

/// Counters and gauges, readable from any thread.
#[derive(Debug, Default)]
pub struct BridgeStats {
    /// Playback chunks written to the card.
    pub rx_chunks: AtomicU64,
    /// Playback chunks that were silence because no ring was attached.
    pub rx_silence_chunks: AtomicU64,
    /// Playback underruns recovered.
    pub rx_xruns: AtomicU64,
    /// Playback position re-anchors (the loop lost track).
    pub rx_slips: AtomicU64,
    /// Playback frames the daemon had not written (unwritten, hole, overrun).
    pub rx_missing_frames: AtomicU64,
    /// Playback frames the resampler did not produce for a chunk, written as
    /// silence (the bridge's own gaps, as opposed to the network's; 0 in a
    /// healthy run).
    pub rx_short_frames: AtomicU64,
    /// Playback fill error, frames (positive: behind the media clock).
    pub rx_err: AtomicI64,
    /// Largest absolute playback fill error since the last reset, frames.
    pub rx_err_max: AtomicU64,
    /// Playback resampling ratio trim, parts per billion.
    pub rx_trim_ppb: AtomicI64,
    /// Playback: the offset the loop has learned, parts per billion (the trim
    /// without the proportional term; this is the clock offset estimate).
    pub rx_trend_ppb: AtomicI64,
    /// Frames queued in the playback card, last observation.
    pub rx_queue: AtomicU64,
    /// Frames captured and not yet read from the capture card, last
    /// observation.
    pub tx_queue: AtomicU64,
    /// Live retunes of the PI loops applied ([`BridgeHandle::set_loop`]).
    pub loop_retunes: AtomicU64,
    /// Live retunes the bridge could not apply (a clamp outside the range its
    /// resampler was built for); the previous tuning stays.
    pub loop_retunes_refused: AtomicU64,
    /// Capture chunks read from the card.
    pub tx_chunks: AtomicU64,
    /// Capture chunks discarded because no ring was attached.
    pub tx_discarded_chunks: AtomicU64,
    /// Capture overruns recovered.
    pub tx_xruns: AtomicU64,
    /// Capture position re-anchors.
    pub tx_slips: AtomicU64,
    /// Capture frames the TX ring dropped as too old.
    pub tx_late_frames: AtomicU64,
    /// Capture: the offset the loop has learned, parts per billion.
    pub tx_trend_ppb: AtomicI64,
    /// Capture fill error, frames (positive: ahead of the wanted position).
    pub tx_err: AtomicI64,
    /// Largest absolute capture fill error since the last reset, frames.
    pub tx_err_max: AtomicU64,
    /// Capture resampling ratio trim, parts per billion.
    pub tx_trim_ppb: AtomicI64,
    /// Chunks handled while the local clock stood in for the daemon's.
    pub clock_fallback_chunks: AtomicU64,
    /// Switches between the daemon's clock and the fallback.
    pub clock_switches: AtomicU64,
    /// Card errors that were neither an under/overrun nor a removal; the
    /// stream was recovered or will be retried.
    pub card_errors: AtomicU64,
    /// Times a resampler refused the ratio the controller asked for.
    pub ratio_errors: AtomicU64,
    /// Set when a card was lost (hot-unplug).
    pub card_lost: AtomicBool,
}

impl BridgeStats {
    /// Reads and clears the largest playback error.
    pub fn take_rx_err_max(&self) -> u64 {
        self.rx_err_max.swap(0, Ordering::Relaxed)
    }
    /// Reads and clears the largest capture error.
    pub fn take_tx_err_max(&self) -> u64 {
        self.tx_err_max.swap(0, Ordering::Relaxed)
    }
}

/// The ring a playback bridge reads (network to card).
#[derive(Debug)]
pub struct RxLink {
    /// The consumer role of an RX ring.
    pub consumer: RingConsumer,
}

/// The ring a capture bridge writes (card to network).
#[derive(Debug)]
pub struct TxLink {
    /// The producer role of a TX ring.
    pub producer: RingProducer,
}

enum Cmd {
    AttachRx(Box<RxLink>),
    DetachRx,
    AttachTx(Box<TxLink>),
    DetachTx,
    SetClock(Option<Box<dyn Clock + Send>>),
    /// A controller built on the control thread (plain data): the bridge
    /// thread only copies it in.
    SetLoop(PiController, f64),
}

/// The PI loop's tuning, changeable while the bridge runs
/// ([`BridgeHandle::set_loop`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoopParams {
    /// Natural frequency, Hz (positive, finite).
    pub bandwidth_hz: f64,
    /// Largest correction as a fraction, in `(0, MAX_CORRECTION]`.
    pub max_correction: f64,
}

impl LoopParams {
    /// A loop with time constant `tau_s` seconds (`1 / (2 pi f_n)`, the
    /// inverse of the angular natural frequency) and a clamp of `ppm` parts
    /// per million.
    #[must_use]
    pub fn from_time_constant(tau_s: f64, ppm: f64) -> Self {
        Self {
            bandwidth_hz: 1.0 / (2.0 * std::f64::consts::PI * tau_s),
            max_correction: ppm * 1e-6,
        }
    }

    /// Checks the values.
    ///
    /// # Errors
    /// [`BridgeError::Config`] naming the bad value.
    pub fn validate(&self) -> Result<(), BridgeError> {
        if !self.bandwidth_hz.is_finite() || self.bandwidth_hz <= 0.0 {
            return Err(BridgeError::Config(
                "loop bandwidth must be positive".into(),
            ));
        }
        if !self.max_correction.is_finite()
            || self.max_correction <= 0.0
            || self.max_correction > MAX_CORRECTION
        {
            return Err(BridgeError::Config(format!(
                "the correction clamp must be above 0 and at most {} ppm",
                MAX_CORRECTION * 1e6
            )));
        }
        let transient_s = self.transient_s();
        if transient_s.is_nan() || transient_s > MAX_TRANSIENT_S {
            return Err(BridgeError::Config(format!(
                "a loop this slow for its clamp lets {transient_s:.0} s of position error build up (more than {MAX_TRANSIENT_S} s): the guard, the lead and the ring would grow without bound; shorten the time constant or lower the clamp"
            )));
        }
        Ok(())
    }

    /// The loop's transient in seconds (`0.4 * clamp * tau`, see
    /// [`transient_frames`]).
    #[must_use]
    pub fn transient_s(&self) -> f64 {
        0.4 * self.max_correction / (2.0 * std::f64::consts::PI * self.bandwidth_hz)
    }
}

/// Largest loop transient, seconds, a bridge accepts: the guard (playback)
/// or lead (capture) grows by this much latency, and the ring with it. A
/// minute of extra latency is far beyond any use; the limit keeps a typo
/// (a time constant in milliseconds) from building a bridge that reads only
/// missing audio. With the largest clamp (3000 ppm) it allows a time
/// constant of 50 000 s; with the default 200 ppm, 750 000 s.
pub const MAX_TRANSIENT_S: f64 = 60.0;

#[allow(dead_code)] // held only to be dropped on the control thread
enum Retired {
    Rx(Box<RxLink>),
    Tx(Box<TxLink>),
    Clock(Box<dyn Clock + Send>),
}

impl std::fmt::Debug for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cmd")
    }
}

impl std::fmt::Debug for Retired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Retired")
    }
}

/// The control thread's end: attaches and detaches rings and the clock.
#[derive(Debug)]
pub struct BridgeHandle {
    cmd: rtrb::Producer<Cmd>,
    retired: rtrb::Consumer<Retired>,
    stats: Arc<BridgeStats>,
    rate: u32,
    rx_channels: usize,
    tx_channels: usize,
    /// The largest loop transient (frames) the sized guard, lead and
    /// re-anchor thresholds hold (the smaller of the two directions).
    capacity: u64,
    rx_guard: u64,
    tx_lead: u64,
}

const QUEUE: usize = 16;

impl BridgeHandle {
    /// The counters.
    pub fn stats(&self) -> &Arc<BridgeStats> {
        &self.stats
    }

    fn push(&mut self, c: Cmd) -> Result<(), BridgeError> {
        self.cmd.push(c).map_err(|_| BridgeError::Busy)
    }

    /// Starts feeding the playback card from `link`. Replaces a previous ring.
    ///
    /// # Errors
    /// [`BridgeError::Mismatch`] if the ring's rate differs from the card's
    /// or the bridge has no playback card, [`BridgeError::Busy`] if the queue
    /// is full.
    pub fn attach_rx(&mut self, link: RxLink) -> Result<(), BridgeError> {
        let p = link.consumer.params();
        if self.rx_channels == 0 {
            return Err(BridgeError::Mismatch(
                "the bridge has no playback card".into(),
            ));
        }
        if p.sample_rate_hz != self.rate {
            return Err(BridgeError::Mismatch(format!(
                "ring at {} Hz, card at {} Hz",
                p.sample_rate_hz, self.rate
            )));
        }
        self.push(Cmd::AttachRx(Box::new(link)))
    }

    /// Stops reading the ring; the card gets silence.
    ///
    /// # Errors
    /// [`BridgeError::Busy`] if the queue is full.
    pub fn detach_rx(&mut self) -> Result<(), BridgeError> {
        self.push(Cmd::DetachRx)
    }

    /// Starts writing the capture card's audio to `link`.
    ///
    /// # Errors
    /// As for [`BridgeHandle::attach_rx`].
    pub fn attach_tx(&mut self, link: TxLink) -> Result<(), BridgeError> {
        let p = link.producer.params();
        if self.tx_channels == 0 {
            return Err(BridgeError::Mismatch(
                "the bridge has no capture card".into(),
            ));
        }
        if p.sample_rate_hz != self.rate {
            return Err(BridgeError::Mismatch(format!(
                "ring at {} Hz, card at {} Hz",
                p.sample_rate_hz, self.rate
            )));
        }
        self.push(Cmd::AttachTx(Box::new(link)))
    }

    /// Stops writing the ring; capture is discarded.
    ///
    /// # Errors
    /// [`BridgeError::Busy`] if the queue is full.
    pub fn detach_tx(&mut self) -> Result<(), BridgeError> {
        self.push(Cmd::DetachTx)
    }

    /// Installs the daemon's media clock (or removes it: free-running).
    ///
    /// # Errors
    /// [`BridgeError::Busy`] if the queue is full.
    pub fn set_clock(&mut self, clock: Option<Box<dyn Clock + Send>>) -> Result<(), BridgeError> {
        self.push(Cmd::SetClock(clock))
    }

    /// Retunes the PI loops while the bridge runs. The controller is built
    /// here, on the control thread, so every error is returned here; the
    /// bridge thread only swaps it in. What the loop has learned about the
    /// clock offset is kept as the feed-forward term, which
    /// `PiController::set_feed_forward` clamps to the new limit, so a retune
    /// does not re-anchor or slip. A clamp below the learned offset makes the
    /// loop fall behind (a slip later): compare [`BridgeStats::rx_trend_ppb`]
    /// / `tx_trend_ppb` with the new clamp before lowering it.
    ///
    /// # Errors
    /// [`BridgeError::Config`] for invalid values, [`BridgeError::Busy`] if
    /// the queue is full.
    pub fn set_loop(&mut self, params: LoopParams) -> Result<(), BridgeError> {
        params.validate()?;
        let t = transient_frames(self.rate, params.bandwidth_hz, params.max_correction);
        if t > self.capacity {
            return Err(BridgeError::NeedsReopen(format!(
                "the new loop's transient of {t} frames exceeds the {} the bridge was sized for",
                self.capacity
            )));
        }
        let pi = follower_pi(self.rate, params.bandwidth_hz, params.max_correction)?;
        self.push(Cmd::SetLoop(pi, params.max_correction))
    }

    /// The largest loop transient, frames, this bridge's guard, lead and
    /// re-anchor thresholds were sized for; [`BridgeHandle::set_loop`]
    /// refuses a tuning beyond it with [`BridgeError::NeedsReopen`].
    #[must_use]
    pub fn transient_capacity(&self) -> u64 {
        self.capacity
    }

    /// Playback: how far behind `media_now - effective_latency` the RX ring
    /// is read, frames (0 without playback). Part of the bridge's latency,
    /// and of what the RX ring must hold.
    #[must_use]
    pub fn rx_guard_frames(&self) -> u64 {
        self.rx_guard
    }

    /// Capture: how far ahead of the media clock the TX ring is written,
    /// frames (0 without capture).
    #[must_use]
    pub fn tx_lead_frames(&self) -> u64 {
        self.tx_lead
    }

    /// Drops what the loop has handed back (replaced rings, clocks); call it
    /// periodically. Returns how many objects were dropped.
    pub fn reap(&mut self) -> usize {
        let mut n = 0;
        while self.retired.pop().is_ok() {
            n += 1;
        }
        n
    }
}

/// What a [`Bridge::step`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    /// Playback chunks written.
    pub rx_chunks: usize,
    /// Capture chunks processed.
    pub tx_chunks: usize,
    /// Transient card errors met in this step ([`BridgeStats::card_errors`]).
    pub card_errors: usize,
    /// Under- and overruns recovered in this step.
    pub xruns: usize,
}

/// The bridge: owns the cards and runs the loop. Create it on the control
/// thread, then move it to the bridge thread.
pub struct Bridge {
    clock: BridgeClock,
    pb: Option<Playback>,
    cap: Option<Capture>,
    cmd: rtrb::Consumer<Cmd>,
    retire: rtrb::Producer<Retired>,
    stats: Arc<BridgeStats>,
    max_chunks: usize,
    last_source: Source,
    lost: bool,
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bridge")
            .field("playback", &self.pb.is_some())
            .field("capture", &self.cap.is_some())
            .field("lost", &self.lost)
            .finish_non_exhaustive()
    }
}

/// Recovers a card after an error. Only a removal ends the loop; a recovery
/// that fails otherwise is counted and retried on the next step.
pub(crate) fn recover_card(card: &mut dyn CardPcm, stats: &BridgeStats) -> Result<(), BridgeError> {
    match card.recover() {
        Ok(()) => Ok(()),
        Err(CardError::Disconnected) => Err(BridgeError::Card(CardError::Disconnected)),
        Err(_) => {
            stats.card_errors.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }
}

/// The follower PI controller for a stream at `rate_hz`.
///
/// # Errors
/// [`BridgeError::Config`] if the values give no usable controller (never a
/// silent zero-gain fallback: a loop that does not correct would only show
/// up as slips much later).
pub(crate) fn follower_pi(
    rate_hz: u32,
    bandwidth_hz: f64,
    max_correction: f64,
) -> Result<PiController, BridgeError> {
    LoopParams {
        bandwidth_hz,
        max_correction,
    }
    .validate()?;
    let cfg =
        PiConfig::from_bandwidth(f64::from(rate_hz.max(1)), bandwidth_hz, 1.0, max_correction)
            .with_error_filter(ERROR_FILTER_TAU);
    PiController::new(cfg)
        .map_err(|e| BridgeError::Config(format!("drift loop ({bandwidth_hz} Hz): {e}")))
}

impl Bridge {
    /// Builds a bridge over a playback card, a capture card, or both, with
    /// `local` as the free-running clock (use [`LocalClock`]).
    ///
    /// # Errors
    /// [`BridgeError::Config`] if there is no card, the cards disagree on the
    /// rate, or a parameter is invalid; the resampler's error.
    pub fn new(
        playback: Option<Box<dyn CardPcm>>,
        capture: Option<Box<dyn CardPcm>>,
        cfg: &BridgeConfig,
        local: Box<dyn Clock + Send>,
    ) -> Result<(Bridge, BridgeHandle), BridgeError> {
        let rate = match (&playback, &capture) {
            (Some(p), Some(c)) if p.rate() != c.rate() => {
                return Err(BridgeError::Config(format!(
                    "playback card at {} Hz, capture card at {} Hz",
                    p.rate(),
                    c.rate()
                )));
            }
            (Some(p), _) => p.rate(),
            (None, Some(c)) => c.rate(),
            (None, None) => return Err(BridgeError::Config("no card".into())),
        };
        LoopParams {
            bandwidth_hz: cfg.loop_bandwidth_hz,
            max_correction: cfg.max_correction,
        }
        .validate()?;
        if cfg.max_chunks_per_step == 0 {
            return Err(BridgeError::Config(
                "max_chunks_per_step must be at least 1".into(),
            ));
        }
        let (rx_channels, tx_channels) = (
            playback.as_ref().map_or(0, |c| c.channels()),
            capture.as_ref().map_or(0, |c| c.channels()),
        );
        let pb = playback.map(|c| Playback::new(c, cfg)).transpose()?;
        let cap = capture.map(|c| Capture::new(c, cfg)).transpose()?;
        let capacity = pb
            .as_ref()
            .map_or(u64::MAX, Playback::transient_capacity)
            .min(cap.as_ref().map_or(u64::MAX, Capture::transient_capacity));
        let rx_guard = pb.as_ref().map_or(0, Playback::guard);
        let tx_lead = cap.as_ref().map_or(0, Capture::lead);
        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::new(QUEUE);
        let (ret_tx, ret_rx) = rtrb::RingBuffer::new(QUEUE);
        let stats = Arc::new(BridgeStats::default());
        let stale = u64::try_from(cfg.overlay_stale.as_nanos()).unwrap_or(u64::MAX);
        Ok((
            Bridge {
                clock: BridgeClock::new(local, stale),
                pb,
                cap,
                cmd: cmd_rx,
                retire: ret_tx,
                stats: Arc::clone(&stats),
                max_chunks: cfg.max_chunks_per_step,
                last_source: Source::Local,
                lost: false,
            },
            BridgeHandle {
                cmd: cmd_tx,
                retired: ret_rx,
                stats,
                rate,
                rx_channels,
                tx_channels,
                capacity,
                rx_guard,
                tx_lead,
            },
        ))
    }

    /// The counters.
    pub fn stats(&self) -> &Arc<BridgeStats> {
        &self.stats
    }

    /// True once a card was lost; [`Bridge::step`] then keeps failing.
    pub fn is_lost(&self) -> bool {
        self.lost
    }

    /// Hands a replaced object back to the control thread. There is always
    /// room (see `apply_commands`); if there somehow is not, the object is
    /// leaked rather than dropped here: freeing or unmapping on the real-time
    /// thread is the worse failure.
    fn hand_back(&mut self, r: Retired) {
        if let Err(rtrb::PushError::Full(r)) = self.retire.push(r) {
            std::mem::forget(r);
        }
    }

    fn apply_commands(&mut self) {
        // Every command hands back at most one object, so a command is taken
        // only while the retire queue has room for it: a full queue (the control
        // thread is not reaping) leaves commands waiting instead of making
        // this thread drop (free, unmap) what it replaced.
        while self.retire.slots() > 0 {
            let Ok(c) = self.cmd.pop() else { break };
            match c {
                Cmd::AttachRx(l) => {
                    if let Some(pb) = &mut self.pb {
                        if let Some(old) = pb.set_link(Some(l)) {
                            self.hand_back(Retired::Rx(old));
                        }
                    } else {
                        self.hand_back(Retired::Rx(l));
                    }
                }
                Cmd::DetachRx => {
                    if let Some(old) = self.pb.as_mut().and_then(|p| p.set_link(None)) {
                        self.hand_back(Retired::Rx(old));
                    }
                }
                Cmd::AttachTx(l) => {
                    if let Some(cap) = &mut self.cap {
                        if let Some(old) = cap.set_link(Some(l)) {
                            self.hand_back(Retired::Tx(old));
                        }
                    } else {
                        self.hand_back(Retired::Tx(l));
                    }
                }
                Cmd::DetachTx => {
                    if let Some(old) = self.cap.as_mut().and_then(|c| c.set_link(None)) {
                        self.hand_back(Retired::Tx(old));
                    }
                }
                Cmd::SetClock(c) => {
                    if let Some(old) = self.clock.set_primary(c) {
                        self.hand_back(Retired::Clock(old));
                    }
                }
                Cmd::SetLoop(pi, max) => {
                    // Both directions or neither: check first, then apply (a
                    // copy of plain data per direction: no allocation).
                    let fits = self.pb.as_mut().is_none_or(|p| p.fits(max))
                        && self.cap.as_mut().is_none_or(|c| c.fits(max));
                    if fits {
                        if let Some(pb) = &mut self.pb {
                            pb.retune(pi.clone(), max);
                        }
                        if let Some(cap) = &mut self.cap {
                            cap.retune(pi, max);
                        }
                    }
                    let applied = fits;
                    let counter = if applied {
                        &self.stats.loop_retunes
                    } else {
                        &self.stats.loop_retunes_refused
                    };
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// One pass of the loop: applies control commands, waits for the card for
    /// at most `max_wait`, and moves every chunk that is due (at most
    /// `max_chunks_per_step` per direction).
    ///
    /// # Errors
    /// [`BridgeError::Card`] with [`CardError::Disconnected`] when a card was
    /// removed; the bridge is then [`lost`](Bridge::is_lost) and every later
    /// call fails at once without touching the card. Under- and overruns are
    /// recovered internally and counted.
    pub fn step(&mut self, max_wait: Duration) -> Result<Progress, BridgeError> {
        if self.lost {
            return Err(BridgeError::Card(CardError::Disconnected));
        }
        let errors_before = self.stats.card_errors.load(Ordering::Relaxed);
        let xruns_before = self.xruns();
        self.apply_commands();
        // Wait on one card (the playback card, else the capture card); the
        // other is serviced at the same cadence, with a timeout of half its
        // period so it cannot starve.
        let mut wait_failed = false;
        let waited = match (&mut self.pb, &mut self.cap) {
            (Some(p), Some(c)) => {
                let half = c.half_period();
                p.wait(max_wait.min(half))
            }
            (Some(p), None) => p.wait(max_wait),
            (None, Some(c)) => c.wait(max_wait),
            (None, None) => Ok(()),
        };
        match waited {
            Ok(()) => {}
            // The card will report the under- or overrun on the next `avail`,
            // where it is recovered and counted.
            Err(CardError::Xrun | CardError::Suspended) => {}
            Err(CardError::Disconnected) => return Err(self.fail(CardError::Disconnected)),
            // Transient: counted below, once per step, unless the `avail` that
            // follows meets the same error and `on_error` counts it.
            Err(CardError::Other(_)) => wait_failed = true,
        }
        let now = self.clock.now();
        if now.source != self.last_source {
            self.last_source = now.source;
            self.stats.clock_switches.fetch_add(1, Ordering::Relaxed);
        }
        let mut progress = Progress::default();
        if let Some(p) = &mut self.pb {
            match p.step(&now, &self.stats, self.max_chunks) {
                Ok(n) => progress.rx_chunks = n,
                Err(e) => return Err(self.fail_bridge(e)),
            }
        }
        if let Some(c) = &mut self.cap {
            match c.step(&now, &self.stats, self.max_chunks) {
                Ok(n) => progress.tx_chunks = n,
                Err(e) => return Err(self.fail_bridge(e)),
            }
        }
        if wait_failed && self.stats.card_errors.load(Ordering::Relaxed) == errors_before {
            self.stats.card_errors.fetch_add(1, Ordering::Relaxed);
        }
        let errors = self.stats.card_errors.load(Ordering::Relaxed);
        progress.card_errors =
            usize::try_from(errors.saturating_sub(errors_before)).unwrap_or(usize::MAX);
        progress.xruns =
            usize::try_from(self.xruns().saturating_sub(xruns_before)).unwrap_or(usize::MAX);
        Ok(progress)
    }

    fn xruns(&self) -> u64 {
        self.stats
            .rx_xruns
            .load(Ordering::Relaxed)
            .saturating_add(self.stats.tx_xruns.load(Ordering::Relaxed))
    }

    fn fail(&mut self, e: CardError) -> BridgeError {
        self.fail_bridge(BridgeError::Card(e))
    }

    fn fail_bridge(&mut self, e: BridgeError) -> BridgeError {
        if matches!(e, BridgeError::Card(CardError::Disconnected)) {
            self.lost = true;
            self.stats.card_lost.store(true, Ordering::Relaxed);
        }
        e
    }

    /// Runs [`Bridge::step`] until `stop` is set or a card is lost, with the
    /// default [`ErrorSleep`].
    ///
    /// # Errors
    /// The error of the failing step.
    pub fn run(&mut self, stop: &AtomicBool) -> Result<(), BridgeError> {
        self.run_with(stop, ErrorSleep::DEFAULT)
    }

    /// [`run`](Self::run) with the pause after a step that met an xrun or a
    /// card error given by `sleep`.
    ///
    /// # Errors
    /// The error of the failing step.
    pub fn run_with(&mut self, stop: &AtomicBool, sleep: ErrorSleep) -> Result<(), BridgeError> {
        // A card that keeps failing (errors, or an xrun on every step) must not
        // turn the loop into a spin on a real-time thread: back off, longer the
        // longer it lasts, and go back to full speed when a step is clean.
        let mut streak = 0u32;
        while !stop.load(Ordering::Relaxed) {
            let p = self.step(Duration::from_millis(20))?;
            if p.card_errors > 0 || p.xruns > 0 {
                streak = streak.saturating_add(1).min(ErrorSleep::MAX_STREAK);
                std::thread::sleep(sleep.wait(streak));
            } else {
                streak = 0;
            }
        }
        Ok(())
    }
}

/// The pause a bridge thread makes after a step that met an xrun or a card
/// error, so that a card in trouble cannot make a real-time loop spin (a
/// timed `poll` cannot replace it: the card keeps reporting the error at
/// once in exactly those states). Never made in steady state.
///
/// `min_ms` on the first such step, doubled on each further one up to
/// `max_ms` (`doubling: false`: `min_ms` every time); a clean step resets
/// the count. Never zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorSleep {
    /// Double on each further step with an error.
    pub doubling: bool,
    /// First pause, milliseconds (at least 1 is used).
    pub min_ms: u32,
    /// Longest pause, milliseconds (at least `min_ms` is used).
    pub max_ms: u32,
}

impl ErrorSleep {
    /// 2 ms doubling to 64 ms.
    pub const DEFAULT: Self = Self {
        doubling: true,
        min_ms: 2,
        max_ms: 64,
    };

    /// The largest streak worth counting (`min_ms << 31` is beyond any
    /// `max_ms`).
    pub const MAX_STREAK: u32 = 32;

    /// The pause after `streak` steps with an error in a row (1 for the
    /// first). Pure, no allocation.
    #[must_use]
    pub fn wait(&self, streak: u32) -> Duration {
        let min = u64::from(self.min_ms.max(1));
        let max = u64::from(self.max_ms).max(min);
        let ms = if self.doubling {
            let shift = streak.saturating_sub(1).min(31);
            (min << shift).min(max)
        } else {
            min
        };
        Duration::from_millis(ms)
    }
}

impl Default for ErrorSleep {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod error_sleep_tests {
    use super::ErrorSleep;
    use std::time::Duration;

    #[test]
    fn the_default_is_the_former_2_to_64_ms_rule_and_it_is_never_zero() {
        let d = ErrorSleep::default();
        let ms: Vec<u128> = (1..=8).map(|n| d.wait(n).as_millis()).collect();
        assert_eq!(ms, [2, 4, 8, 16, 32, 64, 64, 64]);
        assert_eq!(d.wait(ErrorSleep::MAX_STREAK), Duration::from_millis(64));
        assert_eq!(d.wait(u32::MAX), Duration::from_millis(64));
        let fixed = ErrorSleep {
            doubling: false,
            min_ms: 0,
            max_ms: 0,
        };
        assert_eq!(fixed.wait(10), Duration::from_millis(1), "never no pause");
        let odd = ErrorSleep {
            doubling: true,
            min_ms: 30,
            max_ms: 10,
        };
        assert_eq!(odd.wait(5), Duration::from_millis(30), "max below min: min");
    }
}
