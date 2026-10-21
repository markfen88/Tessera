//! Config schema, state model, command handling, persistence and event bus
//! (ADR 0008).
//!
//! # Overview
//!
//! * [`loader`] reads `/etc/tessera/config.toml` and `config.d/*.toml`
//!   (lexical order; tables merge, scalars and arrays replace, `[[device]]`
//!   entries merge by `id`, `[[bridge]]` replaces), tracks which
//!   file set which key ([`loader::Provenance`]), rejects unknown keys and
//!   reports *all* type and validation errors with key paths
//!   ([`validate`]). The typed result is [`config::Config`].
//! * [`persist`] owns `state.toml`: operator intent changed at runtime and
//!   the generated device identities. Atomic writes, a schema version with a
//!   migration hook, recovery from missing/corrupt files, and a sans-IO
//!   [`persist::Debouncer`].
//! * [`model::reconcile`] combines the two following ADR 0008 precedence:
//!   a persisted value is used if it is present, valid, within the configured
//!   bounds and not locked; otherwise the configured default is used and a
//!   warning is produced.
//! * [`engine::StateEngine`] applies [`tessera_control_types::Command`]s
//!   (validate, apply on a copy, commit, mark for persistence, publish
//!   events), answers queries, and owns the [`bus::EventBus`].
//!
//! No tokio: everything is synchronous and runtime-agnostic. `tesserad` runs
//! one `StateEngine` inside its state task; [`bus::Subscription::recv_async`]
//! is a plain `Future`.
//!
//! # Choices where the specification is silent
//!
//! These are deliberate, simple behaviours; each can be changed without
//! touching the public command model.
//!
//! * **Unknown keys** are errors everywhere in the configuration, and the
//!   error lists the valid keys of that table. In `state.toml` unknown keys
//!   are ignored.
//! * **Empty or missing configuration** is valid: one device `main`, named
//!   after the host (shortened to the name limit at a character boundary),
//!   2x2 channels, 48 kHz, 24 bit, platform-default latency. With several
//!   devices and no explicit names, names are `<host>-<id>`.
//! * **Name rules** (devices and channel labels): non-empty, at most 31
//!   bytes (configurable in [`env::Environment`]), no control characters
//!   (wire strings are NUL-terminated).
//! * **Device names** must be unique, ignoring ASCII case.
//! * **Schema and reference.** [`schema`] lists every key with its default,
//!   unit, range, apply class (`live`/`reload`/`restart`) and restart scope;
//!   [`reference_config`] renders the commented reference file from it
//!   (what `tesserad --print-default-config` prints).
//! * **Overrides.** [`Overrides`] carry command-line values over every file
//!   (precedence: command line > `locked` > `state.toml` > `config.d` >
//!   `config.toml` > default); reloads re-apply them.
//! * **Confirmation.** A command whose computed [`tessera_control_types::Impact`]
//!   restarts something is refused for CLI and API origins with
//!   `ConfirmationRequired` unless it comes through
//!   [`StateEngine::apply_confirmed`] ([`impact`] has the scope table).
//! * **Channel counts** set at run time (`SetDeviceChannels`) are persisted
//!   like the other device values; subscriptions on removed RX channels are
//!   deleted (not kept inactive), labels are kept inactive.
//! * **Locks.** `locked` entries are `name`, `sample_rate`, `encoding`,
//!   `latency`, `rx_channel_labels`, `tx_channel_labels`, `rx_channels`,
//!   `tx_channels` (per device) and
//!   `role`, `priority` (a `locked` list in `[clock]`). Unknown entries are
//!   errors. A locked key is cleared from `state.toml`, so after unlocking the
//!   old state value is gone and the configured one applies until something
//!   changes it. Subscriptions cannot be locked.
//! * **`allow_unsafe_latency`** is a per-device boolean; the platform latency
//!   floor is a value in [`env::Environment`], never a constant in this crate.
//! * **Clock role and priority** are persisted in `[clock]` of `state.toml`
//!   like other runtime changes.
//! * **Device identity** (8 bytes) comes from [`env::DeviceIdSource`] at the
//!   first start and is persisted as `wire_device_id`; there is no command to
//!   change it.
//! * **Kept but inactive.** A persisted value that fails a check (a name that
//!   clashes with another device, an unsupported rate, a latency outside the
//!   current bounds, a label beyond the channel count, a subscription with an
//!   unusable name) is left in `state.toml` untouched, left out of the
//!   effective model, and reported as a warning, so a temporary clash cannot
//!   erase what was set at runtime. Only keys pinned by `locked` are cleared,
//!   and repeated entries for one RX channel collapse (reported). An explicit
//!   command for the same key overwrites the stored value.
//! * **Renames** are rejected (`Invalid`) when the name is already used by
//!   another device's effective name, configured name, or *stored* name,
//!   including inactive stored names and those of devices that are currently
//!   not in the configuration; otherwise an owner coming back would silently
//!   lose its name.
//! * **Label keys** in `state.toml` are normalised on load (`"03"` and `"+3"`
//!   become `"3"`, with a warning); if several keys name one channel the
//!   canonical one wins, else the last, and the loss is reported. Setting a
//!   label compares with the stored value, so an inactive stored value is
//!   replaced.
//! * **Partly wrong `state.toml`.** If the file as a whole does not fit the
//!   schema, it is moved aside as `state.toml.corrupt-N`, but each device
//!   section that still parses is kept, and for the others at least
//!   `wire_device_id`, so network identities survive.
//! * **`daemon.state_dir` on reload** is rejected (restart required) unless
//!   the caller gave an explicit state-file path.
//! * **Blocking I/O.** `start`, `poll_persist`, `flush` and reload do
//!   synchronous file I/O; run them off any async executor thread.
//! * **Duplicate stored subscriptions** for one RX channel *are* dropped (the
//!   last wins) with a warning; every other unusable subscription is kept.
//! * **Subscriptions** that do not fit the channel count (after a config
//!   change) stay in `state.toml` with a warning and are inactive; they are
//!   never dropped. The `Subscribe` command rejects out-of-range channels and
//!   remote names that fail the name rules.
//!   `Unsubscribe` of a channel without a subscription succeeds as a no-op.
//! * **Rate limiting** (ADR 0010) applies to commands from `Origin::Protocol`
//!   (per peer), `Origin::Cli` (per uid) and `Origin::Api` (per principal): a
//!   token bucket per principal, refused commands get
//!   `CommandError::RateLimited`. Reloads and `Origin::Internal` are not
//!   limited.
//! * **Debouncing** opens a window at the first unsaved change; later changes
//!   join it without extending it (so writes are never postponed forever).
//! * **Events** are published after the in-memory state changed, not after the
//!   debounced write. Reload events carry `Origin::Config`. A change of the
//!   clock role or priority (command or reload) publishes
//!   `Event::ClockSettingsChanged`.
//! * **Reload** applies everything that can change in place and reports the
//!   rest as [`engine::Effect`]s (device restarts, changed sections).
//!   `ConfigReloaded` is published last.
//! * **Runtime observations** (`report_subscription_state`,
//!   `report_clock_status`) are not commands and are never persisted.
//! * Semantic validation runs after type errors too, so that everything is
//!   reported in one pass; only the checks that read a value that failed to
//!   parse (the key itself, its `[[...]]` entry, keys that depend on it)
//!   are left out, to avoid consequential messages.
//! * **Zero channels.** A device with `rx_channels = 0` or `tx_channels =
//!   0` is valid in the files and can change its other direction at run
//!   time. A direction that has channels cannot be taken to 0 by
//!   `SetDeviceChannels` or by a reload of a running device until
//!   `tesserad` has a test for a device restarted into that shape
//!   (CFG-ZERO); starting the daemon with 0 is allowed.

pub mod bus;
pub mod config;
pub mod engine;
pub mod env;
pub mod error;
pub mod impact;
pub mod loader;
pub mod model;
pub mod network_profiles;
pub mod persist;
pub mod schema;
pub mod validate;

pub use bus::{EventBus, Received, RecvError, Subscription};
pub use config::{Config, DeviceConfig, FlowConfig, FlowFormat};
pub use engine::{Applied, Effect, Paths, RestartReason, StartupReport, StateEngine};
pub use env::{Clock, Environment, ManualClock, MonotonicClock};
pub use error::{ConfigError, Issue, Severity, StartError, StateError};
pub use loader::{
    ConfigPaths, LoadedConfig, Overrides, Provenance, Source, load_config, load_config_with,
    load_from_sources, load_from_sources_with,
};
pub use model::{DeviceModel, Model, WireDeviceId};
pub use persist::{Debouncer, StateFile, StateLoad};
pub use schema::{ApplyClass, KeySpec, reference_config};
