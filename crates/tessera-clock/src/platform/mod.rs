//! Linux platform layer: sockets, `SO_TIMESTAMPING`, clock conversion and
//! the runner thread (ADR 0004, ADR 0005).
//!
//! This is the only module of the crate that uses `unsafe` (ADR 0002): raw
//! `recvmsg` with a pre-allocated control buffer (so the receive loop never
//! allocates), socket and ethtool `ioctl`s, `sched_setscheduler`. Each block
//! carries a `// SAFETY:` comment.
//!
//! # Embedding (what `tesserad` does)
//!
//! ```ignore
//! let writer = ClockOverlayWriter::create()?;             // tessera-audio-ipc
//! let runner = Runner::new(RunnerConfig::new("eth0", addr, port_cfg),
//!                          OverlayPublisher::new(writer))?;
//! let mut ptp = runner.spawn()?;                           // RunnerHandle
//!
//! // Live control: no restart, no rebind, overlay and epoch untouched.
//! ptp.send(RunnerCommand::SetRole(Role::Follower))?;
//! ptp.send(RunnerCommand::SetLocalClock(LocalClock { stratum: 2, ..LocalClock::leader_capable() }))?;
//! ptp.send(RunnerCommand::SetPublishGuard { threshold_ns: 1_000_000, hold_ns: 0 })?;
//! ptp.send(RunnerCommand::SetTimestamping(TimestampMode::Software))?;
//!
//! let status = ptp.status();        // Arc<SharedStatus>: atomics, role/stratum/
//!                                   // guard mirrored, commands_applied/rejected
//! while let Some(ev) = ptp.pop_event() { /* Copy events: state, parent, lock */ }
//!
//! let last = ptp.stop();            // graceful: Option<RunnerFinal>
//! ```
//!
//! Commands travel through a lock-free SPSC ring (64 entries) and are applied
//! by the PTP thread within one wake-up; applying one never allocates or
//! locks. A role or priority change re-runs the election through the state
//! machine: media time is never stepped by the command (a leader demotion
//! keeps its mapping and runs it at the frequency it had learned; a promotion
//! leads from the current mapping), and the overlay `epoch` changes only if
//! the servo later has to step (ADR 0005). `stop()` publishes one final
//! overlay state (`FreeRunning`, mapping and epoch unchanged) before the
//! thread exits, so clients never keep seeing a stale `Locked` or `Leader`.
//!
//! Timestamping can change live without a rebind: software and hardware
//! stamping are switched on the existing event socket (`SO_TIMESTAMPING`) and
//! the NIC filter is widened, never narrowed. A switch to `Hardware` on an
//! interface that cannot stamp PTPv1 is rejected (`commands_rejected`) and
//! the current mode stays. Changing the interface or address needs a new
//! runner (new sockets); do that by creating the new `Runner` with a
//! publisher over the same overlay writer, then stopping the old one.
//!
//! Rules kept here:
//!
//! * No kernel clock is ever written: no call that sets or slews any clock
//!   is made anywhere in this crate (project rule 8). `CLOCK_REALTIME`/`CLOCK_TAI` are only
//!   *read*: to convert software timestamps (which the kernel takes from
//!   `CLOCK_REALTIME`) into the monotonic domain the core works in, and to
//!   give a leader a plausible starting time.
//! * The runner loop does not allocate, lock or log; it publishes counters
//!   through atomics ([`status::SharedStatus`]) and `Copy` events through an
//!   SPSC ring ([`runner::RunnerEvent`]).
//! * Hardware timestamping is detected at runtime
//!   ([`tsinfo::query_ts_caps`]) and falls back to software. The hardware
//!   path could only be exercised against kernel interfaces here, not a real
//!   PHC (the build VM has none): treat it as untested.
#![allow(unsafe_code)]

pub mod clockconv;
pub mod overlay;
pub mod publisher;
pub mod runner;
pub mod socket;
pub mod status;
pub mod tsinfo;
mod txmatch;

pub use clockconv::{mono_ns, raw_ns, realtime_ns, tai_ns};
pub use overlay::OverlaySnapshot;
pub use publisher::OverlayPublisher;
pub use runner::{
    CommandError, Runner, RunnerCommand, RunnerConfig, RunnerEvent, RunnerFinal, RunnerHandle,
    TimestampMode,
};
pub use status::SharedStatus;
