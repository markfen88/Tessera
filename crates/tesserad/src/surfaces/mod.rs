//! Hardware control surfaces in the daemon (plan
//! `docs/plans/2026-11-21-hardware-control-surface.md` §2.4).
//!
//! | Module | Does |
//! |---|---|
//! | [`discovery`] | what the host has: `hidraw` interfaces, sound cards and their rawmidi ports, from sysfs, procfs and `/dev/snd` |
//! | [`reader`] | reads a device node (or, lab only, a scripted file) on its own thread, stoppable on a quiet device; writes to a MIDI port |
//! | [`runtime`] | one task per enabled surface: decode, map, submit typed commands, feed a desk back; live state; learning |
//! | [`compat`] | the calls into `tessera-surface` whose API changes on a parallel branch |
//!
//! A surface never writes gain or mute itself: each button press or fader
//! move becomes a typed command (`SetChannelMute`, `SetChannelGain`) applied
//! through the state engine as `Origin::Surface { name }`, and the media path
//! follows the engine's events (`media::gains`). The portal, the CLI and the
//! surfaces share that one authority.

pub mod compat;
pub mod discovery;
pub mod reader;
pub mod runtime;

pub use runtime::{Surfaces, run};
