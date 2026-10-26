//! ALSA backend of tessera (ADR 0002, 0007).
//!
//! * The **PCM plugin** `pcm.tessera` (feature `plugin`, on by default): an
//!   alsa-lib *ioplug* in this cdylib, installed as
//!   `libasound_module_pcm_tessera.so`. It is a thin ring client of
//!   `tesserad` (see `tessera-audio-ipc`): it connects to `audio.sock`,
//!   attaches a ring and the clock overlay, derives the hardware pointer from
//!   the media clock and copies between the application's buffer and the
//!   ring. See [`settings`] for the PCM configuration schema.
//! * The **hardware-card bridge** (feature `bridge`, off by default): a module
//!   for `tesserad` that bridges a local ALSA card to audio-ipc rings with
//!   drift compensation (ADR 0006). It is not part of the plugin build.
#![warn(missing_docs)]

pub mod client;
pub mod clock;
pub mod convert;
pub mod error;
pub mod settings;
pub mod stream;
mod tick;

#[cfg(feature = "plugin")]
#[doc(hidden)]
pub mod ffi;
#[cfg(feature = "plugin")]
#[doc(hidden)]
pub mod plugin;

#[cfg(feature = "bridge")]
pub mod bridge;

pub use error::Error;
