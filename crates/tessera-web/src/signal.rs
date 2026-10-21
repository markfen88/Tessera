//! The signal path on the overview page: network, flows, device, audio
//! clients, with the paths that carry audio right now highlighted.
//!
//! "Carries audio" means the packet counters of at least one flow in that
//! direction moved between two samples of the metrics.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use maud::{Markup, html};
use tessera_control_types::{Direction, Metrics, Status};

/// Packet counters per flow at one moment.
#[derive(Debug, Clone)]
pub struct Sample {
    at: Instant,
    packets: HashMap<String, u64>,
}

impl Sample {
    /// The counters in `m`, taken at `at`.
    #[must_use]
    pub fn of(m: &Metrics, at: Instant) -> Self {
        Self {
            at,
            packets: m.flows.iter().map(|f| (f.id.clone(), f.packets)).collect(),
        }
    }

    /// Is this sample recent enough to compare with one taken at `now`?
    #[must_use]
    pub fn usable_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.at) < Duration::from_secs(60)
    }
}

/// Traffic in one direction.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Way {
    /// Flows in this direction.
    pub flows: usize,
    /// Packets per second over the sample interval.
    pub pps: f64,
}

impl Way {
    /// Packets are moving.
    #[must_use]
    pub fn active(&self) -> bool {
        self.pps > 0.0
    }
}

/// Receive and transmit traffic between two samples.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Activity {
    /// Network to device.
    pub rx: Way,
    /// Device to network.
    pub tx: Way,
}

/// Compare `now` with `prev` (`None`: nothing is known to move).
#[must_use]
pub fn activity(prev: Option<&Sample>, m: &Metrics, now: &Sample) -> Activity {
    let mut a = Activity::default();
    let secs = prev
        .map(|p| now.at.saturating_duration_since(p.at).as_secs_f64())
        .unwrap_or(0.0);
    for f in &m.flows {
        let way = match f.direction {
            Direction::Rx => &mut a.rx,
            _ => &mut a.tx,
        };
        way.flows += 1;
        if let Some(before) = prev.and_then(|p| p.packets.get(&f.id)) {
            if secs > 0.0 && f.packets > *before {
                #[allow(clippy::cast_precision_loss)]
                let d = (f.packets - before) as f64;
                way.pps += d / secs;
            }
        }
    }
    a
}

fn class(on: bool) -> &'static str {
    if on { "edge on" } else { "edge" }
}

/// The diagram.
#[must_use]
pub fn render(s: &Status, m: Option<&Metrics>, a: Activity) -> Markup {
    let net = s.network.as_ref();
    let iface = net
        .and_then(|n| n.primary.clone())
        .unwrap_or_else(|| "none".into());
    let addr = net
        .and_then(|n| {
            n.interfaces
                .iter()
                .find(|i| Some(&i.name) == n.primary.as_ref())
                .and_then(|i| i.ipv4.clone())
        })
        .unwrap_or_default();
    let clock = s.clocks.first().map_or_else(
        || "no clock".to_owned(),
        |c| format!("clock {:?}", c.state).to_lowercase(),
    );
    let dev = s.devices.first();
    let dev_name = dev.map_or_else(|| "no device".to_owned(), |d| d.name.clone());
    let dev_fmt = dev.map_or_else(String::new, |d| {
        format!(
            "{} kHz / {} bit",
            f64::from(d.sample_rate.0) / 1000.0,
            d.encoding.0
        )
    });
    let receiving = s
        .devices
        .iter()
        .flat_map(|d| &d.subscriptions)
        .filter(|x| matches!(x.state, tessera_control_types::SubscriptionState::Receiving))
        .count();
    let clients = m.map_or(0, |m| m.audio.sessions);
    let audio_on = clients > 0;
    let boxes: [(&str, String, String, bool); 4] = [
        (
            "Network",
            iface,
            format!("{addr} {clock}"),
            a.rx.active() || a.tx.active(),
        ),
        (
            "Flows",
            format!("RX {} · TX {}", a.rx.flows, a.tx.flows),
            format!("{:.0} / {:.0} pkt/s", a.rx.pps, a.tx.pps),
            a.rx.active() || a.tx.active(),
        ),
        (
            "Device",
            dev_name,
            format!("{dev_fmt} · {receiving} receiving"),
            a.rx.active() || a.tx.active(),
        ),
        (
            "Audio clients",
            format!("{clients} attached"),
            "PipeWire / ALSA / JACK".into(),
            audio_on,
        ),
    ];
    let (w, gap, bw, bh) = (960, 40, 200, 96);
    html! {
        svg class="signal" viewBox={"0 0 " (w) " 150"} role="img" aria-label="Signal path" {
            title { "Signal path: network, flows, device, audio clients" }
            @for (i, (title, main, sub, on)) in boxes.iter().enumerate() {
                @let x = 20 + i * (bw + gap);
                g class=(if *on { "node on" } else { "node" }) {
                    rect x=(x) y="20" width=(bw) height=(bh) rx="10" {}
                    text x=(x + 12) y="44" class="node-title" { (title) }
                    text x=(x + 12) y="72" class="node-main" { (main) }
                    text x=(x + 12) y="96" class="node-sub" { (sub) }
                }
                @if i + 1 < boxes.len() {
                    @let x0 = x + bw;
                    @let x1 = x0 + gap;
                    @let rx_on = a.rx.active() && (i < 2 || audio_on);
                    @let tx_on = a.tx.active() && (i < 2 || audio_on);
                    g class=(class(rx_on)) {
                        title { "receive" }
                        line x1=(x0 + 4) y1="56" x2=(x1 - 8) y2="56" {}
                        path d={"M" (x1 - 8) " 50 L" (x1 - 2) " 56 L" (x1 - 8) " 62 Z"} {}
                    }
                    g class=(class(tx_on)) {
                        title { "transmit" }
                        line x1=(x0 + 8) y1="82" x2=(x1 - 4) y2="82" {}
                        path d={"M" (x0 + 8) " 76 L" (x0 + 2) " 82 L" (x0 + 8) " 88 Z"} {}
                    }
                }
            }
            text x="20" y="142" class="legend" {
                "Highlighted: packets moved in the last few seconds. Upper arrows: receive; lower arrows: transmit."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_control_types::{DeviceKey, FlowMetrics};

    fn metrics(rx: u64, tx: u64) -> Metrics {
        let mut m = Metrics::default();
        let mut f = FlowMetrics::new("in", DeviceKey("d".into()), Direction::Rx);
        f.packets = rx;
        let mut g = FlowMetrics::new("out", DeviceKey("d".into()), Direction::Tx);
        g.packets = tx;
        m.flows = vec![f, g];
        m
    }

    #[test]
    fn moving_counters_light_the_path() {
        let t0 = Instant::now();
        let a = metrics(100, 50);
        let s0 = Sample::of(&a, t0);
        let b = metrics(1100, 50);
        let s1 = Sample::of(&b, t0 + Duration::from_secs(1));
        let act = activity(Some(&s0), &b, &s1);
        assert_eq!(act.rx.flows, 1);
        assert!((act.rx.pps - 1000.0).abs() < 1e-6);
        assert!(act.rx.active() && !act.tx.active());
        assert!(!activity(None, &b, &s1).rx.active());
    }
}
