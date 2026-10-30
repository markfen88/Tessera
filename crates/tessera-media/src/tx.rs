//! TX flow engine: schedules, packetises and encodes outgoing packets.
//!
//! # Timing (ADR 0006)
//!
//! A packet's timestamp is the media time of its first frame. It is released
//! no earlier than `timestamp + frames_per_packet` (the instant just after
//! its last frame) plus `send_offset`, so it can never arrive "in the
//! future" at a receiver. The engine behaves like a hardware converter: the
//! client must already have written the samples by then (the standard
//! soundcard contract); positions it has not written go out as silence and
//! are counted as underruns. The audio for a packet at timestamp `ts` is the
//! source content at media positions `ts .. ts + fpp`.
//!
//! Timestamps are aligned to multiples of the packet size when the flow
//! starts (`align_to_packet`, `likely` from the reference; configurable).
//! The `-0.5 ms` stamp of the prior art is deliberately not implemented
//! (ADR 0006, alternatives).
//!
//! # Never burst (ADR 0004)
//!
//! [`TxFlow::poll`] returns at most one packet per call. If the oldest
//! pending packet is more than `max_lag` past its release time the flow has
//! stalled: it skips every older pending packet, resumes at the *latest*
//! packet that is already due (it is sent by the next poll, with a lag of
//! less than one packet period), counts one `late_skip` and the skipped
//! packets, and continues. Receivers see a hole, never a catch-up burst. A
//! clock epoch change, or media time stepping backwards by more than
//! `resync_back`, re-anchors the flow instead.
//!
//! `max_lag` defaults to **two** packet periods. A driver that sleeps until
//! [`TxFlow::next_due_ns`] (mapped to local time) sees lags of a few tens of
//! microseconds; a driver that polls on a fixed period close to the packet
//! period instead accumulates a rate difference between its timer and the
//! media clock (10 ppm moves the phase by a whole period every 67 s), so
//! occasionally two packets are due at one wake-up, and the older one is up
//! to one period late. Two periods tolerate that without discarding audio.
//! Drivers must schedule from `next_due_ns`; fixed-period polling is only a
//! fallback.
//!
//! # Provisional
//!
//! `send_offset_ns` (default 0), `align_to_packet` and the 1400-byte payload
//! limit are values from the ADRs/spec marked `likely`/`guess`; they are
//! configuration, not constants.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::buffer::FrameSource;
use crate::convert::Dither;
use crate::counter::Counter;
use crate::error::{ConfigError, TxError};
use crate::packet::PacketFormat;
use crate::time::{MediaClock, ns_to_samples, ns_to_samples_ceil, samples_to_ns_ceil};

/// Default maximum datagram size used to bound packet size (`flows-media.md`
/// section 2: 1400 bytes of audio; ADR 0006 `mtu_payload`). PROVISIONAL.
pub const DEFAULT_MAX_PAYLOAD: usize = 1400;

/// Configuration of one TX flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxConfig {
    /// Media sample rate in Hz.
    pub sample_rate: u32,
    /// For each wire channel position, the source channel to read, or `None`
    /// for a position that carries silence (`flows-media.md`: channel id 0).
    pub channel_map: Vec<Option<usize>>,
    /// Frames per packet.
    pub frames_per_packet: usize,
    /// Extra delay after the last frame before a packet is released
    /// (`tx_send_offset`, ADR 0006; default 0, to be confirmed by captures).
    pub send_offset_ns: u64,
    /// Round the first timestamp up to a multiple of `frames_per_packet`.
    pub align_to_packet: bool,
    /// How far past its release time a packet may still be sent. `None`
    /// means two packet periods (see the module documentation).
    pub max_lag_ns: Option<u64>,
    /// Media time stepping back by more than this re-anchors the flow.
    pub resync_back_ns: u64,
    /// Upper bound for the datagram size; the configuration is rejected if a
    /// packet would exceed it.
    pub max_payload: usize,
    /// Seed for TPDF dither applied when the wire format is narrower than
    /// the internal 32 bits. `None` (the default) means bit-exact truncation
    /// (ADR 0006).
    pub dither_seed: Option<u64>,
}

impl TxConfig {
    /// A flow carrying source channels `0..channels` in order, with the
    /// given packet size and the default offsets.
    #[must_use]
    pub fn new(sample_rate: u32, channels: usize, frames_per_packet: usize) -> Self {
        Self {
            sample_rate,
            channel_map: (0..channels).map(Some).collect(),
            frames_per_packet,
            send_offset_ns: 0,
            align_to_packet: true,
            max_lag_ns: None,
            resync_back_ns: 10_000_000,
            max_payload: DEFAULT_MAX_PAYLOAD,
            dither_seed: None,
        }
    }

    /// Like [`new`](Self::new) with the packet size derived from a packet
    /// period (rounded to the nearest whole number of frames, at least 1).
    #[must_use]
    pub fn with_packet_period(sample_rate: u32, channels: usize, period_ns: u64) -> Self {
        let fpp = crate::packet::frames_for_period_ns(sample_rate, period_ns);
        Self::new(sample_rate, channels, fpp)
    }

    /// Packet duration in nanoseconds (rounded up).
    #[must_use]
    pub fn packet_period_ns(&self) -> u64 {
        samples_to_ns_ceil(self.frames_per_packet as u64, self.sample_rate)
    }

    /// Checks the configuration against a packet format.
    ///
    /// # Errors
    /// Zero rate/channels/packet size, channel mismatch with the format, or
    /// a packet larger than `max_payload`.
    pub fn validate<F: PacketFormat>(&self, format: &F) -> Result<(), ConfigError> {
        if self.sample_rate == 0 {
            return Err(ConfigError::ZeroSampleRate);
        }
        if self.channel_map.is_empty() {
            return Err(ConfigError::ZeroChannels);
        }
        if self.frames_per_packet == 0 {
            return Err(ConfigError::ZeroFramesPerPacket);
        }
        if format.channels() != self.channel_map.len() {
            return Err(ConfigError::ChannelMismatch {
                config: self.channel_map.len(),
                format: format.channels(),
            });
        }
        self.channel_map
            .len()
            .checked_mul(self.frames_per_packet)
            .ok_or(ConfigError::SizeOverflow)?;
        let needed = format.encoded_len(self.frames_per_packet);
        if needed > self.max_payload {
            return Err(ConfigError::PacketTooLarge {
                needed,
                max: self.max_payload,
            });
        }
        Ok(())
    }
}

/// Atomic TX metrics, shared with the control thread via [`Arc`].
#[derive(Debug, Default)]
pub struct TxMetrics {
    /// Packets produced.
    pub packets: Counter,
    /// Frames in produced packets.
    pub frames: Counter,
    /// Bytes in produced packets.
    pub bytes: Counter,
    /// Channel-samples the source could not supply (sent as silence).
    pub underrun_samples: Counter,
    /// Times the flow skipped forward instead of catching up.
    pub late_skips: Counter,
    /// Packets that were never sent because of a skip.
    pub skipped_packets: Counter,
    /// Re-anchors caused by an epoch change or a backward clock step.
    pub resyncs: Counter,
    /// Largest delay between a packet's release time and when it was sent.
    pub max_send_lag_ns: Counter,
    /// Per wire position, the largest absolute sample sent since the last
    /// [`TxMetrics::take_peaks`] (signal metering).
    pub peaks: alloc::boxed::Box<[Counter]>,
    /// Monotonic time (ns) of the latest datagram the flow's receiver sent
    /// back (a keepalive), 0 if none yet. Written by the I/O loop that owns
    /// the socket; read by the control plane to expire silent receivers.
    pub last_keepalive_ns: Counter,
}

impl TxMetrics {
    /// Reads and resets the per-position peaks (for the periodic heartbeat).
    pub fn take_peaks(&self) -> impl Iterator<Item = u64> + '_ {
        self.peaks.iter().map(Counter::take)
    }

    /// A plain copy of all counters.
    #[must_use]
    pub fn snapshot(&self) -> TxSnapshot {
        TxSnapshot {
            packets: self.packets.get(),
            frames: self.frames.get(),
            bytes: self.bytes.get(),
            underrun_samples: self.underrun_samples.get(),
            late_skips: self.late_skips.get(),
            skipped_packets: self.skipped_packets.get(),
            resyncs: self.resyncs.get(),
            max_send_lag_ns: self.max_send_lag_ns.get(),
        }
    }
}

/// Plain-data copy of [`TxMetrics`]; field meanings are identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(missing_docs)]
pub struct TxSnapshot {
    pub packets: u64,
    pub frames: u64,
    pub bytes: u64,
    pub underrun_samples: u64,
    pub late_skips: u64,
    pub skipped_packets: u64,
    pub resyncs: u64,
    pub max_send_lag_ns: u64,
}

/// A packet produced by [`TxFlow::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxPacket {
    /// Media time of the first frame (the value stamped into the header).
    pub timestamp: u64,
    /// Datagram length in bytes (the prefix of the caller's buffer).
    pub len: usize,
    /// Frames in the packet.
    pub frames: usize,
    /// Packets skipped immediately before this one because the flow had
    /// stalled (0 in normal operation).
    pub skipped: u64,
}

/// Result of one [`TxFlow::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxPoll {
    /// A datagram is in the caller's buffer, ready to send.
    Sent(TxPacket),
    /// The flow had stalled, skipped forward, and the resumed packet is not
    /// due yet (only possible with `max_lag` below one packet period).
    Skipped {
        /// Packets that will never be sent.
        packets: u64,
    },
    /// Nothing is due yet; see [`TxFlow::next_due_ns`].
    NotDue,
}

/// One TX flow. See the [module documentation](self).
#[derive(Debug)]
pub struct TxFlow<F: PacketFormat> {
    cfg: TxConfig,
    format: F,
    fpp: u64,
    max_lag_ns: u64,
    /// Planar scratch, `channels * fpp`.
    planes: Vec<i32>,
    /// Interleaved scratch, `fpp * channels`.
    interleaved: Vec<i32>,
    next_ts: Option<u64>,
    epoch: Option<u64>,
    dither: Option<Dither>,
    metrics: Arc<TxMetrics>,
}

impl<F: PacketFormat> TxFlow<F> {
    /// Builds a flow, allocating its scratch buffers (control thread only).
    ///
    /// # Errors
    /// See [`TxConfig::validate`].
    pub fn new(cfg: TxConfig, format: F) -> Result<Self, ConfigError> {
        cfg.validate(&format)?;
        let n = cfg.channel_map.len() * cfg.frames_per_packet;
        Ok(Self {
            fpp: cfg.frames_per_packet as u64,
            max_lag_ns: cfg
                .max_lag_ns
                .unwrap_or_else(|| cfg.packet_period_ns().saturating_mul(2)),
            planes: vec![0; n],
            interleaved: vec![0; n],
            next_ts: None,
            epoch: None,
            dither: cfg.dither_seed.map(Dither::new),
            metrics: Arc::new(TxMetrics {
                peaks: (0..cfg.channel_map.len())
                    .map(|_| Counter::default())
                    .collect(),
                ..TxMetrics::default()
            }),
            cfg,
            format,
        })
    }

    /// The shared metrics handle.
    #[must_use]
    pub fn metrics(&self) -> Arc<TxMetrics> {
        Arc::clone(&self.metrics)
    }

    /// The metrics, borrowed (no reference count change on the RT path).
    #[must_use]
    pub fn metrics_ref(&self) -> &TxMetrics {
        &self.metrics
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &TxConfig {
        &self.cfg
    }

    /// Bytes of the datagram buffer one packet needs.
    #[must_use]
    pub fn packet_len(&self) -> usize {
        self.format.encoded_len(self.cfg.frames_per_packet)
    }

    /// Media timestamp of the next packet to be sent, once anchored.
    #[must_use]
    pub fn next_timestamp(&self) -> Option<u64> {
        self.next_ts
    }

    /// Media time (ns) at which the next packet becomes due, once anchored.
    /// A driver sleeps until this instant (mapped to local time).
    #[must_use]
    pub fn next_due_ns(&self) -> Option<u64> {
        self.next_ts.map(|ts| self.due_ns(ts))
    }

    /// Forgets the schedule; the next poll anchors at the then-current time.
    pub fn reset(&mut self) {
        self.next_ts = None;
    }

    fn due_ns(&self, ts: u64) -> u64 {
        samples_to_ns_ceil(ts.saturating_add(self.fpp), self.cfg.sample_rate)
            .saturating_add(self.cfg.send_offset_ns)
    }

    /// First timestamp of a flow that starts (or re-anchors) at media time
    /// `now_samples`.
    fn anchor_ts(&self, now_samples: u64) -> u64 {
        if self.cfg.align_to_packet {
            now_samples
                .div_ceil(self.fpp)
                .checked_mul(self.fpp)
                .unwrap_or(now_samples)
        } else {
            now_samples
        }
    }

    /// Produces at most one packet.
    ///
    /// `out` must hold at least [`packet_len`](Self::packet_len) bytes.
    /// Call repeatedly until it returns [`TxPoll::NotDue`] (continue on
    /// [`TxPoll::Skipped`]); each call does a bounded amount of work.
    /// Allocation-free. `out` is checked first, so a too-small buffer is an
    /// error even when nothing is due, and never changes any state.
    ///
    /// # Errors
    /// [`TxError::OutputTooSmall`] (state unchanged) or a format error.
    pub fn poll<C: MediaClock, S: FrameSource + ?Sized>(
        &mut self,
        clock: &C,
        source: &mut S,
        out: &mut [u8],
    ) -> Result<TxPoll, TxError> {
        let needed = self.packet_len();
        if out.len() < needed {
            return Err(TxError::OutputTooSmall { needed });
        }
        let rate = self.cfg.sample_rate;
        let snap = clock.now();
        let now_ns = snap.media_ns;
        let now_samples = ns_to_samples(now_ns, rate);
        let m = &*self.metrics;

        let epoch = snap.epoch;
        let ts = match (self.next_ts, self.epoch == Some(epoch)) {
            (Some(ts), true) => ts,
            (prev, _) => {
                if prev.is_some() {
                    m.resyncs.inc();
                }
                self.epoch = Some(epoch);
                let ts = self.anchor_ts(now_samples);
                self.next_ts = Some(ts);
                ts
            }
        };

        // Media time stepped backwards: the schedule is in the future.
        let back = ns_to_samples_ceil(self.cfg.resync_back_ns, rate);
        let ts = if ts > now_samples.saturating_add(self.fpp).saturating_add(back) {
            m.resyncs.inc();
            let ts = self.anchor_ts(now_samples);
            self.next_ts = Some(ts);
            ts
        } else {
            ts
        };

        let due = self.due_ns(ts);
        if now_ns < due {
            return Ok(TxPoll::NotDue);
        }
        let mut ts = ts;
        let mut skipped = 0;
        let lag = now_ns - due;

        if lag > self.max_lag_ns {
            // Resume at the latest packet that is already due (k packets
            // ahead of `ts`). The estimate is exact to within one packet;
            // the loops fix the rounding.
            let due_at = |k: u64| self.due_ns(ts.saturating_add(k.saturating_mul(self.fpp)));
            let mut k = ns_to_samples(lag, rate) / self.fpp;
            for _ in 0..4 {
                if due_at(k + 1) <= now_ns {
                    k += 1;
                } else {
                    break;
                }
            }
            for _ in 0..4 {
                if k > 0 && due_at(k) > now_ns {
                    k -= 1;
                } else {
                    break;
                }
            }
            // If even the latest due packet is beyond max_lag (only possible
            // with a max_lag below one period), skip it too.
            if now_ns - due_at(k) > self.max_lag_ns {
                k += 1;
            }
            ts = ts.saturating_add(k.saturating_mul(self.fpp));
            self.next_ts = Some(ts);
            m.late_skips.inc();
            m.skipped_packets.add(k);
            skipped = k;
            if now_ns < self.due_ns(ts) {
                // Even the resumed packet is not due yet (max_lag below one
                // period): nothing to send this time.
                return Ok(TxPoll::Skipped { packets: k });
            }
            // Otherwise send the resumed packet now, in this same call, so a
            // driver that loops until NotDue cannot fall a tick behind.
        }
        let lag = now_ns - self.due_ns(ts);

        // Gather each wire position from the client ring (or silence).
        let fpp = self.cfg.frames_per_packet;
        let channels = self.cfg.channel_map.len();
        let mut underrun = 0u64;
        for ((plane, map), peak) in self
            .planes
            .chunks_exact_mut(fpp)
            .zip(&self.cfg.channel_map)
            .zip(m.peaks.iter())
        {
            match map {
                Some(src_ch) => {
                    let valid = source.read(*src_ch, ts, plane).min(fpp);
                    underrun += (fpp - valid) as u64;
                    let level = plane.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
                    peak.record_max(u64::from(level));
                }
                None => plane.iter_mut().for_each(|s| *s = 0),
            }
        }
        // Interleave: frame-major, wire position order.
        for (j, plane) in self.planes.chunks_exact(fpp).enumerate() {
            for (frame, s) in self.interleaved.chunks_exact_mut(channels).zip(plane) {
                if let Some(slot) = frame.get_mut(j) {
                    *slot = *s;
                }
            }
        }
        let len = self
            .format
            .encode(ts, &self.interleaved, out, self.dither.as_mut())
            .map_err(TxError::Format)?;

        self.next_ts = Some(ts.saturating_add(self.fpp));
        m.packets.inc();
        m.frames.add(self.fpp);
        m.bytes.add(len as u64);
        m.underrun_samples.add(underrun);
        m.max_send_lag_ns.record_max(lag);
        Ok(TxPoll::Sent(TxPacket {
            timestamp: ts,
            len,
            frames: fpp,
            skipped,
        }))
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::buffer::MediaBuffer;
    use crate::convert::WireFormat;
    use crate::packet::Decoded;
    use crate::reference::ReferenceFormat;
    use crate::time::ClockSnapshot;
    use core::cell::Cell;

    struct Clk {
        now: Cell<u64>,
        epoch: Cell<u64>,
    }

    impl Clk {
        fn new(now: u64) -> Self {
            Clk {
                now: Cell::new(now),
                epoch: Cell::new(0),
            }
        }
    }

    impl MediaClock for Clk {
        fn now(&self) -> ClockSnapshot {
            self.at_local(self.now.get())
        }
        fn at_local(&self, l: u64) -> ClockSnapshot {
            ClockSnapshot {
                epoch: self.epoch.get(),
                media_ns: l,
            }
        }
    }

    const RATE: u32 = 48_000;
    const T0: u64 = 10_000_000_000; // 10 s

    fn tx(ch: usize, fpp: usize) -> (TxFlow<ReferenceFormat>, ReferenceFormat) {
        let f = ReferenceFormat::new(ch, RATE, WireFormat::BE24).unwrap();
        (TxFlow::new(TxConfig::new(RATE, ch, fpp), f).unwrap(), f)
    }

    /// A source holding positions around T0 (8000 samples starting 64 before it).
    fn filled_source(ch: usize) -> MediaBuffer {
        let from = ns_to_samples(T0, RATE) - 64;
        let to = from + 8000;
        let mut b = MediaBuffer::new(ch, 8192).unwrap();
        for c in 0..ch {
            let data: Vec<i32> = (from..to)
                .map(|p| ((p as i32 & 0xFFFF) + 1 + c as i32 * 100_000) << 8)
                .collect();
            b.write(c, from, &data);
        }
        b
    }

    fn decode(f: &ReferenceFormat, d: &[u8]) -> (Decoded, Vec<i32>) {
        let mut s = vec![0i32; 64 * f.channels()];
        let dec = f.decode(d, &mut s).unwrap();
        s.truncate(dec.frames * f.channels());
        (dec, s)
    }

    #[test]
    fn config_validation() {
        let f = ReferenceFormat::new(8, RATE, WireFormat::BE24).unwrap();
        // 9 + 58 * 8 * 3 = 1401 > 1400: ADR 0011 says 57 is the maximum.
        assert_eq!(
            TxConfig::new(RATE, 8, 58).validate(&f),
            Err(ConfigError::PacketTooLarge {
                needed: 1401,
                max: 1400
            })
        );
        assert!(TxConfig::new(RATE, 8, 57).validate(&f).is_ok());
        assert_eq!(
            TxConfig::new(RATE, 8, 0).validate(&f),
            Err(ConfigError::ZeroFramesPerPacket)
        );
        assert_eq!(
            TxConfig::new(0, 8, 32).validate(&f),
            Err(ConfigError::ZeroSampleRate)
        );
        assert_eq!(
            TxConfig::new(RATE, 4, 32).validate(&f),
            Err(ConfigError::ChannelMismatch {
                config: 4,
                format: 8
            })
        );
        let by_period = TxConfig::with_packet_period(RATE, 2, 666_667);
        assert_eq!(by_period.frames_per_packet, 32);
        assert_eq!(by_period.packet_period_ns(), 666_667);
    }

    #[test]
    fn not_sent_before_last_frame_time_plus_offset() {
        let (mut tx, f) = tx(2, 32);
        let mut src = filled_source(2);
        let clk = Clk::new(T0);
        let mut buf = vec![0u8; tx.packet_len()];
        // First poll anchors at the next multiple of 32 samples.
        assert_eq!(tx.poll(&clk, &mut src, &mut buf).unwrap(), TxPoll::NotDue);
        let ts = tx.next_timestamp().unwrap();
        assert_eq!(ts % 32, 0);
        let now_samples = ns_to_samples(T0, RATE);
        assert!(ts >= now_samples && ts < now_samples + 32);
        let due = tx.next_due_ns().unwrap();
        assert_eq!(due, samples_to_ns_ceil(ts + 32, RATE));
        // One ns before due: nothing. At due: the packet.
        clk.now.set(due - 1);
        assert_eq!(tx.poll(&clk, &mut src, &mut buf).unwrap(), TxPoll::NotDue);
        clk.now.set(due);
        let TxPoll::Sent(p) = tx.poll(&clk, &mut src, &mut buf).unwrap() else {
            panic!()
        };
        assert_eq!((p.timestamp, p.frames, p.len), (ts, 32, tx.packet_len()));
        let (d, s) = decode(&f, &buf[..p.len]);
        assert_eq!((d.timestamp, d.frames), (ts, 32));
        // Interleaved: ch0 then ch1 per frame, content from the positions.
        assert_eq!(s[0], ((ts as i32 & 0xFFFF) + 1) << 8);
        assert_eq!(s[1], ((ts as i32 & 0xFFFF) + 1 + 100_000) << 8);
        assert_eq!(s[2], (((ts + 1) as i32 & 0xFFFF) + 1) << 8);
        // Next packet is not due until one packet period later.
        assert_eq!(tx.poll(&clk, &mut src, &mut buf).unwrap(), TxPoll::NotDue);
        assert_eq!(tx.next_timestamp(), Some(ts + 32));
    }

    #[test]
    fn send_offset_delays_release() {
        let f = ReferenceFormat::new(1, RATE, WireFormat::BE24).unwrap();
        let mut cfg = TxConfig::new(RATE, 1, 32);
        cfg.send_offset_ns = 300_000;
        let mut tx = TxFlow::new(cfg, f).unwrap();
        let mut src = filled_source(1);
        let clk = Clk::new(T0);
        let mut buf = vec![0u8; tx.packet_len()];
        tx.poll(&clk, &mut src, &mut buf).unwrap();
        let due = tx.next_due_ns().unwrap();
        assert_eq!(
            due,
            samples_to_ns_ceil(tx.next_timestamp().unwrap() + 32, RATE) + 300_000
        );
        clk.now.set(due - 1);
        assert_eq!(tx.poll(&clk, &mut src, &mut buf).unwrap(), TxPoll::NotDue);
        clk.now.set(due);
        assert!(matches!(
            tx.poll(&clk, &mut src, &mut buf).unwrap(),
            TxPoll::Sent(_)
        ));
    }

    #[test]
    fn steady_state_is_gapless_and_never_early() {
        let (mut tx, _f) = tx(2, 32);
        let mut src = filled_source(2);
        let clk = Clk::new(T0);
        let mut buf = vec![0u8; tx.packet_len()];
        let mut last_end = None;
        let mut sent = 0;
        // Tick every 100 us for 50 ms.
        for i in 0..500u64 {
            clk.now.set(T0 + i * 100_000);
            while let TxPoll::Sent(p) = tx.poll(&clk, &mut src, &mut buf).unwrap() {
                // Never before the end of the last frame.
                assert!(clk.now.get() >= samples_to_ns_ceil(p.timestamp + 32, RATE));
                if let Some(e) = last_end {
                    assert_eq!(p.timestamp, e, "timestamps must be contiguous");
                }
                last_end = Some(p.timestamp + 32);
                sent += 1;
            }
        }
        assert!((70..=76).contains(&sent), "sent {sent}");
        assert_eq!(tx.metrics().snapshot().late_skips, 0);
    }

    #[test]
    fn unwritten_positions_are_silence_and_counted() {
        let (mut tx, f) = tx(2, 32);
        let clk = Clk::new(T0);
        let mut buf = vec![0u8; tx.packet_len()];
        tx.poll(&clk, &mut MediaBuffer::new(2, 64).unwrap(), &mut buf)
            .unwrap();
        let ts = tx.next_timestamp().unwrap();
        // The client wrote only the first 20 frames of channel 0.
        let mut src = MediaBuffer::new(2, 8192).unwrap();
        src.write(0, ts, &[1 << 8; 20]);
        clk.now.set(tx.next_due_ns().unwrap());
        let TxPoll::Sent(p) = tx.poll(&clk, &mut src, &mut buf).unwrap() else {
            panic!()
        };
        let (_, s) = decode(&f, &buf[..p.len]);
        for frame in 0..32 {
            assert_eq!(s[frame * 2], if frame < 20 { 1 << 8 } else { 0 });
            assert_eq!(s[frame * 2 + 1], 0);
        }
        assert_eq!(tx.metrics().snapshot().underrun_samples, 12 + 32);
    }

    #[test]
    fn silent_channel_positions_and_reordered_map() {
        let f = ReferenceFormat::new(3, RATE, WireFormat::BE24).unwrap();
        let mut cfg = TxConfig::new(RATE, 3, 8);
        cfg.channel_map = vec![Some(1), None, Some(0)];
        let mut tx = TxFlow::new(cfg, f).unwrap();
        let mut src = MediaBuffer::new(2, 8192).unwrap();
        let clk = Clk::new(T0);
        let mut buf = vec![0u8; tx.packet_len()];
        tx.poll(&clk, &mut src, &mut buf).unwrap();
        let ts = tx.next_timestamp().unwrap();
        src.write(0, ts, &[5 << 8; 8]);
        src.write(1, ts, &[7 << 8; 8]);
        clk.now.set(tx.next_due_ns().unwrap());
        let TxPoll::Sent(p) = tx.poll(&clk, &mut src, &mut buf).unwrap() else {
            panic!()
        };
        let (_, s) = decode(&f, &buf[..p.len]);
        assert_eq!(&s[..3], &[7 << 8, 0, 5 << 8]);
        // The silent position is not an underrun.
        assert_eq!(tx.metrics().snapshot().underrun_samples, 0);
    }

    #[test]
    fn repeated_source_is_encoded_in_each_wire_slot() {
        let f = ReferenceFormat::new(2, RATE, WireFormat::BE24).unwrap();
        let mut cfg = TxConfig::new(RATE, 2, 8);
        cfg.channel_map = vec![Some(0), Some(0)];
        let mut tx = TxFlow::new(cfg, f).unwrap();
        let mut src = MediaBuffer::new(1, 8192).unwrap();
        let clk = Clk::new(T0);
        let mut buf = vec![0u8; tx.packet_len()];
        tx.poll(&clk, &mut src, &mut buf).unwrap();
        let ts = tx.next_timestamp().unwrap();
        let samples = [1, 2, 3, 4, 5, 6, 7, 8].map(|s| s << 8);
        src.write(0, ts, &samples);
        clk.now.set(tx.next_due_ns().unwrap());
        let TxPoll::Sent(p) = tx.poll(&clk, &mut src, &mut buf).unwrap() else {
            panic!()
        };
        let (_, decoded) = decode(&f, &buf[..p.len]);
        assert_eq!(
            decoded,
            samples.into_iter().flat_map(|s| [s, s]).collect::<Vec<_>>()
        );
        // Each wire position meters the samples it carried.
        assert_eq!(
            tx.metrics().take_peaks().collect::<Vec<_>>(),
            [8 << 8, 8 << 8]
        );
        assert_eq!(tx.metrics().take_peaks().collect::<Vec<_>>(), [0, 0]);
        assert_eq!(tx.metrics().snapshot().underrun_samples, 0);
    }

    #[test]
    fn late_flow_skips_forward_instead_of_bursting() {
        let (mut tx, _f) = tx(1, 32);
        let mut src = filled_source(1);
        let clk = Clk::new(T0);
        let mut buf = vec![0u8; tx.packet_len()];
        tx.poll(&clk, &mut src, &mut buf).unwrap();
        let ts0 = tx.next_timestamp().unwrap();
        // The thread stalls for 50 ms (75 packet periods).
        let stall = tx.next_due_ns().unwrap() + 50_000_000;
        clk.now.set(stall);
        // One call skips the stale packets and sends the latest due one (small
        // lag); then nothing else is due. No burst.
        let r = tx.poll(&clk, &mut src, &mut buf).unwrap();
        let TxPoll::Sent(p) = r else { panic!("{r:?}") };
        let packets = p.skipped;
        assert!((74..=77).contains(&packets), "{packets}");
        let ts1 = ts0 + packets * 32;
        assert_eq!(p.timestamp, ts1);
        assert!(stall - samples_to_ns_ceil(ts1 + 32, RATE) < 666_667);
        assert_eq!(tx.poll(&clk, &mut src, &mut buf).unwrap(), TxPoll::NotDue);
        assert!(tx.next_due_ns().unwrap() > stall);
        let s = tx.metrics().snapshot();
        assert_eq!(
            (s.late_skips, s.skipped_packets, s.packets),
            (1, packets, 1)
        );
    }

    #[test]
    fn lag_up_to_two_periods_is_not_a_stall() {
        // Two packets due at one wake-up (fixed-period polling with a small
        // clock-rate difference): the older is up to one period late and
        // must be sent, not discarded.
        let (mut tx, _f) = tx(1, 32);
        let mut src = filled_source(1);
        let clk = Clk::new(T0);
        let mut buf = vec![0u8; tx.packet_len()];
        tx.poll(&clk, &mut src, &mut buf).unwrap();
        let ts0 = tx.next_timestamp().unwrap();
        clk.now.set(tx.next_due_ns().unwrap() + 666_667 + 1_000);
        let TxPoll::Sent(a) = tx.poll(&clk, &mut src, &mut buf).unwrap() else {
            panic!()
        };
        let TxPoll::Sent(b) = tx.poll(&clk, &mut src, &mut buf).unwrap() else {
            panic!()
        };
        assert_eq!((a.timestamp, b.timestamp), (ts0, ts0 + 32));
        assert_eq!(tx.poll(&clk, &mut src, &mut buf).unwrap(), TxPoll::NotDue);
        assert_eq!(tx.metrics().snapshot().late_skips, 0);
    }

    #[test]
    fn small_max_lag_discards_a_due_packet_that_is_too_old() {
        // max_lag below one period: when even the latest due packet is
        // beyond max_lag it is skipped too.
        let f = ReferenceFormat::new(1, RATE, WireFormat::BE24).unwrap();
        let mut cfg = TxConfig::new(RATE, 1, 32);
        cfg.max_lag_ns = Some(100_000);
        let mut tx = TxFlow::new(cfg, f).unwrap();
        let mut src = filled_source(1);
        let clk = Clk::new(T0);
        let mut buf = vec![0u8; tx.packet_len()];
        tx.poll(&clk, &mut src, &mut buf).unwrap();
        let ts0 = tx.next_timestamp().unwrap();
        clk.now.set(tx.next_due_ns().unwrap() + 300_000);
        assert_eq!(
            tx.poll(&clk, &mut src, &mut buf).unwrap(),
            TxPoll::Skipped { packets: 1 }
        );
        assert_eq!(tx.next_timestamp(), Some(ts0 + 32));
        assert_eq!(tx.poll(&clk, &mut src, &mut buf).unwrap(), TxPoll::NotDue);
    }

    #[test]
    fn small_lag_within_max_lag_is_still_sent() {
        let (mut tx, _f) = tx(1, 32);
        let mut src = filled_source(1);
        let clk = Clk::new(T0);
        let mut buf = vec![0u8; tx.packet_len()];
        tx.poll(&clk, &mut src, &mut buf).unwrap();
        // 0.5 packet periods late: sends this one, then the next is not yet due.
        clk.now.set(tx.next_due_ns().unwrap() + 300_000);
        assert!(matches!(
            tx.poll(&clk, &mut src, &mut buf).unwrap(),
            TxPoll::Sent(_)
        ));
        let lag = tx.metrics().snapshot().max_send_lag_ns;
        assert_eq!(lag, 300_000);
        assert_eq!(tx.poll(&clk, &mut src, &mut buf).unwrap(), TxPoll::NotDue);
    }

    #[test]
    fn epoch_change_and_backward_step_reanchor() {
        let (mut tx, _f) = tx(1, 32);
        let mut src = filled_source(1);
        let clk = Clk::new(T0);
        let mut buf = vec![0u8; tx.packet_len()];
        tx.poll(&clk, &mut src, &mut buf).unwrap();
        let ts0 = tx.next_timestamp().unwrap();
        clk.epoch.set(5);
        clk.now.set(T0 + 3_000_000_000);
        tx.poll(&clk, &mut src, &mut buf).unwrap();
        let ts1 = tx.next_timestamp().unwrap();
        assert!(ts1 > ts0 + 100_000);
        assert_eq!(tx.metrics().snapshot().resyncs, 1);
        // Media time steps back by 1 s without an epoch change.
        clk.now.set(T0 + 2_000_000_000);
        tx.poll(&clk, &mut src, &mut buf).unwrap();
        assert!(tx.next_timestamp().unwrap() < ts1);
        assert_eq!(tx.metrics().snapshot().resyncs, 2);
        assert_eq!(tx.metrics().snapshot().late_skips, 0);
    }

    #[test]
    fn small_output_buffer_is_an_error_not_a_panic() {
        let (mut tx, _f) = tx(2, 32);
        let mut src = filled_source(2);
        let clk = Clk::new(T0);
        let mut buf = vec![0u8; tx.packet_len()];
        tx.poll(&clk, &mut src, &mut buf).unwrap();
        clk.now.set(tx.next_due_ns().unwrap());
        let mut small = [0u8; 10];
        assert_eq!(
            tx.poll(&clk, &mut src, &mut small),
            Err(TxError::OutputTooSmall {
                needed: 9 + 32 * 2 * 3
            })
        );
        // State unchanged: the packet can still be produced.
        assert!(matches!(
            tx.poll(&clk, &mut src, &mut buf).unwrap(),
            TxPoll::Sent(_)
        ));
    }

    /// One 16-bit-wire packet of the constant `0x1234_8000`, with the given
    /// dither seed; returns the decoded samples.
    fn narrow_packet(dither_seed: Option<u64>) -> Vec<i32> {
        let f = ReferenceFormat::new(1, RATE, WireFormat::BE16).unwrap();
        let mut cfg = TxConfig::new(RATE, 1, 64);
        cfg.dither_seed = dither_seed;
        let mut tx = TxFlow::new(cfg, f).unwrap();
        let clk = Clk::new(T0);
        let mut src = MediaBuffer::new(1, 8192).unwrap();
        let mut buf = vec![0u8; tx.packet_len()];
        tx.poll(&clk, &mut src, &mut buf).unwrap();
        let ts = tx.next_timestamp().unwrap();
        src.write(0, ts, &[0x1234_8000; 64]);
        clk.now.set(tx.next_due_ns().unwrap());
        let TxPoll::Sent(p) = tx.poll(&clk, &mut src, &mut buf).unwrap() else {
            panic!()
        };
        decode(&f, &buf[..p.len]).1
    }

    #[test]
    fn dither_is_off_by_default_and_opt_in() {
        // Default: plain truncation, bit exact.
        assert!(narrow_packet(None).iter().all(|s| *s == 0x1234_0000));
        // Configured: reproducible, within one LSB of the target width of
        // the rounded value, and actually varying.
        let a = narrow_packet(Some(42));
        assert_eq!(a, narrow_packet(Some(42)));
        assert_ne!(a, narrow_packet(Some(43)));
        assert!(a.iter().any(|s| *s != 0x1234_0000));
        assert!(a.iter().all(|s| (*s - 0x1234_8000).abs() <= 3 << 15));
    }

    #[test]
    fn unaligned_flow_starts_at_now() {
        let f = ReferenceFormat::new(1, RATE, WireFormat::BE24).unwrap();
        let mut cfg = TxConfig::new(RATE, 1, 32);
        cfg.align_to_packet = false;
        let mut tx = TxFlow::new(cfg, f).unwrap();
        let clk = Clk::new(T0 + 7_000);
        let mut src = MediaBuffer::new(1, 64).unwrap();
        tx.poll(&clk, &mut src, &mut [0u8; 200]).unwrap();
        assert_eq!(tx.next_timestamp(), Some(ns_to_samples(T0 + 7_000, RATE)));
    }
}
