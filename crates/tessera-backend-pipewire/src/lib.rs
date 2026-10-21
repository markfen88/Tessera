//! PipeWire backend of tessera (ADR 0007): the `tessera-pw` user-session
//! client of `tesserad`.
//!
//! The library holds everything that does not need PipeWire headers and is
//! therefore built and tested on every target:
//!
//! * [`config`]: command line and defaults,
//! * [`device`]: device descriptions from the daemon's control socket and the
//!   mapping of a logical device to PipeWire nodes,
//! * [`session`]: the client side of the `audio.sock` handshake,
//! * [`timing`]: media-clock arithmetic and the graph-cycle scheduler,
//! * [`rt`] and [`follow`]: the real-time engines (driver and follower mode),
//! * [`backoff`]: reconnect back-off.
//!
//! The PipeWire glue (`feature = "pw"`) is in `node` and `supervisor`; the
//! binary is `src/main.rs`.
//!
//! # Real-time rules
//!
//! The process callbacks (`rt::RxEngine`, `rt::TxEngine`) never allocate, lock,
//! make syscalls or log (ADR 0004); `tests/rt_alloc.rs` proves it with a
//! counting allocator. Sessions cross to the real-time side through
//! single-producer/single-consumer rings and are retired the same way.

pub mod alloc_check;
pub mod backoff;
pub mod config;
pub mod device;
pub mod driver;
pub mod error;
pub mod follow;
pub mod rt;
pub mod session;
pub mod sys;
pub mod timing;

#[cfg(feature = "pw")]
pub mod node;
#[cfg(feature = "pw")]
pub mod supervisor;

pub use config::Config;
pub use error::Error;
