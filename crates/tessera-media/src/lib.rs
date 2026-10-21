//! Media-time-addressed buffering and flow scheduling for tessera.
//!
//! This is the sans-IO media core of the daemon (ADR 0002/0004/0006): it takes
//! bytes and timestamps in and returns bytes and actions out. It owns no
//! sockets, threads, timers or clocks.
//!
//! | Module | Purpose | Needs |
//! |---|---|---|
//! | [`time`] | [`MediaClock`] trait, sample/ns conversions | core |
//! | [`convert`] | 16/24/32-bit integer and f32 sample conversion, optional dither | core |
//! | [`packet`] | [`PacketFormat`] trait (header + payload codec abstraction) | core |
//! | [`mod@reference`] | PROVISIONAL reference format for tests | core |
//! | [`counter`] | Atomic metric counters | core |
//! | [`drift`] | [`Resampler`] trait, PI controller, rate estimator | core |
//! | `buffer` | Media-time-addressed rings (`MediaBuffer`) | `alloc` |
//! | `rx` | RX flow engine (`RxFlow`) | `alloc` |
//! | `tx` | TX flow engine (`TxFlow`) | `alloc` |
//! | `resample` | Linear resampler, quality factory | `alloc` |
//!
//! # Design rules honoured here
//!
//! * Positions are `u64` media-sample counts. Unwritten positions read as
//!   silence; stale audio is never replayed; there is no adaptive jitter
//!   buffer (ADR 0006).
//! * The hot paths (`RxFlow::ingest`, `RxFlow::read`, `TxFlow::poll`,
//!   conversion, `Resampler::process`) never allocate, lock, make syscalls
//!   or log; all buffers are sized at construction. The integration test
//!   `tests/alloc.rs` (and `tests/rubato.rs` for the `rubato` adapter) proves
//!   this with the testkit counting allocator.
//! * Metrics are relaxed atomics shared through `Arc`; events are the
//!   caller's business.
//! * All wire-facing values are provisional (`spec/protocol/flows-media.md`
//!   is `likely`/`guess`): the packet layout lives behind [`PacketFormat`],
//!   and numeric defaults are configuration, not constants.
//!
//! # Features
//!
//! * `std` (default) implies `alloc`.
//! * `alloc`: buffers, flow engines, linear resampler.
//!
//! The `rubato` adapter (cubic and sinc quality levels) lives in the separate
//! `tessera-resample` crate, because rubato needs Rust 1.87 and this crate
//! stays at the workspace MSRV of 1.85 and builds for `thumbv8m`.

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(missing_docs)]

#[cfg(feature = "alloc")]
extern crate alloc;

pub mod convert;
pub mod counter;
pub mod drift;
pub mod error;
pub mod packet;
pub mod reference;
pub mod time;

#[cfg(feature = "alloc")]
pub mod buffer;
#[cfg(feature = "alloc")]
pub mod resample;
#[cfg(feature = "alloc")]
pub mod rx;
#[cfg(feature = "alloc")]
pub mod tx;

#[cfg(feature = "alloc")]
pub use buffer::{FrameSource, MIN_CAPACITY, MediaBuffer, ReadStats, WriteStats};
pub use convert::{Dither, Endian, SampleFormat, WireFormat};
pub use counter::Counter;
pub use drift::{
    DriftCompensator, PiConfig, PiController, ProcessResult, Quality, RateEstimator, Resampler,
};
pub use error::{ConfigError, ConvertError, DriftConfigError, FormatError, ResampleError, TxError};
pub use packet::{Decoded, PacketFormat};
#[cfg(feature = "alloc")]
pub use rx::{IngestOutcome, RxConfig, RxFlow, RxMetrics, RxSnapshot};
pub use time::{ClockSnapshot, MediaClock};
#[cfg(feature = "alloc")]
pub use tx::{TxConfig, TxFlow, TxMetrics, TxPacket, TxPoll, TxSnapshot};
