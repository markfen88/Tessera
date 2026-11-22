//! Hardware control surfaces for tessera.
//!
//! A control surface is a physical device with buttons and faders (a USB
//! streaming mixer, a MIDI controller, a keyboard's media keys) whose
//! controls drive channel **mute** and **gain** of a tessera device, the
//! host volume, router port levels and crosspoint gains, or (`follow`) a
//! control of another surface.
//!
//! This crate is sans-IO. It owns no file descriptors, threads or clocks:
//!
//! | Module | Purpose |
//! |---|---|
//! | [`config`] | `[[surface]]` schema: the controls a device has and what each is bound to |
//! | [`decode`] | Raw input report bytes → [`SurfaceEvent`]s, driven by the config's field layout |
//! | [`mapper`] | [`SurfaceEvent`]s → [`SurfaceAction`]s (toggle / momentary / push-to-talk, fader taper, deadband, fader priority, motorized faders) |
//! | [`learn`] | Raw HID reports while the user moves one control → its suggested layout |
//! | [`midi`] | MIDI byte parser, NRPN assembly, [`MidiDecoder`] (MIDI → [`SurfaceEvent`]s), [`MidiLearner`], encoders for feedback to the device |
//! | [`profile`] | Shareable device descriptions (TOML or JSON): controls, fader laws, suggested bindings |
//! | [`presets`] | Built-in profiles (Allen & Heath CQ) |
//!
//! The daemon side (not in this crate) reads the device (hidraw, evdev, ALSA
//! mixer events or MIDI), feeds the bytes or events through [`decode`] and
//! [`mapper`], and translates each [`SurfaceAction`] into a typed command
//! submitted to the state engine. A surface never writes gain or mute state
//! itself: every change goes through the command model, so the portal, the
//! CLI and the surface all see one authority.
//!
//! RX/TX are relative to the tessera device, as everywhere else: an `rx`
//! target is network → host (what the mixer plays), a `tx` target is
//! host → network (what the mixer captures).

pub mod config;
pub mod decode;
pub mod learn;
pub mod mapper;
pub mod midi;
pub mod presets;
pub mod profile;

pub use config::{
    BindingAction, BindingConfig, ButtonMode, ControlConfig, ControlKind, SourceConfig,
    SurfaceConfig, Taper, Target, check_curve,
};
pub use decode::{Decoder, SurfaceEvent, SurfaceEventKind};
pub use learn::Learner;
pub use mapper::{Mapper, SurfaceAction, fader_db, fader_position};
pub use midi::{MidiDecoder, MidiLearner, MidiMessage, MidiParser, NrpnAssembler};
pub use profile::{ProfileError, SurfaceProfile, load_dir};
