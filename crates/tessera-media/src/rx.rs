//! RX flow engine: packets in, media-time-addressed samples out.
//!
//! An [`RxFlow`] owns one [`MediaBuffer`]. [`RxFlow::ingest`] decodes a
//! datagram through a [`PacketFormat`], classifies it (early / late / out of
//! order / gap) and writes its frames at their absolute media positions.
//! Each engine tick the consumer calls [`RxFlow::tick`], which checks the
//! clock epoch and returns the playout position, then [`RxFlow::read`], and
//! always gets either the exact samples written for a position or silence.
//!
//! # Read-window convention
//!
//! A frame at media position `p` has its playout time at `p + L`. At media
//! time `now` the *playout position* is `now - L`: positions **below** it are
//! due (their playout time has passed), positions at or above it are still
//! in the future. A consumer therefore reads, each tick, the span that has
//! become due since its previous tick, `[previous position, position)`, and
//! what it gets for a position is whatever arrived before that tick. This is
//! exactly the late rule below: a frame is late iff `p < now - L`, i.e. iff
//! it is already due when it arrives. A consumer that wants slack (ADR 0007
//! `client_offset`) simply reads further behind. The two ways to miss a
//! frame land in different counters: arriving late counts `late_frames` at
//! ingest (the frame is not stored), never arriving counts nothing at
//! ingest; either way the later read of that position counts a
//! `hole_sample`.
//!
//! # Decisions where the spec is silent (all provisional)
//!
//! * **Late rule.** With effective latency `L` samples, a frame at media
//!   position `p` must be in the buffer by media time `p + L`. A packet
//!   whose first frame has `ts + L < now` (now = the receive timestamp
//!   mapped to media time, floored to samples) is *late*: it is counted in
//!   `late_packets`, its already-expired head frames are dropped
//!   (`late_frames`) and any unexpired tail is still written.
//! * **Early rule.** A packet is *early* (dropped and counted, never
//!   tracked) if its end lies beyond `now - L + capacity - client_period`:
//!   writing it would advance the window over positions the consumer may
//!   still read. This also contains a corrupt or hostile timestamp: one
//!   wild packet cannot wipe valid data.
//! * **Continuity.** The wire carries no sequence number
//!   (`flows-media.md` section 3); loss detection is timestamp continuity
//!   (`expected = previous ts + previous frames`). A forward jump is a
//!   *gap*, a backward one *out of order*; either of at least one buffer
//!   capacity is a *discontinuity* (sender restart or clock step) and
//!   is not counted as loss.
//! * **Epoch.** A change of the clock epoch resets the buffer and the
//!   continuity tracker; nothing from the old epoch can be read.
//! * **Holes** are measured when the consumer reads, not when packets
//!   arrive: `hole_samples` (never written), `unwritten_samples` (beyond
//!   the newest data), `overrun_samples` (too old), all in channel-sample
//!   units.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::buffer::{MediaBuffer, ReadStats, capacity_for_frames};
use crate::counter::Counter;
use crate::error::ConfigError;
use crate::packet::PacketFormat;
use crate::time::{MediaClock, ns_to_samples, ns_to_samples_ceil, samples_to_ns};

/// Upper edges (exclusive, nanoseconds) of the lateness histogram buckets.
/// Bucket 0 is `lateness < 0` (the packet's media time is still in the
/// future when it arrives); the last bucket is everything at or beyond the
/// final edge. Lateness is `rx media time - packet timestamp`.
pub const LATENESS_EDGES_NS: [i64; 10] = [
    0, 125_000, 250_000, 500_000, 1_000_000, 2_000_000, 4_000_000, 8_000_000, 16_000_000,
    32_000_000,
];

/// Number of lateness histogram buckets.
pub const LATENESS_BUCKETS: usize = LATENESS_EDGES_NS.len() + 1;

/// Bucket index of a lateness value.
#[must_use]
pub fn lateness_bucket(lateness_ns: i64) -> usize {
    LATENESS_EDGES_NS
        .iter()
        .position(|&edge| lateness_ns < edge)
        .unwrap_or(LATENESS_EDGES_NS.len())
}

/// Configuration of one RX flow. All durations are configuration with
/// defaults (rule 7); nothing here is a hard limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxConfig {
    /// Media sample rate in Hz.
    pub sample_rate: u32,
    /// Channels in the flow.
    pub channels: usize,
    /// Own latency setting in nanoseconds (default 4 ms; ADR 0011).
    pub latency_ns: u64,
    /// Minimum latency advertised by the transmitter; the effective latency
    /// is `max(latency_ns, tx_min_latency_ns)`.
    pub tx_min_latency_ns: u64,
    /// Largest latency this flow may ever be configured to; sizes the ring
    /// (ADR 0006: 40 ms).
    pub max_latency_ns: u64,
    /// Reorder window added to the ring depth (ADR 0006: 10 ms).
    pub reorder_window_ns: u64,
    /// Largest consumer period in frames (ADR 0011: 1024 at 48 kHz).
    pub client_period_frames: u64,
    /// Largest packet, in frames, that can be decoded.
    pub max_frames_per_packet: usize,
}

impl RxConfig {
    /// A config with the ADR defaults: latency 4 ms, ring sized for 40 ms +
    /// 10 ms reorder + 1024 frames, packets up to 1024 frames.
    #[must_use]
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        Self {
            sample_rate,
            channels,
            latency_ns: 4_000_000,
            tx_min_latency_ns: 0,
            max_latency_ns: 40_000_000,
            reorder_window_ns: 10_000_000,
            client_period_frames: 1024,
            max_frames_per_packet: 1024,
        }
    }

    /// Effective latency in nanoseconds.
    #[must_use]
    pub fn effective_latency_ns(&self) -> u64 {
        self.latency_ns.max(self.tx_min_latency_ns)
    }

    /// Effective latency in whole samples (rounded up).
    #[must_use]
    pub fn effective_latency_samples(&self) -> u64 {
        ns_to_samples_ceil(self.effective_latency_ns(), self.sample_rate)
    }

    /// Ring capacity per channel: the next power of two at or above
    /// `max(max latency, effective latency) + reorder window + client period`.
    #[must_use]
    pub fn buffer_capacity(&self) -> usize {
        let latency = self.max_latency_ns.max(self.effective_latency_ns());
        let ns = latency.saturating_add(self.reorder_window_ns);
        let frames =
            ns_to_samples_ceil(ns, self.sample_rate).saturating_add(self.client_period_frames);
        capacity_for_frames(frames)
    }

    /// Checks the configuration.
    ///
    /// # Errors
    /// Zero rate/channels/packet size, or a ring that does not fit in memory.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.sample_rate == 0 {
            return Err(ConfigError::ZeroSampleRate);
        }
        if self.channels == 0 {
            return Err(ConfigError::ZeroChannels);
        }
        if self.max_frames_per_packet == 0 {
            return Err(ConfigError::ZeroFramesPerPacket);
        }
        self.channels
            .checked_mul(self.max_frames_per_packet)
            .ok_or(ConfigError::SizeOverflow)?;
        self.channels
            .checked_mul(self.buffer_capacity())
            .ok_or(ConfigError::SizeOverflow)?;
        // The latency must fit the ring with room for the client period.
        if self
            .effective_latency_samples()
            .saturating_add(self.client_period_frames)
            >= self.buffer_capacity() as u64
        {
            return Err(ConfigError::OutOfRange("latency"));
        }
        Ok(())
    }
}

/// Atomic RX metrics, shared with the control thread via [`Arc`]. Counters
/// are in packets, frames or channel-samples as named; there is no logging
/// on the RT path (ADR 0009).
#[derive(Debug, Default)]
pub struct RxMetrics {
    /// Datagrams accepted for processing (decoded successfully).
    pub packets: Counter,
    /// Frames in accepted datagrams.
    pub frames: Counter,
    /// Bytes of accepted datagrams.
    pub bytes: Counter,
    /// Datagrams that failed to decode (including oversize).
    pub malformed_packets: Counter,
    /// Packets dropped as too far in the future (see the module docs).
    pub early_packets: Counter,
    /// Packets whose first frame had already missed its playout time.
    pub late_packets: Counter,
    /// Frames dropped because their playout time had passed.
    pub late_frames: Counter,
    /// Forward jumps in timestamp continuity.
    pub gap_events: Counter,
    /// Frames skipped over by forward jumps (an upper bound on loss: a
    /// reordered packet may still fill the gap).
    pub gap_frames: Counter,
    /// Packets whose timestamp was behind the expected next one.
    pub out_of_order_packets: Counter,
    /// Jumps of at least one buffer capacity in either direction.
    pub discontinuities: Counter,
    /// Buffer resets caused by a clock epoch change.
    pub epoch_resets: Counter,
    /// Read samples inside the window that were never written.
    pub hole_samples: Counter,
    /// Read samples beyond the newest data.
    pub unwritten_samples: Counter,
    /// Read samples older than the window.
    pub overrun_samples: Counter,
    /// Read samples that were valid.
    pub valid_samples: Counter,
    /// Read samples silenced because the clock epoch differed from the one
    /// the buffer belongs to (a tick or packet has not reset it yet).
    pub epoch_mismatch_samples: Counter,
    /// Lateness histogram, see [`LATENESS_EDGES_NS`].
    pub lateness_hist: [Counter; LATENESS_BUCKETS],
    /// Largest lateness seen since the last [`RxMetrics::take_max_lateness_ns`]
    /// (clamped at zero).
    pub max_lateness_ns: Counter,
}

impl RxMetrics {
    /// Reads and resets the maximum lateness (for the periodic heartbeat).
    pub fn take_max_lateness_ns(&self) -> u64 {
        self.max_lateness_ns.take()
    }

    /// A plain copy of all counters.
    #[must_use]
    pub fn snapshot(&self) -> RxSnapshot {
        let mut hist = [0u64; LATENESS_BUCKETS];
        for (h, c) in hist.iter_mut().zip(&self.lateness_hist) {
            *h = c.get();
        }
        RxSnapshot {
            packets: self.packets.get(),
            frames: self.frames.get(),
            bytes: self.bytes.get(),
            malformed_packets: self.malformed_packets.get(),
            early_packets: self.early_packets.get(),
            late_packets: self.late_packets.get(),
            late_frames: self.late_frames.get(),
            gap_events: self.gap_events.get(),
            gap_frames: self.gap_frames.get(),
            out_of_order_packets: self.out_of_order_packets.get(),
            discontinuities: self.discontinuities.get(),
            epoch_resets: self.epoch_resets.get(),
            hole_samples: self.hole_samples.get(),
            unwritten_samples: self.unwritten_samples.get(),
            overrun_samples: self.overrun_samples.get(),
            valid_samples: self.valid_samples.get(),
            epoch_mismatch_samples: self.epoch_mismatch_samples.get(),
            lateness_hist: hist,
            max_lateness_ns: self.max_lateness_ns.get(),
        }
    }
}

/// Plain-data copy of [`RxMetrics`]; field meanings are identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(missing_docs)]
pub struct RxSnapshot {
    pub packets: u64,
    pub frames: u64,
    pub bytes: u64,
    pub malformed_packets: u64,
    pub early_packets: u64,
    pub late_packets: u64,
    pub late_frames: u64,
    pub gap_events: u64,
    pub gap_frames: u64,
    pub out_of_order_packets: u64,
    pub discontinuities: u64,
    pub epoch_resets: u64,
    pub hole_samples: u64,
    pub unwritten_samples: u64,
    pub overrun_samples: u64,
    pub valid_samples: u64,
    pub epoch_mismatch_samples: u64,
    pub lateness_hist: [u64; LATENESS_BUCKETS],
    pub max_lateness_ns: u64,
}

/// Result of [`RxFlow::tick`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Playout {
    /// Exclusive end of the positions that are due: `now_media - latency`
    /// in samples. Positions below it have passed their playout time.
    pub position: u64,
    /// True if this tick observed a clock epoch change and dropped the
    /// buffered audio: the consumer must re-anchor its read cursor to
    /// `position` instead of reading the span since its previous tick.
    pub discontinuity: bool,
}

/// What [`RxFlow::ingest`] did with a datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestOutcome {
    /// Written. `late_frames` head frames were dropped as expired.
    Accepted {
        /// Media position of the first frame in the datagram.
        timestamp: u64,
        /// Frames in the datagram.
        frames: usize,
        /// Frames dropped because their playout time had passed.
        late_frames: usize,
    },
    /// Every frame had already missed its playout time.
    Late,
    /// Dropped as too far in the future.
    Early,
    /// Could not be decoded.
    Malformed,
}

/// One RX flow. See the [module documentation](self).
#[derive(Debug)]
pub struct RxFlow<F: PacketFormat> {
    cfg: RxConfig,
    format: F,
    buffer: MediaBuffer,
    scratch: Vec<i32>,
    latency_samples: u64,
    cap: u64,
    epoch: Option<u64>,
    expected_next: Option<u64>,
    metrics: Arc<RxMetrics>,
}

impl<F: PacketFormat> RxFlow<F> {
    /// Builds a flow, allocating its buffers (control thread only).
    ///
    /// # Errors
    /// Invalid configuration, or a format whose channel count differs.
    pub fn new(cfg: RxConfig, format: F) -> Result<Self, ConfigError> {
        cfg.validate()?;
        if format.channels() != cfg.channels {
            return Err(ConfigError::ChannelMismatch {
                config: cfg.channels,
                format: format.channels(),
            });
        }
        let capacity = cfg.buffer_capacity();
        let buffer = MediaBuffer::new(cfg.channels, capacity)?;
        let scratch = vec![0i32; cfg.channels * cfg.max_frames_per_packet];
        Ok(Self {
            latency_samples: cfg.effective_latency_samples(),
            cap: capacity as u64,
            scratch,
            buffer,
            format,
            cfg,
            epoch: None,
            expected_next: None,
            metrics: Arc::new(RxMetrics::default()),
        })
    }

    /// The shared metrics handle.
    #[must_use]
    pub fn metrics(&self) -> Arc<RxMetrics> {
        Arc::clone(&self.metrics)
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &RxConfig {
        &self.cfg
    }

    /// Effective latency in samples (the playout delay).
    #[must_use]
    pub fn latency_samples(&self) -> u64 {
        self.latency_samples
    }

    /// Ring capacity per channel in samples.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.buffer.capacity()
    }

    /// Read access to the underlying buffer (for tests and tooling).
    #[must_use]
    pub fn buffer(&self) -> &MediaBuffer {
        &self.buffer
    }

    /// Advances the flow to the clock's present: checks the epoch (dropping
    /// everything buffered if it changed) and returns the playout position.
    ///
    /// Call this once per engine tick, before reading. It is the only way to
    /// obtain the playout position, and [`read`](Self::read) independently
    /// refuses to return audio from an epoch other than the clock's current
    /// one, so old-epoch audio can never be heard even if a tick is missed.
    pub fn tick<C: MediaClock>(&mut self, clock: &C) -> Playout {
        let snap = clock.now();
        let discontinuity = self.sync_epoch(snap.epoch);
        Playout {
            position: ns_to_samples(snap.media_ns, self.cfg.sample_rate)
                .saturating_sub(self.latency_samples),
            discontinuity,
        }
    }

    /// Processes one datagram received at local monotonic time `rx_local_ns`.
    /// Allocation-free; never panics on any input.
    pub fn ingest<C: MediaClock>(
        &mut self,
        datagram: &[u8],
        rx_local_ns: u64,
        clock: &C,
    ) -> IngestOutcome {
        let m = &*self.metrics;
        let decoded = match self.format.decode(datagram, &mut self.scratch) {
            Ok(d) if d.frames > 0 => d,
            _ => {
                m.malformed_packets.inc();
                return IngestOutcome::Malformed;
            }
        };
        let rate = self.cfg.sample_rate;

        // Epoch: positions from an earlier epoch are meaningless now.
        let snap = clock.at_local(rx_local_ns);
        self.sync_epoch(snap.epoch);
        let m = &*self.metrics;

        let ts = decoded.timestamp;
        let frames = decoded.frames as u64;
        let Some(end) = ts.checked_add(frames) else {
            m.malformed_packets.inc();
            return IngestOutcome::Malformed;
        };
        m.packets.inc();
        m.frames.add(frames);
        m.bytes.add(datagram.len() as u64);

        let now_ns = snap.media_ns;
        let now = ns_to_samples(now_ns, rate);

        // Lateness histogram: rx media time minus the packet timestamp.
        let ts_ns = samples_to_ns(ts, rate);
        let lateness = i64::try_from(i128::from(now_ns) - i128::from(ts_ns))
            .unwrap_or(if now_ns >= ts_ns { i64::MAX } else { i64::MIN });
        if let Some(h) = m.lateness_hist.get(lateness_bucket(lateness)) {
            h.inc();
        }
        m.max_lateness_ns
            .record_max(u64::try_from(lateness).unwrap_or(0));

        // Early: would advance the window over positions still to be read.
        let reader = now.saturating_sub(self.latency_samples);
        let horizon = reader
            .saturating_add(self.cap)
            .saturating_sub(self.cfg.client_period_frames);
        if end > horizon {
            m.early_packets.inc();
            return IngestOutcome::Early;
        }

        // Continuity (timestamp based; the wire has no sequence number).
        match self.expected_next {
            None => self.expected_next = Some(end),
            Some(exp) if ts == exp => self.expected_next = Some(end),
            Some(exp) if ts > exp => {
                let jump = ts - exp;
                if jump >= self.cap {
                    m.discontinuities.inc();
                } else {
                    m.gap_events.inc();
                    m.gap_frames.add(jump);
                }
                self.expected_next = Some(end);
            }
            Some(exp) => {
                if exp - ts >= self.cap {
                    m.discontinuities.inc();
                    self.expected_next = Some(end);
                } else {
                    m.out_of_order_packets.inc();
                    self.expected_next = Some(exp.max(end));
                }
            }
        }

        // Late: drop the frames whose playout time has already passed.
        let skip = reader.saturating_sub(ts).min(frames);
        if skip > 0 {
            m.late_packets.inc();
            m.late_frames.add(skip);
            if skip == frames {
                return IngestOutcome::Late;
            }
        }
        let skip_frames = skip as usize;
        let first = skip_frames * self.cfg.channels;
        let used = decoded.frames * self.cfg.channels;
        if let Some(samples) = self.scratch.get(first..used) {
            self.buffer.write_interleaved(ts + skip, samples);
        }
        IngestOutcome::Accepted {
            timestamp: ts,
            frames: decoded.frames,
            late_frames: skip_frames,
        }
    }

    /// Compares an epoch with the one this flow last saw; on a change drops
    /// all buffered audio and continuity state (positions from an earlier
    /// epoch are meaningless) and returns `true` (`false` on the first epoch
    /// ever seen). Called by [`ingest`](Self::ingest) and [`tick`](Self::tick).
    fn sync_epoch(&mut self, epoch: u64) -> bool {
        if self.epoch == Some(epoch) {
            return false;
        }
        let changed = self.epoch.is_some();
        if changed {
            self.metrics.epoch_resets.inc();
        }
        self.buffer.reset();
        self.expected_next = None;
        self.epoch = Some(epoch);
        changed
    }

    /// Reads `out.len()` samples of `channel` from media positions
    /// `pos .. pos + out.len()`.
    ///
    /// Read-window convention: positions below the latest
    /// [`Playout::position`] are *due* (their playout time `p + L` has
    /// passed); a consumer reads, each tick, the positions that became due
    /// since its previous read, `[previous position, position)`. Anything
    /// not valid reads as silence and is counted.
    ///
    /// If the clock's epoch is not the one this flow has synced to, the whole
    /// read is silence and counts as `epoch_mismatch_samples`: the buffer can
    /// still hold old-epoch audio until the next [`tick`](Self::tick) or
    /// packet resets it, and it must not be heard.
    pub fn read<C: MediaClock>(
        &self,
        clock: &C,
        channel: usize,
        pos: u64,
        out: &mut [i32],
    ) -> ReadStats {
        let m = &*self.metrics;
        if self.epoch != Some(clock.now().epoch) {
            out.iter_mut().for_each(|o| *o = 0);
            m.epoch_mismatch_samples.add(out.len() as u64);
            return ReadStats {
                unwritten: out.len() as u64,
                ..ReadStats::default()
            };
        }
        let st = self.buffer.read(channel, pos, out);
        m.valid_samples.add(st.valid);
        m.hole_samples.add(st.hole);
        m.unwritten_samples.add(st.unwritten);
        m.overrun_samples.add(st.overrun);
        st
    }

    /// Drops all buffered audio and continuity state (for example when the
    /// flow is torn down and re-created by the same owner).
    pub fn reset(&mut self) {
        self.buffer.reset();
        self.expected_next = None;
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::convert::WireFormat;
    use crate::reference::ReferenceFormat;
    use crate::time::ClockSnapshot;
    use core::cell::Cell;

    /// Test clock: media = local + OFFSET.
    struct Clk {
        now_local: Cell<u64>,
        epoch: Cell<u64>,
    }

    const OFFSET: u64 = 1_000_000_000;

    impl Clk {
        fn new() -> Self {
            Clk {
                now_local: Cell::new(0),
                epoch: Cell::new(0),
            }
        }
        /// Sets "now" to media sample `s` plus 1 us (still inside sample `s`).
        fn set_media_samples(&self, s: u64) {
            self.now_local.set(samples_to_ns(s, 48_000) + 1000 - OFFSET);
        }
    }

    impl MediaClock for Clk {
        fn now(&self) -> ClockSnapshot {
            self.at_local(self.now_local.get())
        }
        fn at_local(&self, l: u64) -> ClockSnapshot {
            ClockSnapshot {
                epoch: self.epoch.get(),
                media_ns: l + OFFSET,
            }
        }
    }

    const BASE: u64 = 240_000; // media sample of "now" in most tests (5 s)

    fn flow(ch: usize) -> (RxFlow<ReferenceFormat>, ReferenceFormat) {
        let f = ReferenceFormat::new(ch, 48_000, WireFormat::BE24).unwrap();
        let mut cfg = RxConfig::new(48_000, ch);
        cfg.latency_ns = 4_000_000; // 192 samples
        (RxFlow::new(cfg, f).unwrap(), f)
    }

    fn pkt(f: &ReferenceFormat, ts: u64, frames: usize, base: i32) -> Vec<u8> {
        let ch = f.channels();
        let samples: Vec<i32> = (0..frames * ch).map(|i| (base + i as i32) << 8).collect();
        let mut out = vec![0u8; f.encoded_len(frames)];
        f.encode(ts, &samples, &mut out, None).unwrap();
        out
    }

    fn rx_local(clk: &Clk) -> u64 {
        clk.now_local.get()
    }

    #[test]
    fn config_derives_depth_from_latency() {
        let c = RxConfig::new(48_000, 2);
        // 40 ms + 10 ms = 2400 frames, + 1024 = 3424 -> 4096.
        assert_eq!(c.buffer_capacity(), 4096);
        assert_eq!(c.effective_latency_samples(), 192);
        let mut c2 = c.clone();
        c2.tx_min_latency_ns = 10_000_000;
        assert_eq!(c2.effective_latency_ns(), 10_000_000);
        assert_eq!(c2.effective_latency_samples(), 480);
        let c192 = RxConfig::new(192_000, 2);
        assert_eq!(c192.buffer_capacity(), 16384);
        let mut big = c.clone();
        big.latency_ns = 400_000_000;
        assert!(big.buffer_capacity() > 4096);
        assert!(big.validate().is_ok());
        let mut bad = c;
        bad.channels = 0;
        assert_eq!(bad.validate(), Err(ConfigError::ZeroChannels));
    }

    #[test]
    fn channel_mismatch_is_rejected() {
        let f = ReferenceFormat::new(3, 48_000, WireFormat::BE24).unwrap();
        assert_eq!(
            RxFlow::new(RxConfig::new(48_000, 2), f).unwrap_err(),
            ConfigError::ChannelMismatch {
                config: 2,
                format: 3
            }
        );
    }

    #[test]
    fn on_time_packet_is_played_out_at_timestamp_plus_latency() {
        let (mut rx, f) = flow(2);
        let clk = Clk::new();
        clk.set_media_samples(BASE + 32); // packet [BASE, BASE+32) arrives as it ends
        let out = rx.ingest(&pkt(&f, BASE, 32, 1), rx_local(&clk), &clk);
        assert_eq!(
            out,
            IngestOutcome::Accepted {
                timestamp: BASE,
                frames: 32,
                late_frames: 0
            }
        );
        let mut o = [0i32; 32];
        let st = rx.read(&clk, 0, BASE, &mut o);
        assert_eq!(st.valid, 32);
        assert_eq!(o[0], 1 << 8);
        assert_eq!(o[1], 3 << 8);
        rx.read(&clk, 1, BASE, &mut o);
        assert_eq!(o[0], 2 << 8);
        let s = rx.metrics().snapshot();
        assert_eq!(
            (s.packets, s.frames, s.late_packets, s.valid_samples),
            (1, 32, 0, 64)
        );
        assert_eq!(rx.tick(&clk).position, BASE + 32 - rx.latency_samples());
    }

    #[test]
    fn late_packet_is_dropped_and_counted() {
        let (mut rx, f) = flow(1);
        let clk = Clk::new();
        let l = rx.latency_samples();
        clk.set_media_samples(BASE + l + 32 + 1);
        let out = rx.ingest(&pkt(&f, BASE, 32, 1), rx_local(&clk), &clk);
        assert_eq!(out, IngestOutcome::Late);
        let mut o = [9i32; 32];
        let st = rx.read(&clk, 0, BASE, &mut o);
        assert_eq!(o, [0; 32]);
        assert_eq!(st.valid, 0);
        let s = rx.metrics().snapshot();
        assert_eq!((s.late_packets, s.late_frames), (1, 32));
    }

    #[test]
    fn late_rule_boundary_is_exact() {
        let clk = Clk::new();
        let (mut rx, f) = flow(1);
        let l = rx.latency_samples();
        // now == ts + L is NOT late (the rule is ts + L < now).
        clk.set_media_samples(BASE + l);
        let out = rx.ingest(&pkt(&f, BASE, 8, 1), rx_local(&clk), &clk);
        assert_eq!(
            out,
            IngestOutcome::Accepted {
                timestamp: BASE,
                frames: 8,
                late_frames: 0
            }
        );
        // One sample later the first frame is late but the tail is kept.
        let (mut rx, f) = flow(1);
        clk.set_media_samples(BASE + l + 1);
        let out = rx.ingest(&pkt(&f, BASE, 8, 1), rx_local(&clk), &clk);
        assert_eq!(
            out,
            IngestOutcome::Accepted {
                timestamp: BASE,
                frames: 8,
                late_frames: 1
            }
        );
        let mut o = [0i32; 8];
        let st = rx.read(&clk, 0, BASE, &mut o);
        assert_eq!((st.valid, st.hole), (7, 1));
        assert_eq!(o[0], 0);
        assert_eq!(o[1], 2 << 8);
    }

    #[test]
    fn far_future_timestamp_cannot_wipe_valid_data() {
        let (mut rx, f) = flow(1);
        let clk = Clk::new();
        clk.set_media_samples(BASE + 32);
        rx.ingest(&pkt(&f, BASE, 32, 1), rx_local(&clk), &clk);
        // A corrupt stamp 10 minutes ahead.
        let wild = BASE + 48_000 * 600;
        assert_eq!(
            rx.ingest(&pkt(&f, wild, 32, 99), rx_local(&clk), &clk),
            IngestOutcome::Early
        );
        let mut o = [0i32; 32];
        assert_eq!(rx.read(&clk, 0, BASE, &mut o).valid, 32);
        assert_eq!(rx.buffer().hi(0), BASE + 32);
        assert_eq!(rx.metrics().snapshot().early_packets, 1);
        // The wild packet did not disturb continuity tracking.
        rx.ingest(&pkt(&f, BASE + 32, 32, 1), rx_local(&clk), &clk);
        assert_eq!(rx.metrics().snapshot().gap_events, 0);
    }

    #[test]
    fn near_u64_max_timestamp_is_handled() {
        let (mut rx, _f) = flow(1);
        let clk = Clk::new();
        clk.set_media_samples(BASE);
        // Hand-built datagram: seconds = u32::MAX, sub = 47_999 -> huge but valid.
        let mut d = vec![2u8];
        d.extend_from_slice(&u32::MAX.to_be_bytes());
        d.extend_from_slice(&47_999u32.to_be_bytes());
        d.extend_from_slice(&[0, 0, 0]);
        assert_eq!(rx.ingest(&d, rx_local(&clk), &clk), IngestOutcome::Early);
    }

    #[test]
    fn gaps_reorders_and_discontinuities_are_tracked() {
        let (mut rx, f) = flow(1);
        let clk = Clk::new();
        clk.set_media_samples(BASE + 150);
        let local = rx_local(&clk);
        rx.ingest(&pkt(&f, BASE, 32, 1), local, &clk);
        rx.ingest(&pkt(&f, BASE + 32, 32, 1), local, &clk); // in order
        rx.ingest(&pkt(&f, BASE + 96, 32, 1), local, &clk); // gap of 32
        rx.ingest(&pkt(&f, BASE + 64, 32, 1), local, &clk); // the missing one, late: out of order
        let s = rx.metrics().snapshot();
        assert_eq!(
            (s.gap_events, s.gap_frames, s.out_of_order_packets),
            (1, 32, 1)
        );
        // The reordered packet filled the hole.
        let mut o = [0i32; 128];
        assert_eq!(rx.read(&clk, 0, BASE, &mut o).valid, 128);
        // A jump of less than a capacity is a gap, a larger one a discontinuity.
        let far = BASE + 4096 - 32;
        clk.set_media_samples(far + 40);
        rx.ingest(&pkt(&f, far, 8, 1), rx_local(&clk), &clk);
        assert_eq!(rx.metrics().snapshot().discontinuities, 0);
        let far2 = far + 100_000;
        clk.set_media_samples(far2 + 40);
        rx.ingest(&pkt(&f, far2, 8, 1), rx_local(&clk), &clk);
        let s = rx.metrics().snapshot();
        assert_eq!(s.discontinuities, 1);
        assert_eq!(s.gap_events, 2);
    }

    #[test]
    fn lateness_histogram_and_max() {
        let clk = Clk::new();
        let (mut rx, f) = flow(1);
        // lateness = 1 ms + ~1 us.
        clk.set_media_samples(BASE + 48);
        rx.ingest(&pkt(&f, BASE, 8, 1), rx_local(&clk), &clk);
        let s = rx.metrics().snapshot();
        assert_eq!(s.lateness_hist[lateness_bucket(1_000_000)], 1);
        assert!(rx.metrics().take_max_lateness_ns() >= 1_000_000);
        assert_eq!(rx.metrics().take_max_lateness_ns(), 0);
        // Early arrival: negative lateness lands in bucket 0.
        let (mut rx, f) = flow(1);
        clk.set_media_samples(BASE - 100);
        rx.ingest(&pkt(&f, BASE, 8, 1), rx_local(&clk), &clk);
        assert_eq!(rx.metrics().snapshot().lateness_hist[0], 1);
        assert_eq!(lateness_bucket(-1), 0);
        assert_eq!(lateness_bucket(0), 1);
        assert_eq!(lateness_bucket(i64::MAX), LATENESS_BUCKETS - 1);
    }

    #[test]
    fn epoch_change_resets_and_prevents_replay() {
        let (mut rx, f) = flow(1);
        let clk = Clk::new();
        clk.set_media_samples(BASE + 32);
        rx.ingest(&pkt(&f, BASE, 32, 1), rx_local(&clk), &clk);
        clk.epoch.set(1);
        clk.set_media_samples(BASE + 64);
        rx.ingest(&pkt(&f, BASE + 32, 32, 7), rx_local(&clk), &clk);
        let mut o = [0i32; 32];
        assert_eq!(
            rx.read(&clk, 0, BASE, &mut o).valid,
            0,
            "old-epoch audio must be gone"
        );
        assert_eq!(rx.read(&clk, 0, BASE + 32, &mut o).valid, 32);
        assert_eq!(rx.metrics().snapshot().epoch_resets, 1);
    }

    #[test]
    fn malformed_datagrams_are_counted() {
        let (mut rx, f) = flow(2);
        let clk = Clk::new();
        assert_eq!(rx.ingest(&[], 0, &clk), IngestOutcome::Malformed);
        assert_eq!(rx.ingest(&[0; 9], 0, &clk), IngestOutcome::Malformed);
        // Larger than max_frames_per_packet.
        let big = pkt(&f, BASE, 1025, 1);
        assert_eq!(rx.ingest(&big, 0, &clk), IngestOutcome::Malformed);
        assert_eq!(rx.metrics().snapshot().malformed_packets, 3);
    }

    #[test]
    fn clock_step_back_and_epoch_bump_without_packets_never_replays() {
        // Audio is playing, then media time steps BACK by 4 ms (192 samples,
        // so the new playout positions fall inside the old valid window) and
        // the epoch bumps; no packet arrives afterwards.
        let (mut rx, f) = flow(1);
        let clk = Clk::new();
        for k in 0..8u64 {
            clk.set_media_samples(BASE + 32 * (k + 1));
            rx.ingest(&pkt(&f, BASE + 32 * k, 32, 100), rx_local(&clk), &clk);
        }
        clk.set_media_samples(BASE + 256 + 192);
        let before = rx.tick(&clk);
        let mut o = [0i32; 64];
        assert_eq!(rx.read(&clk, 0, before.position - 64, &mut o).valid, 64);
        clk.now_local.set(clk.now_local.get() - 4_000_000);
        clk.epoch.set(1);
        // Even before the flow has ticked, reads must not return old audio.
        let old = BASE; // valid in the old epoch and inside the old window
        let st = rx.read(&clk, 0, old, &mut o);
        assert_eq!(o, [0; 64]);
        assert_eq!((st.valid, st.unwritten), (0, 64));
        assert_eq!(rx.metrics().snapshot().epoch_mismatch_samples, 64);
        // The tick resets the buffer, flags the discontinuity and returns the
        // new playout position; the positions around it were valid before.
        let after = rx.tick(&clk);
        assert!(after.discontinuity);
        assert_eq!(after.position, before.position - 192);
        for pos in [
            after.position.saturating_sub(64),
            after.position,
            BASE,
            BASE + 100,
        ] {
            let st = rx.read(&clk, 0, pos, &mut o);
            assert_eq!(o, [0; 64], "replayed old audio at {pos}");
            assert_eq!(st.valid, 0);
        }
        assert!(!rx.tick(&clk).discontinuity);
        assert_eq!(rx.metrics().snapshot().epoch_resets, 1);
    }

    #[test]
    fn missed_frames_hit_late_or_hole_counters_depending_on_cause() {
        // Read-window convention: positions below tick().position are due.
        // Packet A arrives after its playout time (late); packet B never
        // arrives (lost); packet C arrives in time.
        let (mut rx, f) = flow(1);
        let clk = Clk::new();
        let l = rx.latency_samples();
        // C first, on time, so that the ring has a high-water mark.
        clk.set_media_samples(BASE + 64 + 32);
        rx.ingest(&pkt(&f, BASE + 64, 32, 1), rx_local(&clk), &clk);
        // A (ts BASE) arrives when its playout time has long passed.
        clk.set_media_samples(BASE + 32 + l + 40);
        assert_eq!(
            rx.ingest(&pkt(&f, BASE, 32, 1), rx_local(&clk), &clk),
            IngestOutcome::Late
        );
        let s = rx.metrics().snapshot();
        assert_eq!((s.late_packets, s.late_frames), (1, 32));
        assert_eq!(s.hole_samples, 0, "nothing has been read yet");
        // The consumer now reads everything that is due: A, B and C.
        clk.set_media_samples(BASE + 96 + l + 10);
        let due = rx.tick(&clk).position;
        assert!(due >= BASE + 96);
        let mut o = [0i32; 96];
        let st = rx.read(&clk, 0, BASE, &mut o);
        // A and B (64 frames) are holes; C (32) is valid. Late frames are
        // counted at ingest, holes at read; a lost packet only ever shows up
        // as a hole.
        assert_eq!((st.valid, st.hole, st.unwritten), (32, 64, 0));
        let s = rx.metrics().snapshot();
        assert_eq!((s.hole_samples, s.late_frames), (64, 32));
        // Positions past the newest data are "unwritten", not holes.
        let st = rx.read(&clk, 0, BASE + 96, &mut o[..8]);
        assert_eq!((st.hole, st.unwritten), (0, 8));
    }

    #[test]
    fn silence_after_flow_stops_even_beyond_capacity() {
        let (mut rx, f) = flow(1);
        let clk = Clk::new();
        clk.set_media_samples(BASE + 32);
        rx.ingest(&pkt(&f, BASE, 32, 1), rx_local(&clk), &clk);
        let mut o = [1i32; 32];
        for k in 1..10u64 {
            let st = rx.read(&clk, 0, BASE + k * 4096, &mut o);
            assert_eq!(o, [0; 32]);
            assert_eq!(st.unwritten, 32);
        }
    }
}
