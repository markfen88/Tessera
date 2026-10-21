//! PTPv1 (IEEE 1588-2002) follower and leader, best-master selection, a PI
//! servo and the media-clock model, as a sans-IO state machine (ADR 0005).
//!
//! The crate is `no_std` without a heap: the core takes `(bytes, rx
//! timestamp, now)` in and emits [`port::Action`]s (bytes to send with an
//! optional TX-timestamp request, a wake-up deadline, media-clock updates).
//! Nothing in the core does I/O, reads a clock, or allocates, so it is
//! deterministic under a virtual clock and its steady-state path is
//! provably allocation-free (`tests/rt_alloc.rs`).
//!
//! The (non-default) `linux` feature adds [`platform`]: UDP sockets with
//! `SO_TIMESTAMPING`, a runner thread and an adapter that publishes the media
//! clock through the `tessera-audio-ipc` clock overlay. It never writes any
//! kernel clock (project rule 8).
//!
//! # Provisional values
//!
//! Dante's real PTPv1 values (subdomain, strata, intervals, one- vs two-step,
//! election rules) are UNKNOWN until lab captures exist. Every such value is
//! a field of [`config::PortConfig`] with its default in [`provisional`]; see
//! `PROGRESS.md` "Open spec questions".
#![no_std]
#![warn(missing_docs)]

pub mod action;
pub mod bmc;
pub mod command;
pub mod config;
pub mod media_clock;
pub mod port;
pub mod provisional;
pub mod servo;
pub mod time;
pub mod wire;

#[cfg(any(feature = "std", test))]
extern crate std;

#[cfg(all(feature = "linux", target_os = "linux"))]
pub mod platform;
