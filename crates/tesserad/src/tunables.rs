//! The daemon's tunables, derived from the effective configuration
//! (`tessera-state`'s schema, with the command-line overrides already
//! applied by the loader; see [`crate::options`]).
//!
//! Each schema key has one apply class. Here they split into:
//!
//! * [`Reloadable`]: apply class `reload`. The state thread derives it again
//!   after every reload and publishes it ([`crate::engine_thread::Settings`]);
//!   new control connections, new audio clients, the persistence debouncer,
//!   the rate limiters and the shutdown path read the current value.
//! * [`media_config`], [`crate::runtime::RuntimeOpts::of`], the socket mode,
//!   group and audio switch, the control threads: apply class `restart`,
//!   read once at start. A reload that changes them is reported (log, reload
//!   result, status warnings) and takes effect at the next service start.
//! * `[defaults.flow]` (per flow, at its next (re)start) and
//!   `[defaults.bridge]` (per bridge, at its next card error) travel with
//!   the flow and bridge plans of the runtime reconciler.

use std::time::Duration;

use tessera_state::Config;
use tessera_state::config::{AudioSocket, KeepalivePayload, TickUs};
use tessera_state::env::RateLimit;

use crate::audio::AudioConfig;
use crate::control::ControlConfig;
use crate::media;

/// Peers each rate limiter tracks at once (a memory bound, not a key: the
/// least recently seen peer is forgotten and starts with a full bucket).
pub const RATE_MAX_PEERS: usize = 1024;

/// The engine tick for `[engine] tick_us = "auto"` (ADR 0004/0011).
pub const AUTO_TICK_US: u32 = 1_000;

/// The smallest engine tick the worker runs, microseconds (a safety floor
/// below the schema's own minimum).
pub const MIN_TICK_US: u32 = 50;

fn to_usize(v: impl TryInto<usize>) -> usize {
    v.try_into().unwrap_or(usize::MAX)
}

/// Control connection caps (`[control]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionCaps {
    /// `max_connections`.
    pub max_total: usize,
    /// `admin_reserved_connections`.
    pub reserved: usize,
    /// `max_connections_per_uid`.
    pub per_uid: usize,
}

/// `[audio]` limits that apply to new audio clients (and to bridges at their
/// next open).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioLimits {
    /// `max_clients`.
    pub max_clients: usize,
    /// `max_clients_per_uid`.
    pub max_clients_per_uid: usize,
    /// `max_period_frames`.
    pub max_period_frames: u32,
    /// `ring_budget_mib`, bytes.
    pub ring_budget_bytes: u64,
    /// `max_ring_mib`, bytes.
    pub max_ring_bytes: u64,
    /// `reorder_window_us`.
    pub reorder_window_us: u32,
    /// `default_period_frames`.
    pub default_period_frames: u32,
}

impl AudioLimits {
    /// The audio endpoint options with these limits and `rx_margin_us` (from
    /// the media engine: one tick plus `[engine] rx_margin_us`).
    #[must_use]
    pub fn audio_config(&self, rx_margin_us: u32) -> AudioConfig {
        let mut cfg = AudioConfig {
            max_clients: self.max_clients,
            max_clients_per_uid: self.max_clients_per_uid,
            max_period_frames: self.max_period_frames,
            ring_budget_bytes: self.ring_budget_bytes,
            reorder_window_us: self.reorder_window_us,
            default_period_frames: self.default_period_frames,
            rx_margin_us,
            ..AudioConfig::default()
        };
        cfg.limits.max_ring_bytes = self.max_ring_bytes;
        cfg
    }
}

/// Everything of apply class `reload` the daemon itself applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reloadable {
    /// Limits of new control connections.
    pub control: ControlConfig,
    /// Control connection caps.
    pub admission: AdmissionCaps,
    /// Per-uid frame limit of the control socket and per-principal command
    /// limit of the state engine.
    pub rate: RateLimit,
    /// Limits of new audio clients.
    pub audio: AudioLimits,
    /// `[daemon] persist_delay_ms`.
    pub persist_delay: Duration,
    /// `[daemon] shutdown_timeout_secs`.
    pub shutdown_timeout: Duration,
}

impl Reloadable {
    /// Derive from the effective configuration.
    #[must_use]
    pub fn of(cfg: &Config) -> Self {
        let c = &cfg.control;
        let a = &cfg.audio;
        Self {
            control: ControlConfig {
                max_frame: to_usize(c.max_frame_bytes),
                event_queue: to_usize(c.event_queue),
                frame_timeout: Duration::from_secs(c.frame_timeout_secs),
                idle_timeout: Duration::from_secs(c.idle_timeout_secs),
                write_timeout: Duration::from_secs(c.write_timeout_secs),
                stream_write_timeout: Duration::from_secs(c.stream_write_timeout_secs),
            },
            admission: AdmissionCaps {
                max_total: to_usize(c.max_connections),
                reserved: to_usize(c.admin_reserved_connections),
                per_uid: to_usize(c.max_connections_per_uid),
            },
            rate: RateLimit {
                burst: c.rate_burst,
                per_second: c.rate_per_second,
                max_peers: RATE_MAX_PEERS,
            },
            audio: AudioLimits {
                max_clients: to_usize(a.max_clients),
                max_clients_per_uid: to_usize(a.max_clients_per_uid),
                max_period_frames: a.max_period_frames,
                ring_budget_bytes: a.ring_budget_mib.saturating_mul(1 << 20),
                max_ring_bytes: a.max_ring_mib.saturating_mul(1 << 20),
                reorder_window_us: a.reorder_window_us,
                default_period_frames: a.default_period_frames,
            },
            persist_delay: Duration::from_millis(cfg.daemon.persist_delay_ms),
            shutdown_timeout: Duration::from_secs(cfg.daemon.shutdown_timeout_secs),
        }
    }
}

/// The engine tick, microseconds (`[engine] tick_us`, `auto` = 1000).
#[must_use]
pub fn tick_us(cfg: &Config) -> u32 {
    match cfg.engine.tick_us {
        TickUs::Auto => AUTO_TICK_US,
        TickUs::Micros(n) => n,
    }
}

/// The keepalive of the media engine for a resolved flow keepalive.
#[must_use]
pub fn keepalive(k: &tessera_state::config::Keepalive) -> media::keepalive::KeepaliveConfig {
    media::keepalive::KeepaliveConfig {
        interval_ns: u64::from(k.interval_ms).saturating_mul(1_000_000),
        timeout_ns: u64::from(k.timeout_ms).saturating_mul(1_000_000),
        payload: match k.payload {
            KeepalivePayload::Empty => media::keepalive::KeepalivePayload::Empty,
            KeepalivePayload::Bytes1337 => media::keepalive::KeepalivePayload::Reference,
        },
    }
}

/// Media engine settings (apply class `restart`: they size real-time
/// structures). The keepalive here is the `[defaults.flow]` one; each flow
/// carries its own resolved keepalive in its plan.
#[must_use]
pub fn media_config(cfg: &Config) -> media::MediaConfig {
    let rt = cfg.daemon.realtime;
    let e = &cfg.engine;
    let worker = media::worker::WorkerConfig {
        tick_ns: u64::from(tick_us(cfg).max(MIN_TICK_US)) * 1_000,
        rx_max_batches: to_usize(e.rx_max_batches).max(1),
        tx_max_per_tick: to_usize(e.tx_max_per_tick).max(1),
        free_running_tx: cfg.clock.free_running_tx,
        ..media::worker::WorkerConfig::default()
    };
    let d = &cfg.defaults.flow;
    media::MediaConfig {
        rt_priority: (rt && e.rt_priority > 0).then_some(e.rt_priority),
        cpu: e.cpu_affinity.first().map(|c| to_usize(*c)),
        mlock: rt && e.mlock,
        worker,
        tuning: media::plan::Tuning {
            rx_batch: to_usize(e.rx_batch).max(1),
            tx_batch: to_usize(e.tx_batch).max(1),
            socket_buffer: to_usize(e.socket_buffer_kib).saturating_mul(1024),
            keepalive: keepalive(&tessera_state::config::Keepalive {
                interval_ms: d.rx_keepalive_ms,
                timeout_ms: d.rx_keepalive_timeout_ms,
                payload: d.rx_keepalive_payload,
            }),
            ..media::plan::Tuning::default()
        },
        rx_margin_ns: u64::from(e.rx_margin_us).saturating_mul(1_000),
        ..media::MediaConfig::default()
    }
}

/// Whether to listen on `audio.sock` (`[audio] socket`; `auto`: when
/// `[backends] enabled` names a client backend).
#[must_use]
pub fn want_audio_socket(cfg: &Config) -> bool {
    match cfg.audio.socket {
        AudioSocket::On => true,
        AudioSocket::Off => false,
        AudioSocket::Auto => !cfg.backends.enabled.is_empty(),
    }
}

/// Worker threads of the control runtime (`[control] threads`).
#[must_use]
pub fn control_threads(cfg: &Config) -> usize {
    to_usize(cfg.control.threads).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimeOpts;

    /// The key regression test: with no configuration file and no
    /// command-line option, every value the daemon derives equals the
    /// default it had when these were command-line options (the
    /// `default_value_t` of each option in `options.rs` before PR 3 and the
    /// way `daemon.rs` turned them into the runtime structs), field by field.
    #[test]
    fn with_no_file_and_no_option_every_value_is_the_former_command_line_default() {
        let opts = crate::Options::default();
        let ov = opts.overrides().unwrap();
        assert!(ov.is_empty());
        let loaded = tessera_state::load_from_sources_with(
            &[],
            &tessera_state::Environment::new("test-host"),
            &ov,
        )
        .unwrap();
        let cfg = loaded.config;
        assert!(loaded.overridden.is_empty());

        let r = Reloadable::of(&cfg);
        // --max-frame-bytes 1<<20, --event-queue 256, --frame-timeout-secs 10,
        // --idle-timeout-secs 60, --write-timeout-secs 5,
        // --stream-write-timeout-secs 30
        assert_eq!(r.control.max_frame, 1 << 20);
        assert_eq!(r.control.event_queue, 256);
        assert_eq!(r.control.frame_timeout, Duration::from_secs(10));
        assert_eq!(r.control.idle_timeout, Duration::from_secs(60));
        assert_eq!(r.control.write_timeout, Duration::from_secs(5));
        assert_eq!(r.control.stream_write_timeout, Duration::from_secs(30));
        assert_eq!(r.control, ControlConfig::default());
        // --max-connections 256, --admin-reserved-connections 16,
        // --max-connections-per-uid 16
        assert_eq!(
            r.admission,
            AdmissionCaps {
                max_total: 256,
                reserved: 16,
                per_uid: 16
            }
        );
        // --rate-burst 256, --rate-per-second 128 (1024 peers, as before)
        assert_eq!(
            r.rate,
            RateLimit {
                burst: 256,
                per_second: 128,
                max_peers: 1024
            }
        );
        // --persist-delay-ms 500, --shutdown-timeout-secs 20
        assert_eq!(r.persist_delay, Duration::from_millis(500));
        assert_eq!(r.shutdown_timeout, Duration::from_secs(20));

        // --max-audio-clients 64, --max-audio-clients-per-uid 8,
        // --audio-max-period-frames 16384, --audio-ring-budget-mib 256,
        // --audio-max-ring-mib 64, --audio-reorder-window-us 10000,
        // --audio-default-period-frames 1024
        let a = r.audio.audio_config(1_250);
        assert_eq!(a.max_clients, 64);
        assert_eq!(a.max_clients_per_uid, 8);
        assert_eq!(a.max_period_frames, 16_384);
        assert_eq!(a.ring_budget_bytes, 256 << 20);
        assert_eq!(a.limits.max_ring_bytes, 64 << 20);
        assert_eq!(a.reorder_window_us, 10_000);
        assert_eq!(a.default_period_frames, 1024);
        assert_eq!(a.rx_margin_us, 1_250);
        let d = AudioConfig::default();
        assert_eq!(a.handshake_timeout, d.handshake_timeout);
        assert_eq!(a.send_timeout, d.send_timeout);
        assert_eq!(a.limits, d.limits);

        // --tick-us unset ([engine] tick_us auto = 1000), --rx-batch 32,
        // --rx-max-batches 8, --tx-batch 32, --tx-max-per-tick 32,
        // --socket-buffer-kib 2048, --rx-margin-us 250, --rx-keepalive-ms 250,
        // --rx-keepalive-timeout-ms 10000, --rx-keepalive-payload empty,
        // --no-rt unset ([engine] rt_priority 80, mlock true)
        let m = media_config(&cfg);
        assert_eq!(m.worker.tick_ns, 1_000_000);
        assert_eq!(m.worker.rx_max_batches, 8);
        assert_eq!(m.worker.tx_max_per_tick, 32);
        assert!(!m.worker.free_running_tx);
        assert_eq!(m.tuning.rx_batch, 32);
        assert_eq!(m.tuning.tx_batch, 32);
        assert_eq!(m.tuning.socket_buffer, 2048 * 1024);
        assert_eq!(m.tuning.keepalive.interval_ns, 250_000_000);
        assert_eq!(m.tuning.keepalive.timeout_ns, 10_000_000_000);
        assert_eq!(
            m.tuning.keepalive.payload,
            media::keepalive::KeepalivePayload::Empty
        );
        assert_eq!(m.rx_margin_ns, 250_000);
        assert_eq!(m.rt_priority, Some(80));
        assert_eq!(m.cpu, None);
        assert!(m.mlock);
        let wd = media::worker::WorkerConfig::default();
        assert_eq!(
            (
                wd.idle_ns,
                wd.clock_stale_ns,
                wd.rx_stall_ns,
                wd.max_span_frames
            ),
            (
                m.worker.idle_ns,
                m.worker.clock_stale_ns,
                m.worker.rx_stall_ns,
                m.worker.max_span_frames
            )
        );
        assert_eq!(
            m.tuning.chunk_frames,
            media::plan::Tuning::default().chunk_frames
        );

        // --no-ptp, --no-rt, --no-discovery unset; [engine] ptp_rt_priority 70.
        let o = RuntimeOpts::of(&cfg);
        assert!(!o.no_ptp && !o.no_rt && !o.no_discovery);
        assert_eq!(o.ptp_rt_priority, Some(70));
        assert_eq!(o.cpu, None);

        // --control-threads 2, --socket-mode 660, --socket-group unset,
        // --audio-socket auto (on: the default backends are enabled),
        // --log-level unset (info).
        assert_eq!(control_threads(&cfg), 2);
        assert_eq!(cfg.control.socket_mode, 0o660);
        assert_eq!(cfg.control.socket_group, None);
        assert_eq!(cfg.audio.socket, AudioSocket::Auto);
        assert!(want_audio_socket(&cfg));
        assert_eq!(cfg.daemon.log_level, "info");
    }

    #[test]
    fn command_line_options_reach_the_derived_values() {
        let opts = <crate::Options as clap::Parser>::parse_from([
            "tesserad",
            "--max-frame-bytes",
            "131072",
            "--no-rt",
            "--tick-us",
            "500",
            "--rx-keepalive-ms",
            "100",
            "--audio-ring-budget-mib",
            "32",
            "--audio-max-ring-mib",
            "16",
        ]);
        let cfg = tessera_state::load_from_sources_with(
            &[],
            &tessera_state::Environment::new("test-host"),
            &opts.overrides().unwrap(),
        )
        .unwrap()
        .config;
        let r = Reloadable::of(&cfg);
        assert_eq!(r.control.max_frame, 131_072);
        assert_eq!(r.audio.ring_budget_bytes, 32 << 20);
        let m = media_config(&cfg);
        assert_eq!(m.worker.tick_ns, 500_000);
        assert_eq!(m.rt_priority, None);
        assert!(!m.mlock);
        assert_eq!(m.tuning.keepalive.interval_ns, 100_000_000);
        assert!(RuntimeOpts::of(&cfg).no_rt);
        assert_eq!(RuntimeOpts::of(&cfg).ptp_rt_priority, None);
    }
}
