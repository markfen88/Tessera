//! What should run: one [`BridgePlan`] per enabled `[[bridge]]`, resolved
//! against the effective device (rate, latency, channel count) and the
//! process switches. Pure, so the reconciler and the tests share it.

use std::time::Duration;

use tessera_media::Quality;
use tessera_state::Config;
use tessera_state::config::{Backoff, BridgeDirection, Resampler};
use tessera_state::model::DeviceModel;

/// One bridge as the bridge manager runs it.
#[derive(Debug, Clone, PartialEq)]
pub struct BridgePlan {
    /// `[[bridge]] name`.
    pub name: String,
    /// Device key.
    pub device: String,
    /// ALSA PCM name.
    pub pcm: String,
    /// Playback (network to card) or capture.
    pub direction: BridgeDirection,
    /// First device channel (1-based).
    pub first_channel: u16,
    /// Channels.
    pub channels: u16,
    /// The device's rate, which the card must run at.
    pub rate: u32,
    /// Card period, frames.
    pub period_frames: u32,
    /// Card buffer, frames.
    pub buffer_frames: u32,
    /// Resampler quality (resolved, never `auto`).
    pub quality: Resampler,
    /// The device's latency, microseconds (the RX read position).
    pub latency_us: u32,
    /// The device's largest latency, microseconds (sizes the ring).
    pub latency_max_us: u32,
    /// `SCHED_FIFO` priority of the bridge thread; `None`: normal.
    pub rt_priority: Option<u8>,
    /// CPU to pin the bridge thread to.
    pub cpu: Option<usize>,
    /// PI time constant, seconds.
    pub time_constant_s: f64,
    /// Correction clamp, ppm.
    pub max_correction_ppm: f64,
    /// The manager's wait before it reopens a failed card
    /// (`error_backoff*` of the entry over `[defaults.bridge]`). Only the
    /// reopen delays of the manager thread: not the bridge thread's own
    /// short pause after an xrun (that is [`error_sleep`](Self::error_sleep)).
    pub backoff: Backoff,
    /// The bridge thread's pause after a step that met an xrun or a card
    /// error (`error_sleep*` of the entry over `[defaults.bridge]`; ADR 0004
    /// amendment of 2026-10-18, ADR 0014). Applied to a running bridge at
    /// its next error, without reopening the card.
    pub error_sleep: Backoff,
}

impl BridgePlan {
    /// True if `other` can be applied to a running bridge without reopening
    /// the card: only the loop tuning (time constant, clamp) and the error
    /// back-off (used at the next error) differ.
    #[must_use]
    pub fn same_card(&self, other: &BridgePlan) -> bool {
        let strip = |p: &BridgePlan| BridgePlan {
            time_constant_s: 0.0,
            max_correction_ppm: 0.0,
            backoff: Backoff {
                doubling: false,
                min_ms: 0,
                max_ms: 0,
            },
            error_sleep: Backoff {
                doubling: false,
                min_ms: 0,
                max_ms: 0,
            },
            ..p.clone()
        };
        strip(self) == strip(other)
    }

    /// How long the manager waits before reopening the card after
    /// `failures` consecutive failures (1 for the first): `min_ms` doubled
    /// each time up to `max_ms`, or always `min_ms` with `error_backoff =
    /// false`. Never zero: validation keeps `min_ms` at 10 ms or more, so a
    /// card that keeps failing cannot make the manager spin, and a run of
    /// 10 s resets the count.
    #[must_use]
    pub fn reopen_wait(&self, failures: u32) -> Duration {
        Duration::from_millis(u64::from(self.backoff.wait_ms(failures.max(1)).max(1)))
    }

    /// The resampler quality for the bridge library.
    #[must_use]
    pub fn media_quality(&self) -> Quality {
        match self.quality {
            Resampler::Off => Quality::Off,
            Resampler::Linear => Quality::Linear,
            Resampler::Cubic => Quality::Cubic,
            Resampler::SincHq => Quality::SincHq,
            Resampler::SincFast | Resampler::Auto => Quality::SincFast,
        }
    }

    /// The natural frequency of the loop for its time constant
    /// (`f_n = 1 / (2 pi tau)`).
    #[must_use]
    pub fn loop_bandwidth_hz(&self) -> f64 {
        1.0 / (2.0 * std::f64::consts::PI * self.time_constant_s)
    }

    /// The largest position error, frames, the loop is expected to let build
    /// up (`tessera_backend_alsa::bridge::transient_frames`, the same
    /// formula: `0.4 * clamp * tau * rate`). The bridge sizes its guard and
    /// lead for it when it opens the card.
    #[must_use]
    pub fn transient_frames(&self) -> u64 {
        transient(self.rate, self.time_constant_s, self.max_correction_ppm)
    }

    /// The correction clamp as a fraction.
    #[must_use]
    pub fn max_correction(&self) -> f64 {
        self.max_correction_ppm * 1e-6
    }
}

/// `0.4 * clamp * tau * rate` frames, rounded up (see
/// [`BridgePlan::transient_frames`]).
#[must_use]
pub fn transient(rate: u32, tau_s: f64, ppm: f64) -> u64 {
    let b = 0.4 * ppm * 1e-6 * tau_s * f64::from(rate);
    if b.is_finite() && b >= 0.0 {
        b.ceil().min(1e12) as u64
    } else {
        1 << 40
    }
}

/// The platform default resampler (ADR 0006): `cubic` on ARM, `sinc-fast`
/// elsewhere.
#[must_use]
pub fn platform_resampler() -> Resampler {
    if cfg!(any(target_arch = "arm", target_arch = "aarch64")) {
        Resampler::Cubic
    } else {
        Resampler::SincFast
    }
}

/// An enabled bridge that cannot run on the effective devices, with what
/// its status row shows.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanError {
    /// `[[bridge]] name`.
    pub name: String,
    /// Device key.
    pub device: String,
    /// ALSA PCM name.
    pub pcm: String,
    /// Direction.
    pub direction: BridgeDirection,
    /// First device channel.
    pub first_channel: u16,
    /// Channels (0 if the device is unknown).
    pub channels: u16,
    /// Why it cannot run.
    pub reason: String,
}

/// Plans for every enabled bridge, and errors for those that cannot run on
/// the effective devices (a device whose rate was changed at run time away
/// from a rate the bridge pins). Disabled bridges are left out (the state
/// engine shows them as `disabled` from the configuration).
#[must_use]
pub fn plan_bridges(
    cfg: &Config,
    devices: &[DeviceModel],
    no_rt: bool,
) -> (Vec<BridgePlan>, Vec<PlanError>) {
    let mut plans = Vec::new();
    let mut errors = Vec::new();
    for b in cfg.bridges.iter().filter(|b| b.enabled) {
        let error = |channels: u16, reason: String| PlanError {
            name: b.name.clone(),
            device: b.device.clone(),
            pcm: b.alsa_pcm.clone(),
            direction: b.direction,
            first_channel: b.first_channel,
            channels,
            reason,
        };
        let Some(d) = devices.iter().find(|d| d.key.0 == b.device) else {
            errors.push(error(0, format!("no device `{}`", b.device)));
            continue;
        };
        let available = match b.direction {
            BridgeDirection::Playback => d.rx_channels,
            BridgeDirection::Capture => d.tx_channels,
        };
        let channels = b.channel_count(available);
        // A device whose count was reduced at run time (or a direction at 0):
        // the bridge cannot run rather than open a card for channels the
        // device does not have. The state engine refuses such changes for
        // explicit ranges; "the rest of the channels" can become empty.
        let dir = match b.direction {
            BridgeDirection::Playback => "RX",
            BridgeDirection::Capture => "TX",
        };
        if channels == 0 {
            errors.push(error(
                0,
                format!(
                    "device `{}` has no {dir} channels from channel {} on",
                    b.device, b.first_channel
                ),
            ));
            continue;
        }
        if u32::from(b.first_channel) + u32::from(channels) - 1 > u32::from(available) {
            errors.push(error(
                channels,
                format!(
                    "channels {}-{} are beyond the {available} {dir} channel(s) of device `{}`",
                    b.first_channel,
                    u32::from(b.first_channel) + u32::from(channels) - 1,
                    b.device
                ),
            ));
            continue;
        }
        if let Some(r) = b.sample_rate.filter(|r| *r != d.sample_rate) {
            errors.push(error(
                channels,
                format!(
                    "device `{}` runs at {} Hz, the bridge's card at {r} Hz: the bridge compensates drift, not rate differences",
                    b.device, d.sample_rate
                ),
            ));
            continue;
        }
        let quality = match b.resampler {
            Resampler::Auto => match cfg.backends.resampler {
                Resampler::Auto => platform_resampler(),
                r => r,
            },
            r => r,
        };
        plans.push(BridgePlan {
            name: b.name.clone(),
            device: b.device.clone(),
            pcm: b.alsa_pcm.clone(),
            direction: b.direction,
            first_channel: b.first_channel,
            channels,
            rate: d.sample_rate,
            period_frames: b.period(d.sample_rate),
            buffer_frames: b.buffer(d.sample_rate),
            quality,
            latency_us: d.latency_us,
            latency_max_us: d.latency_bounds_us[1].max(d.latency_us),
            rt_priority: (!no_rt && b.rt_priority > 0).then_some(b.rt_priority),
            cpu: b.cpu.map(|c| c as usize),
            time_constant_s: b.time_constant_s,
            max_correction_ppm: b.max_correction_ppm,
            backoff: b.backoff(&cfg.defaults.bridge),
            error_sleep: b.error_sleep(&cfg.defaults.bridge),
        });
    }
    (plans, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use tessera_control_types::DeviceKey;
    use tessera_state::config::BridgeConfig;
    use tessera_state::model::WireDeviceId;

    fn dev(rate: u32) -> DeviceModel {
        DeviceModel {
            key: DeviceKey("main".into()),
            wire_device_id: WireDeviceId([1; 8]),
            name: "n".into(),
            sample_rate: rate,
            encoding: 24,
            latency_us: 4_000,
            rx_channels: 8,
            tx_channels: 2,
            rx_labels: BTreeMap::new(),
            tx_labels: BTreeMap::new(),
            subscriptions: BTreeMap::new(),
            locked: BTreeSet::new(),
            latency_bounds_us: [1_000, 40_000],
            allow_unsafe_latency: false,
            latency_floor_us: 1_000,
        }
    }

    fn cfg(bridges: Vec<BridgeConfig>) -> Config {
        Config {
            bridges,
            ..Config::default()
        }
    }

    #[test]
    fn a_plan_resolves_the_defaults_against_the_device() {
        let mut b = BridgeConfig::new("main", "hw:Loopback,0,0", BridgeDirection::Playback);
        b.first_channel = 3;
        let (p, e) = plan_bridges(&cfg(vec![b]), &[dev(48_000)], false);
        assert!(e.is_empty());
        let p = &p[0];
        assert_eq!((p.first_channel, p.channels), (3, 6));
        assert_eq!(
            (p.rate, p.period_frames, p.buffer_frames),
            (48_000, 480, 1920)
        );
        assert_eq!(p.quality, platform_resampler());
        assert_eq!((p.latency_us, p.latency_max_us), (4_000, 40_000));
        assert_eq!(p.rt_priority, Some(75));
        assert_eq!((p.time_constant_s, p.max_correction_ppm), (120.0, 200.0));
        assert_eq!(p.cpu, None, "not pinned unless asked");
        // 120 s: about 1.33 mHz.
        assert!((p.loop_bandwidth_hz() - 0.001_326_3).abs() < 1e-6);
        assert!((p.max_correction() - 2e-4).abs() < 1e-12);
    }

    #[test]
    fn disabled_bridges_are_left_out_and_no_rt_is_honoured() {
        let mut a = BridgeConfig::new("main", "hw:0", BridgeDirection::Playback);
        a.enabled = false;
        let b = BridgeConfig::new("main", "hw:1", BridgeDirection::Capture);
        let (p, _) = plan_bridges(&cfg(vec![a, b]), &[dev(48_000)], true);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].pcm, "hw:1");
        assert_eq!(p[0].channels, 2);
        assert_eq!(p[0].rt_priority, None);
    }

    #[test]
    fn a_device_rate_changed_away_from_a_pinned_rate_is_an_error() {
        let mut b = BridgeConfig::new("main", "hw:0", BridgeDirection::Playback);
        b.sample_rate = Some(48_000);
        let (p, e) = plan_bridges(&cfg(vec![b.clone()]), &[dev(96_000)], false);
        assert!(p.is_empty());
        assert_eq!(e[0].name, "main-playback");
        assert!(e[0].reason.contains("96000 Hz"), "{}", e[0].reason);
        // Not pinned: the bridge follows the device.
        b.sample_rate = None;
        let (p, _) = plan_bridges(&cfg(vec![b]), &[dev(96_000)], false);
        assert_eq!((p[0].rate, p[0].period_frames), (96_000, 960));
    }

    #[test]
    fn the_back_off_comes_from_the_entry_over_the_defaults() {
        let mut c = cfg(vec![BridgeConfig::new(
            "main",
            "hw:0",
            BridgeDirection::Playback,
        )]);
        let (p, _) = plan_bridges(&c, &[dev(48_000)], false);
        assert_eq!(
            p[0].backoff,
            Backoff {
                doubling: true,
                min_ms: 500,
                max_ms: 30_000
            },
            "the former BACKOFF_MIN / BACKOFF_MAX"
        );
        // Doubling from min to max: 0.5, 1, 2, 4, 8, 16, 30, 30 s.
        let waits: Vec<u64> = (1..=8)
            .map(|n| p[0].reopen_wait(n).as_millis() as u64)
            .collect();
        assert_eq!(waits, [500, 1000, 2000, 4000, 8000, 16_000, 30_000, 30_000]);
        assert_eq!(p[0].reopen_wait(0), p[0].reopen_wait(1));
        assert_eq!(p[0].reopen_wait(u32::MAX).as_millis(), 30_000);

        c.defaults.bridge.error_backoff_min_ms = 20;
        c.defaults.bridge.error_backoff_max_ms = 100;
        c.bridges[0].error_backoff_max_ms = Some(50);
        let (p, _) = plan_bridges(&c, &[dev(48_000)], false);
        let waits: Vec<u64> = (1..=4)
            .map(|n| p[0].reopen_wait(n).as_millis() as u64)
            .collect();
        assert_eq!(waits, [20, 40, 50, 50], "the entry's max over the default");

        // `error_backoff = false`: every retry waits min_ms, never zero.
        c.bridges[0].error_backoff = Some(false);
        let (p, _) = plan_bridges(&c, &[dev(48_000)], false);
        assert!((1..=10).all(|n| p[0].reopen_wait(n) == Duration::from_millis(20)));
    }

    #[test]
    fn a_back_off_or_pause_change_does_not_reopen_the_card() {
        let b = BridgeConfig::new("main", "hw:0", BridgeDirection::Playback);
        let (p, _) = plan_bridges(&cfg(vec![b]), &[dev(48_000)], false);
        assert_eq!(
            p[0].error_sleep,
            Backoff {
                doubling: true,
                min_ms: 2,
                max_ms: 64
            },
            "the former compiled-in 2 to 64 ms"
        );
        let mut t = p[0].clone();
        t.backoff.min_ms = 10;
        t.backoff.doubling = false;
        t.error_sleep.max_ms = 8;
        assert!(p[0].same_card(&t), "applies at the next error");
    }

    #[test]
    fn a_direction_reduced_to_nothing_or_below_the_range_is_an_error_not_a_card() {
        // CFG-ZERO: the device restarted into an rx-only (tx 0) and a
        // tx-only (rx 0) shape; "the rest of the channels" becomes empty.
        let play = BridgeConfig::new("main", "hw:0", BridgeDirection::Playback);
        let capt = BridgeConfig::new("main", "hw:1", BridgeDirection::Capture);
        let mut tx_only = dev(48_000);
        tx_only.rx_channels = 0;
        let (p, e) = plan_bridges(&cfg(vec![play.clone(), capt.clone()]), &[tx_only], false);
        assert_eq!(p.len(), 1, "the capture bridge still runs: {e:?}");
        assert_eq!((p[0].pcm.as_str(), p[0].channels), ("hw:1", 2));
        assert_eq!(e.len(), 1);
        assert!(e[0].reason.contains("no RX channels"), "{}", e[0].reason);
        let mut rx_only = dev(48_000);
        rx_only.tx_channels = 0;
        let (p, e) = plan_bridges(&cfg(vec![play.clone(), capt]), &[rx_only], false);
        assert_eq!((p.len(), p[0].channels), (1, 8));
        assert!(e[0].reason.contains("no TX channels"), "{}", e[0].reason);
        // An explicit range beyond a reduced count.
        let mut fixed = play;
        fixed.first_channel = 3;
        fixed.channels = Some(4);
        let mut small = dev(48_000);
        small.rx_channels = 4;
        let (p, e) = plan_bridges(&cfg(vec![fixed]), &[small], false);
        assert!(p.is_empty());
        assert!(e[0].reason.contains("beyond the 4 RX"), "{}", e[0].reason);
    }

    #[test]
    fn only_the_loop_tuning_applies_without_a_reopen() {
        let b = BridgeConfig::new("main", "hw:0", BridgeDirection::Playback);
        let (p, _) = plan_bridges(&cfg(vec![b]), &[dev(48_000)], false);
        let a = p[0].clone();
        let mut t = a.clone();
        t.time_constant_s = 30.5;
        t.max_correction_ppm = 0.5;
        assert!(a.same_card(&t));
        for change in [
            |p: &mut BridgePlan| p.channels = 1,
            |p: &mut BridgePlan| p.rate = 96_000,
            |p: &mut BridgePlan| p.latency_us = 5_000,
            |p: &mut BridgePlan| p.period_frames = 256,
            |p: &mut BridgePlan| p.pcm = "hw:1".into(),
            |p: &mut BridgePlan| p.quality = Resampler::Linear,
            |p: &mut BridgePlan| p.rt_priority = Some(60),
        ] {
            let mut c = a.clone();
            change(&mut c);
            assert!(!a.same_card(&c), "{c:?}");
        }
    }

    /// The reload classification from configuration text, as the
    /// reconciler sees it: which edits of a `[[bridge]]` restart the card
    /// and which are applied to the running loop.
    #[test]
    fn which_edits_restart_and_which_retune() {
        use tessera_state::loader::load_from_sources;
        use tessera_state::{Environment, Source};
        let mut env = Environment::new("h");
        env.bridges_supported = true;
        let base = "[[device]]\nid = \"main\"\nrx_channels = 4\ntx_channels = 4\n\
                    [[bridge]]\nname = \"b\"\ndevice = \"main\"\nalsa_pcm = \"hw:0\"\nchannels = 2\n";
        let plan = |extra: &str| {
            let l = load_from_sources(&[Source::new("c.toml", format!("{base}{extra}"))], &env)
                .unwrap_or_else(|e| panic!("{extra}: {e}"));
            plan_bridges(&l.config, &[dev(48_000)], false).0.remove(0)
        };
        let a = plan("");
        for retune in [
            "time_constant_s = 30",
            "max_correction_ppm = 50.5",
            "time_constant_s = 240.25\nmax_correction_ppm = 0.5",
        ] {
            let b = plan(retune);
            assert!(a.same_card(&b), "{retune} must not restart");
            assert_ne!(a, b, "{retune} changes the plan");
        }
        for restart in [
            "alsa_pcm = \"hw:1\"",
            "direction = \"capture\"",
            "first_channel = 2",
            "period_frames = 256",
            "buffer_frames = 4096",
            "resampler = \"linear\"",
            "rt_priority = 60",
            "cpu = 1",
            "sample_rate = 48000\nperiod_frames = 240",
        ] {
            let text = if restart.starts_with("alsa_pcm") {
                plan_text_replace(base, restart)
            } else {
                String::new()
            };
            let b = if text.is_empty() {
                plan(restart)
            } else {
                let l = load_from_sources(&[Source::new("c.toml", &text)], &env).unwrap();
                plan_bridges(&l.config, &[dev(48_000)], false).0.remove(0)
            };
            assert!(!a.same_card(&b), "{restart} must restart");
        }
        // A device change (rate, latency) restarts too.
        let l = load_from_sources(&[Source::new("c.toml", base)], &env).unwrap();
        let mut d = dev(96_000);
        let b = plan_bridges(&l.config, &[d.clone()], false).0.remove(0);
        assert!(!a.same_card(&b));
        d.sample_rate = 48_000;
        d.latency_us = 8_000;
        let b = plan_bridges(&l.config, &[d], false).0.remove(0);
        assert!(!a.same_card(&b));
    }

    fn plan_text_replace(base: &str, line: &str) -> String {
        base.replace("alsa_pcm = \"hw:0\"", line)
    }

    #[test]
    fn the_backends_resampler_is_the_fallback_for_auto() {
        let b = BridgeConfig::new("main", "hw:0", BridgeDirection::Playback);
        let mut c = cfg(vec![b]);
        c.backends.resampler = Resampler::Linear;
        let (p, _) = plan_bridges(&c, &[dev(48_000)], false);
        assert_eq!(p[0].quality, Resampler::Linear);
        assert_eq!(p[0].media_quality(), Quality::Linear);
    }
}
