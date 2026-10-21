//! The live state of RX subscriptions, from what the media layer receives.
//!
//! Until flow control exists, a subscription is realised by a static `[[flow]]`
//! (PROVISIONAL, `docs/configuration.md`): an rx flow of the device that
//! carries the subscription's rx channel. This module watches those flows and
//! reports, for every channel an rx flow carries, whether media is arriving:
//!
//! | Flow | State |
//! |---|---|
//! | packets arrived within [`RECENT`] | `receiving` |
//! | had packets, but none for [`RECENT`] | `no_media` |
//! | never had one, started less than [`START_GRACE`] ago | `in_progress` |
//! | never had one, started longer ago | `no_media` |
//! | no flow carries the channel | `in_progress` (nothing is reported; the state engine resets it) |
//!
//! Where several flows carry a channel the best state wins, in the order
//! `in_progress` (still starting) < `no_media` < `receiving`: a channel is
//! `receiving` if any flow delivers it, `no_media` only if every flow that
//! carries it is silent past its grace. It does **not**
//! check that the media comes from the transmitter named in the subscription
//! (a static flow does not say), and it has no source for `unresolved`,
//! `flow_refused`, `rate_mismatch` or `encoding_mismatch`: those wait for the
//! control plane.
//!
//! **Threading.** [`run`] is a tokio task on the control plane. Once per
//! [`SAMPLE_EVERY`] it reads the packet counter each rx flow already keeps
//! (a relaxed atomic load, through the registry the metrics query reads) and
//! sends one sweep to the state engine thread, which records changes only and
//! publishes `Event::SubscriptionChanged` for them. Nothing runs on the
//! real-time worker and nothing is added to its tick.
//!
//! A sweep is the complete picture, sent every time: a dropped or late one is
//! repaired by the next, and the state engine's own forgetting of a removed
//! or re-pointed subscription cannot leave this side out of step.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tessera_control_types::{Channel, DeviceKey, SubscriptionState};
use tokio_util::sync::CancellationToken;

use crate::engine_thread::EngineHandle;
use crate::media::stats::{MediaStats, RxActivity};

/// How often the flows are looked at.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(1);
/// A flow is `receiving` if its packet count grew less than this ago. The
/// count is looked at every [`SAMPLE_EVERY`], so a stop shows as `no_media`
/// 2 to 3 seconds after the last packet, a restart as `receiving` within one
/// second of the first. It is deliberately not a whole number of sample
/// periods: sample times jitter, and a limit of exactly two periods would tip
/// either way from one stop to the next (3 s or 4 s). One and a half periods
/// means "one of the last two samples saw packets".
pub const RECENT: Duration = Duration::from_millis(1_500);
/// A flow that has not received anything yet is `in_progress` this long after
/// it started (the transmitter may still be starting), then `no_media`.
pub const START_GRACE: Duration = Duration::from_secs(5);

/// One sweep: the state of every rx channel some flow carries.
pub type Sweep = Vec<(DeviceKey, Channel, SubscriptionState)>;

/// What the next sample compares against, per flow.
#[derive(Debug)]
struct Seen {
    packets: u64,
    /// When a sample last saw the packet count grow.
    advanced: Option<Instant>,
}

/// The state of one flow; the order is "better is greater".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Health {
    Starting,
    NoMedia,
    Receiving,
}

/// Remembers, per flow, when its packet count last grew.
///
/// Flows are told apart by their registration number, not their id: a flow
/// rebuilt under the same id (a configuration reload) is a new flow with a
/// new counter and does not inherit the old one's history.
#[derive(Debug, Default)]
pub struct Tracker {
    flows: HashMap<u64, Seen>,
}

impl Tracker {
    /// Fold one sample, taken at `now`, into the history and return the
    /// state of every channel the flows carry.
    pub fn observe(&mut self, now: Instant, flows: &[RxActivity]) -> Sweep {
        self.flows
            .retain(|serial, _| flows.iter().any(|f| f.serial == *serial));
        let mut best: BTreeMap<(&str, u16), Health> = BTreeMap::new();
        for f in flows {
            let seen = self.flows.entry(f.serial).or_insert(Seen {
                packets: 0,
                advanced: None,
            });
            if f.packets > seen.packets {
                seen.packets = f.packets;
                seen.advanced = Some(now);
            }
            let health = match seen.advanced {
                Some(t) if now.saturating_duration_since(t) < RECENT => Health::Receiving,
                Some(_) => Health::NoMedia,
                None if now.saturating_duration_since(f.started) < START_GRACE => Health::Starting,
                None => Health::NoMedia,
            };
            for ch in &f.channels {
                let b = best.entry((f.device.as_str(), *ch)).or_insert(health);
                *b = (*b).max(health);
            }
        }
        best.into_iter()
            .filter_map(|((device, ch), h)| {
                let state = match h {
                    Health::Receiving => SubscriptionState::Receiving,
                    Health::NoMedia => SubscriptionState::NoMedia,
                    Health::Starting => SubscriptionState::InProgress,
                };
                Some((DeviceKey(device.to_owned()), Channel::new(ch)?, state))
            })
            .collect()
    }
}

/// Report the state of the subscriptions every [`SAMPLE_EVERY`] until
/// shutdown.
pub async fn run(engine: EngineHandle, stats: Arc<MediaStats>, shutdown: CancellationToken) {
    let mut tick = tokio::time::interval(SAMPLE_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut tracker = Tracker::default();
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            _ = tick.tick() => {}
        }
        let sweep = tracker.observe(Instant::now(), &stats.rx_activity());
        // A full queue drops the sweep; the next one is a second away.
        let _ = engine.report_subscriptions(sweep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(n: u16) -> Channel {
        Channel::new(n).unwrap()
    }

    fn flow(serial: u64, channels: &[u16], packets: u64, started: Instant) -> RxActivity {
        RxActivity {
            serial,
            device: "main".into(),
            channels: channels.to_vec(),
            packets,
            started,
        }
    }

    fn state_of(sweep: &Sweep, rx: u16) -> Option<SubscriptionState> {
        sweep
            .iter()
            .find(|(d, c, _)| d.0 == "main" && *c == ch(rx))
            .map(|(_, _, s)| s.clone())
    }

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn a_flow_with_growing_counters_is_receiving_on_each_of_its_channels() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        let sweep = t.observe(t0 + S, &[flow(1, &[1, 2], 1_500, t0)]);
        assert_eq!(sweep.len(), 2);
        assert_eq!(state_of(&sweep, 1), Some(SubscriptionState::Receiving));
        assert_eq!(state_of(&sweep, 2), Some(SubscriptionState::Receiving));
        assert_eq!(state_of(&sweep, 3), None, "a channel no flow carries");
    }

    #[test]
    fn a_stopped_flow_becomes_no_media_after_the_recent_window_and_recovers() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        let mut now = t0 + S;
        let at = |t: &mut Tracker, now, p| t.observe(now, &[flow(1, &[1], p, t0)]);
        assert_eq!(
            state_of(&at(&mut t, now, 1_500), 1),
            Some(SubscriptionState::Receiving)
        );
        // Silence: still receiving one sample later, no media at the second.
        now += S;
        assert_eq!(
            state_of(&at(&mut t, now, 1_500), 1),
            Some(SubscriptionState::Receiving)
        );
        now += S;
        assert_eq!(
            state_of(&at(&mut t, now, 1_500), 1),
            Some(SubscriptionState::NoMedia)
        );
        now += 10 * S;
        assert_eq!(
            state_of(&at(&mut t, now, 1_500), 1),
            Some(SubscriptionState::NoMedia),
            "no start grace for a flow that had media"
        );
        // It resumes.
        now += S;
        assert_eq!(
            state_of(&at(&mut t, now, 1_600), 1),
            Some(SubscriptionState::Receiving)
        );
    }

    #[test]
    fn jitter_in_the_sample_times_does_not_move_the_stop_by_a_whole_period() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        for (first, second) in [(940, 1_020), (1_060, 940), (1_000, 1_000), (900, 1_100)] {
            let mut t = Tracker::default();
            let a = t0 + S;
            let _ = t.observe(a, &[flow(1, &[1], 1_500, t0)]);
            let sweep = t.observe(a + ms(first), &[flow(1, &[1], 1_500, t0)]);
            assert_eq!(state_of(&sweep, 1), Some(SubscriptionState::Receiving));
            let sweep = t.observe(a + ms(first + second), &[flow(1, &[1], 1_500, t0)]);
            assert_eq!(
                state_of(&sweep, 1),
                Some(SubscriptionState::NoMedia),
                "samples {first} ms and {} ms after the last advance",
                first + second
            );
        }
    }

    #[test]
    fn a_flow_that_never_got_a_packet_is_in_progress_for_the_start_grace_then_no_media() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        for secs in 0..5 {
            let sweep = t.observe(t0 + secs * S, &[flow(1, &[1], 0, t0)]);
            assert_eq!(
                state_of(&sweep, 1),
                Some(SubscriptionState::InProgress),
                "{secs} s after the start"
            );
        }
        let sweep = t.observe(t0 + START_GRACE, &[flow(1, &[1], 0, t0)]);
        assert_eq!(state_of(&sweep, 1), Some(SubscriptionState::NoMedia));
        let sweep = t.observe(t0 + START_GRACE + S, &[flow(1, &[1], 3, t0)]);
        assert_eq!(state_of(&sweep, 1), Some(SubscriptionState::Receiving));
    }

    #[test]
    fn a_rebuilt_flow_does_not_inherit_the_state_of_the_one_it_replaces() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        let sweep = t.observe(t0 + S, &[flow(1, &[1], 9_000, t0)]);
        assert_eq!(state_of(&sweep, 1), Some(SubscriptionState::Receiving));
        // Same id, new registration, nothing received yet.
        let t1 = t0 + 2 * S;
        let sweep = t.observe(t1, &[flow(2, &[1], 0, t1)]);
        assert_eq!(state_of(&sweep, 1), Some(SubscriptionState::InProgress));
        // Even with a counter that happens to be at or above the old one.
        let sweep = t.observe(t1 + S, &[flow(3, &[1], 9_000, t1 + S)]);
        assert_eq!(state_of(&sweep, 1), Some(SubscriptionState::Receiving));
        assert_eq!(t.flows.len(), 1, "the history of gone flows is dropped");
    }

    #[test]
    fn a_stopped_flow_is_no_longer_reported() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        assert_eq!(t.observe(t0 + S, &[flow(1, &[1], 5, t0)]).len(), 1);
        assert!(t.observe(t0 + 2 * S, &[]).is_empty());
        assert!(t.flows.is_empty());
    }

    #[test]
    fn where_flows_overlap_the_best_state_wins() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        let now = t0 + 10 * S;
        let a = flow(1, &[1, 2], 0, t0); // never received, past the grace
        let b = flow(2, &[2, 3], 0, now); // just started
        let sweep = t.observe(now, &[a.clone(), b]);
        assert_eq!(state_of(&sweep, 1), Some(SubscriptionState::NoMedia));
        assert_eq!(state_of(&sweep, 2), Some(SubscriptionState::NoMedia));
        assert_eq!(state_of(&sweep, 3), Some(SubscriptionState::InProgress));
        let c = flow(3, &[2], 40, t0);
        let sweep = t.observe(now + S, &[a, c]);
        assert_eq!(state_of(&sweep, 2), Some(SubscriptionState::Receiving));
        assert_eq!(state_of(&sweep, 1), Some(SubscriptionState::NoMedia));
    }

    #[test]
    fn two_flows_on_one_channel_one_receiving_one_silent_is_receiving() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        let now = t0 + 10 * S;
        // Both carry channel 1; only the second has ever received anything.
        let silent = flow(1, &[1], 0, t0);
        let live = flow(2, &[1], 480, t0);
        let sweep = t.observe(now, &[silent.clone(), live.clone()]);
        assert_eq!(state_of(&sweep, 1), Some(SubscriptionState::Receiving));
        // The order of the flows does not matter.
        let mut t = Tracker::default();
        let sweep = t.observe(now, &[live, silent]);
        assert_eq!(state_of(&sweep, 1), Some(SubscriptionState::Receiving));
    }

    #[test]
    fn flows_of_different_devices_do_not_mix() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        let mut other = flow(2, &[1], 0, t0);
        other.device = "second".into();
        let sweep = t.observe(t0 + S, &[flow(1, &[1], 10, t0), other]);
        assert_eq!(sweep.len(), 2);
        let second = sweep.iter().find(|(d, _, _)| d.0 == "second").unwrap();
        assert_eq!(second.2, SubscriptionState::InProgress);
        assert_eq!(state_of(&sweep, 1), Some(SubscriptionState::Receiving));
    }
}
