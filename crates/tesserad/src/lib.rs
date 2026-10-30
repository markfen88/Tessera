//! The `tesserad` daemon (ADR 0002, ADR 0004).
//!
//! Threads of the process:
//!
//! | Thread | Module | What it does |
//! |---|---|---|
//! | control (tokio, small pool) | [`control`], [`audio`], [`daemon`], [`runtime`], [`disco`], [`subscriptions`] | sockets, supervision, the runtime reconciler, discovery, the live state of RX subscriptions |
//! | state engine (plain thread, blocking file I/O) | [`engine_thread`] | configuration, commands, persistence, events |
//! | clock manager | [`ptp`] | supervises one PTP runner thread per interface (`tessera-clock`, `SCHED_FIFO`) and publishes the media clock overlay |
//! | media control | [`media`] | builds flows and sessions, hands them to the worker, frees what it returns |
//! | media worker (real time) | [`media::worker`] | the tick loop: `recvmmsg` / `sendmmsg`, flow engines, client rings, wake bytes |
//! | bridge control (feature `alsa-bridge`, only with `[[bridge]]`) | [`bridge`] | opens cards, retries, applies plans, reports bridge status |
//! | bridge (real time, one per running `[[bridge]]`) | `bridge::rt_loop` | card I/O, resampler, PI loop |
//!
//! Static `[[flow]]` entries are **PROVISIONAL** test scaffolding until ARC/CMC
//! flow control exists (see `docs/configuration.md`).
//!
//! The library exists so the integration tests and the binary share code;
//! it is not a public API.
#![warn(missing_docs)]

pub mod admission;
pub mod audio;
pub mod bridge;
pub mod control;
pub mod daemon;
pub mod disco;
pub mod engine_thread;
pub mod frame;
pub mod logging;
pub mod media;
pub mod metrics;
pub(crate) mod native;
pub mod netif;
pub mod notify;
pub mod options;
pub mod peer;
pub mod peerlog;
pub mod platform;
pub mod ptp;
pub mod ratelimit;
mod routing_inventory;
mod routing_operation;
mod routing_receiver;
pub mod rt;
pub mod rtsys;
pub mod runtime;
mod runtime_host;
pub mod sanitize;
pub mod sockets;
pub mod subscriptions;
pub mod supervisor;
pub mod tunables;

pub use daemon::run;
pub use options::Options;
