//! Receiver keepalives of unicast rx flows.
//!
//! A transmitter interoperable with Dante® stops a unicast flow when its
//! receiver stays silent: the receiver sends a small datagram from the flow's
//! own socket (its bound address and port) to the exact source address and
//! port of the media packets, learned from the most recent valid media
//! datagram, every [`DEFAULT_INTERVAL_NS`]. Nothing is sent before the first
//! media packet.
//!
//! Evidence (PROVISIONAL, `likely`):
//!
//! * `spec/protocol/control-flows.md` 7.4 and `interop-fixes.md` F3: hardware
//!   receivers send a **zero-length** UDP datagram every 250 ms, the first
//!   right after the first media datagram, FROM the receive port named in the
//!   flow request TO the source ip:port of the media; this supersedes the
//!   reference's 2-byte `13 37` (`flows-media.md` 4).
//! * `control-flows.md` 7.5: a hardware transmitter stopped 1.2 to 1.3 s after
//!   a flow started without keepalives (the reference's 4 s is a `guess`).
//!
//! The payload stays configuration ([`KeepalivePayload`]): the default is the
//! zero-length datagram, `13 37` remains available for the reference.
//!
//! [`Keepalive`] is the sans-IO scheduler: the worker reports each valid media
//! datagram with [`Keepalive::on_media`] (a few stores, no syscall: fine at
//! thousands of packets per second) and asks [`Keepalive::poll`] once per
//! tick whether to send one now and where. It is `Copy`, allocation-free and
//! driven by the worker's monotonic time only (never `CLOCK_REALTIME`), so it
//! is unit-tested in virtual time.
//!
//! Choices where the evidence is silent (all configuration, see
//! `docs/configuration.md`):
//!
//! * The first keepalive goes out on the first tick after the first valid
//!   media datagram (and after media resumes from a timeout), then every
//!   interval. A late tick never produces a burst: the cadence restarts from
//!   that tick.
//! * A source change (new address or port) is followed at the next due
//!   keepalive; the cadence is kept.
//! * Keepalives stop after [`DEFAULT_TIMEOUT_NS`] without valid media and start
//!   again with the next one. The reference keeps sending while the flow
//!   exists; the timeout is well above the transmitters' 1.3 s so that a
//!   receive-side outage never makes us abandon a transmitter that is still
//!   running, while a sender that is gone is not addressed forever.

use std::net::SocketAddrV4;

/// What a keepalive datagram carries. PROVISIONAL (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeepalivePayload {
    /// A zero-length datagram, as hardware receivers send (`control-flows.md`
    /// 7.4, `interop-fixes.md` F3, `likely`).
    #[default]
    Empty,
    /// The two bytes `13 37` of the reference implementation
    /// (`flows-media.md` 4, `likely`).
    Reference,
}

/// The `13 37` payload of the reference implementation.
pub const REFERENCE_PAYLOAD: [u8; 2] = [0x13, 0x37];

impl KeepalivePayload {
    /// The bytes to send.
    #[must_use]
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Self::Empty => &[],
            Self::Reference => &REFERENCE_PAYLOAD,
        }
    }
}

/// Default interval between keepalives, nanoseconds (250 ms:
/// `spec/protocol/control-flows.md` 7.4, `interop-fixes.md` F3, `likely`).
pub const DEFAULT_INTERVAL_NS: u64 = 250_000_000;

/// Default silence after which keepalives stop, nanoseconds (a tessera
/// choice: well above the transmitters' timeouts).
pub const DEFAULT_TIMEOUT_NS: u64 = 10_000_000_000;

/// Keepalive settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeepaliveConfig {
    /// Interval, nanoseconds; `0` disables keepalives.
    pub interval_ns: u64,
    /// Stop after this long without valid media, nanoseconds; `0`: never stop
    /// once a source is known (the reference behaviour).
    pub timeout_ns: u64,
    /// What each keepalive carries.
    pub payload: KeepalivePayload,
}

impl Default for KeepaliveConfig {
    fn default() -> Self {
        Self {
            interval_ns: DEFAULT_INTERVAL_NS,
            timeout_ns: DEFAULT_TIMEOUT_NS,
            payload: KeepalivePayload::default(),
        }
    }
}

/// The keepalive scheduler of one rx flow. See the [module documentation](self).
#[derive(Debug, Clone, Copy)]
pub struct Keepalive {
    cfg: KeepaliveConfig,
    target: Option<SocketAddrV4>,
    last_media_ns: u64,
    next_due_ns: u64,
    active: bool,
}

impl Keepalive {
    /// A scheduler that has seen no media yet.
    #[must_use]
    pub fn new(cfg: KeepaliveConfig) -> Self {
        Self {
            cfg,
            target: None,
            last_media_ns: 0,
            next_due_ns: 0,
            active: false,
        }
    }

    /// A valid media datagram from `src` arrived at monotonic `now_ns`.
    /// Call it only for datagrams that passed the flow's checks and decode as
    /// media: anything else must not move the keepalives.
    pub fn on_media(&mut self, src: SocketAddrV4, now_ns: u64) {
        self.target = Some(src);
        self.last_media_ns = now_ns;
        if !self.active {
            self.active = true;
            self.next_due_ns = now_ns;
        }
    }

    /// Where to send a keepalive now (monotonic `now_ns`), if one is due.
    /// At most one per call; the caller sends [`Keepalive::payload`] from the flow's
    /// socket.
    pub fn poll(&mut self, now_ns: u64) -> Option<SocketAddrV4> {
        if self.cfg.interval_ns == 0 || !self.active {
            return None;
        }
        if self.cfg.timeout_ns != 0
            && now_ns.saturating_sub(self.last_media_ns) >= self.cfg.timeout_ns
        {
            self.active = false;
            return None;
        }
        if now_ns < self.next_due_ns {
            return None;
        }
        let next = self.next_due_ns.saturating_add(self.cfg.interval_ns);
        // A tick that came late restarts the cadence instead of catching up.
        self.next_due_ns = if next > now_ns {
            next
        } else {
            now_ns.saturating_add(self.cfg.interval_ns)
        };
        self.target
    }

    /// The bytes each keepalive carries.
    #[must_use]
    pub fn payload(&self) -> &'static [u8] {
        self.cfg.payload.bytes()
    }

    /// The source learned from the last valid media datagram.
    #[must_use]
    pub fn target(&self) -> Option<SocketAddrV4> {
        self.target
    }

    /// Whether keepalives are being sent (media seen, not timed out).
    #[must_use]
    pub fn active(&self) -> bool {
        self.active && self.cfg.interval_ns != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const MS: u64 = 1_000_000;

    fn src(port: u16) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), port)
    }

    /// Runs `ka` on a 1 ms tick from `from` to `to` ms (exclusive), feeding a
    /// media datagram from `media` every `media_every` ms while `media` is
    /// set; returns the times (ms) and targets of the keepalives.
    fn run(
        ka: &mut Keepalive,
        from: u64,
        to: u64,
        media: Option<SocketAddrV4>,
        media_every: u64,
    ) -> Vec<(u64, SocketAddrV4)> {
        let mut out = Vec::new();
        for t in from..to {
            if let Some(s) = media {
                if t % media_every == 0 {
                    ka.on_media(s, t * MS);
                }
            }
            if let Some(d) = ka.poll(t * MS) {
                out.push((t, d));
            }
        }
        out
    }

    #[test]
    fn nothing_is_sent_before_the_first_media_datagram() {
        let mut ka = Keepalive::new(KeepaliveConfig::default());
        assert!(run(&mut ka, 0, 10_000, None, 1).is_empty());
        assert_eq!(ka.target(), None);
        assert!(!ka.active());
    }

    #[test]
    fn the_first_packet_starts_a_250_ms_cadence_to_its_source() {
        let mut ka = Keepalive::new(KeepaliveConfig::default());
        let sent = run(&mut ka, 1_000, 3_000, Some(src(40_000)), 1);
        let times: Vec<u64> = sent.iter().map(|s| s.0).collect();
        assert_eq!(
            times,
            [1_000, 1_250, 1_500, 1_750, 2_000, 2_250, 2_500, 2_750]
        );
        assert!(sent.iter().all(|s| s.1 == src(40_000)));
        assert!(ka.active());
    }

    #[test]
    fn keepalives_stop_after_the_timeout_without_media_and_resume_with_it() {
        let cfg = KeepaliveConfig {
            interval_ns: 250 * MS,
            timeout_ns: 1_000 * MS,
            ..KeepaliveConfig::default()
        };
        let mut ka = Keepalive::new(cfg);
        // Media until 999 ms, then silence.
        assert_eq!(run(&mut ka, 0, 1_000, Some(src(1)), 1).len(), 4);
        let quiet = run(&mut ka, 1_000, 5_000, None, 1);
        let times: Vec<u64> = quiet.iter().map(|s| s.0).collect();
        // Last media at 999 ms: sends at 1000, 1250, 1500, 1750; silent from 1999.
        assert_eq!(times, [1_000, 1_250, 1_500, 1_750]);
        assert!(!ka.active());
        // The next valid datagram re-arms it at once.
        ka.on_media(src(1), 5_000 * MS);
        assert_eq!(ka.poll(5_000 * MS), Some(src(1)));
        assert_eq!(ka.poll(5_100 * MS), None);
        assert_eq!(ka.poll(5_250 * MS), Some(src(1)));
    }

    #[test]
    fn a_zero_timeout_keeps_sending_while_the_flow_exists() {
        let mut ka = Keepalive::new(KeepaliveConfig {
            interval_ns: 250 * MS,
            timeout_ns: 0,
            ..KeepaliveConfig::default()
        });
        ka.on_media(src(1), 0);
        assert_eq!(run(&mut ka, 0, 60_000, None, 1).len(), 240);
    }

    #[test]
    fn a_zero_interval_disables_keepalives() {
        let mut ka = Keepalive::new(KeepaliveConfig {
            interval_ns: 0,
            timeout_ns: DEFAULT_TIMEOUT_NS,
            ..KeepaliveConfig::default()
        });
        assert!(run(&mut ka, 0, 5_000, Some(src(1)), 1).is_empty());
        assert!(!ka.active());
    }

    #[test]
    fn a_source_change_is_followed_and_the_cadence_kept() {
        let mut ka = Keepalive::new(KeepaliveConfig::default());
        let a = run(&mut ka, 0, 600, Some(src(1)), 1);
        let b = run(&mut ka, 600, 1_100, Some(src(2)), 1);
        assert_eq!(
            a,
            [(0, src(1)), (250, src(1)), (500, src(1))],
            "first source"
        );
        assert_eq!(b, [(750, src(2)), (1_000, src(2))], "the new source port");
        let other = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 3), 2);
        ka.on_media(other, 1_100 * MS);
        assert_eq!(ka.poll(1_250 * MS), Some(other), "a new address too");
    }

    #[test]
    fn a_datagram_that_is_not_media_does_not_move_the_target() {
        // The worker calls `on_media` only for valid media (see the worker
        // test for the filtering); a stray datagram is never reported, so the
        // target stays where media came from.
        let mut ka = Keepalive::new(KeepaliveConfig::default());
        ka.on_media(src(1), 0);
        assert_eq!(ka.poll(0), Some(src(1)));
        assert_eq!(ka.poll(250 * MS), Some(src(1)));
        assert_eq!(ka.target(), Some(src(1)));
    }

    #[test]
    fn a_late_tick_sends_one_keepalive_and_never_a_burst() {
        let mut ka = Keepalive::new(KeepaliveConfig::default());
        ka.on_media(src(1), 0);
        assert_eq!(ka.poll(0), Some(src(1)));
        // The worker was descheduled for a second.
        ka.on_media(src(1), 1_000 * MS);
        assert_eq!(ka.poll(1_000 * MS), Some(src(1)));
        assert_eq!(ka.poll(1_001 * MS), None, "no catch-up burst");
        assert_eq!(ka.poll(1_249 * MS), None);
        assert_eq!(ka.poll(1_250 * MS), Some(src(1)));
    }

    #[test]
    fn the_payload_is_empty_by_default_and_the_reference_bytes_on_request() {
        let ka = Keepalive::new(KeepaliveConfig::default());
        assert!(ka.payload().is_empty(), "a zero-length datagram");
        let ka = Keepalive::new(KeepaliveConfig {
            payload: KeepalivePayload::Reference,
            ..KeepaliveConfig::default()
        });
        assert_eq!(ka.payload(), &[0x13, 0x37]);
    }

    #[test]
    fn a_high_packet_rate_still_gives_four_keepalives_a_second() {
        // 6000 packets/s (96 kHz, 16 frames per packet), polled on a 1 ms
        // tick: the rate of media does not change the rate of keepalives.
        let mut ka = Keepalive::new(KeepaliveConfig::default());
        let mut sent = 0;
        for us in (0..2_000_000u64).step_by(167) {
            ka.on_media(src(9), us * 1_000);
            if us % 1_000 < 167 && ka.poll(us * 1_000).is_some() {
                sent += 1;
            }
        }
        assert_eq!(sent, 8, "two seconds at 4 per second");
    }
}
