//! What a command does beyond changing a value: the restart it causes, what
//! is dropped, and the control-socket limits a daemon announces.
//!
//! Every control surface (CLI, web, a future API) shows the same [`Impact`]
//! before a change that restarts something, so the text fields are complete
//! sentences meant for people: what restarts, what is dropped, roughly for
//! how long, and what to do afterwards ([`Impact::describe`] joins them).
//!
//! Semver: these types are additive like the rest of the crate. Every struct
//! and enum is `#[non_exhaustive]`. [`Impact`], [`RestartImpact`],
//! [`RingMemory`] and [`Limits`] deserialise missing fields to their
//! defaults; [`RemovedSubscription`] cannot (a channel has no default), so
//! every field added to it later must carry its own `#[serde(default)]`.
//!
//! **Confirming what was shown.** [`Impact::token`] is a digest of the
//! command and of what the impact changes, restarts and deletes (scope,
//! the from/to values in the reason, devices, flows, subscriptions,
//! restart keys, memory). A client sends it back with the confirmed command
//! (`RequestBody::ConfirmedCommand { expect }`); if the daemon's recomputed
//! impact has another token (the files changed, a subscription was added
//! in between), it answers `ConfirmationRequired` again with the new
//! impact instead of applying something the user did not see.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::{Channel, DeviceKey};

/// How much of the system a change restarts. The variants are not ordered:
/// compare them by name, not by size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum RestartScope {
    /// Applied in place; nothing restarts and nothing is dropped.
    #[default]
    None,
    /// The named logical devices restart: their flows are rebuilt, their
    /// audio clients are detached and must re-attach, and they are
    /// re-announced on the network.
    Device,
    /// The whole daemon restarts (every device, the control socket, the
    /// clock).
    Daemon,
}

impl RestartScope {
    /// The spelling used in JSON and in the documentation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Device => "device",
            Self::Daemon => "daemon",
        }
    }

    /// Does a change with this scope need the user's confirmation?
    #[must_use]
    pub fn needs_confirmation(self) -> bool {
        self != Self::None
    }
}

/// The restart part of an [`Impact`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[non_exhaustive]
pub struct RestartImpact {
    /// How much restarts.
    pub scope: RestartScope,
    /// The devices that restart (for [`RestartScope::Device`]).
    pub devices: Vec<DeviceKey>,
    /// Why, in one sentence ("the sample rate of `main` changes from 48000
    /// to 96000 Hz").
    pub reason: String,
    /// What stops or is lost while it happens, one item per entry.
    pub what_drops: Vec<String>,
    /// Roughly how long the interruption lasts, for people ("a few seconds
    /// of silence on this device's channels").
    pub expected_downtime_hint: String,
    /// What the user has to do afterwards, if anything.
    pub afterwards: String,
}

impl RestartImpact {
    /// Nothing restarts.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// A restart of `scope` for `reason`; add the details with the `with_*`
    /// methods.
    #[must_use]
    pub fn new(scope: RestartScope, reason: impl Into<String>) -> Self {
        Self {
            scope,
            reason: reason.into(),
            ..Self::default()
        }
    }

    /// Set the devices that restart.
    #[must_use]
    pub fn with_devices(mut self, devices: Vec<DeviceKey>) -> Self {
        self.devices = devices;
        self
    }

    /// Set what is dropped.
    #[must_use]
    pub fn with_what_drops(mut self, what_drops: Vec<String>) -> Self {
        self.what_drops = what_drops;
        self
    }

    /// Set the downtime hint.
    #[must_use]
    pub fn with_downtime_hint(mut self, hint: impl Into<String>) -> Self {
        self.expected_downtime_hint = hint.into();
        self
    }

    /// Set what the user must do afterwards.
    #[must_use]
    pub fn with_afterwards(mut self, afterwards: impl Into<String>) -> Self {
        self.afterwards = afterwards.into();
        self
    }
}

/// A subscription a command deletes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct RemovedSubscription {
    /// The local device.
    pub device: DeviceKey,
    /// The local RX channel.
    pub rx_channel: Channel,
    /// The remote TX channel it was subscribed to.
    pub tx_channel: String,
    /// The remote device.
    pub tx_device: String,
}

impl RemovedSubscription {
    /// A removed subscription.
    #[must_use]
    pub fn new(
        device: DeviceKey,
        rx_channel: Channel,
        tx_channel: impl Into<String>,
        tx_device: impl Into<String>,
    ) -> Self {
        Self {
            device,
            rx_channel,
            tx_channel: tx_channel.into(),
            tx_device: tx_device.into(),
        }
    }
}

/// Estimated audio ring memory of a device: one client ring spanning every
/// channel in each direction, before and after the change, and the budget
/// all rings together may use (`[audio] ring_budget_mib`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[non_exhaustive]
pub struct RingMemory {
    /// Bytes now.
    pub before_bytes: u64,
    /// Bytes after the change.
    pub after_bytes: u64,
    /// The budget, bytes.
    pub budget_bytes: u64,
}

impl RingMemory {
    /// An estimate.
    #[must_use]
    pub fn new(before_bytes: u64, after_bytes: u64, budget_bytes: u64) -> Self {
        Self {
            before_bytes,
            after_bytes,
            budget_bytes,
        }
    }
}

/// The digest of an [`Impact`] ([`Impact::token`]): what a user confirmed.
/// Opaque: made by the daemon, sent back unchanged, compared for equality
/// only.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize), serde(transparent))]
pub struct ImpactToken(String);

impl ImpactToken {
    /// A token received from a client (for example a hidden form field).
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The token as text, to hand to a client.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// FNV-1a, 64 bit: small, deterministic on every platform, no dependency.
/// Not a security boundary (the user is authenticated anyway); it only
/// detects that the impact changed.
fn fnv1a(parts: &[&str]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.bytes().chain(core::iter::once(0)) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// What a command would do (dry run, [`crate::CommandError::ConfirmationRequired`])
/// or did ([`crate::Reply::Impact`]).
///
/// The state engine fills what it knows; the daemon may add what only it
/// can see (`tx_flows_removed`, `audio_clients`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[non_exhaustive]
pub struct Impact {
    /// The command's [`crate::Command::kind`].
    pub command: String,
    /// The restart it causes.
    pub restart: RestartImpact,
    /// Static flows (`[[flow]] id`) that restart.
    pub flows_restarted: Vec<String>,
    /// Subscriptions that are deleted.
    pub subscriptions_removed: Vec<RemovedSubscription>,
    /// Transmit flows other devices set up to this one that are torn down
    /// (filled by the daemon; empty from the state engine).
    pub tx_flows_removed: Vec<String>,
    /// The restarting devices' audio clients are detached.
    pub audio_clients_detached: bool,
    /// How many audio clients that is, if the daemon knows.
    pub audio_clients: Option<u32>,
    /// Ring memory estimate, for changes of the channel count.
    pub ring_memory: Option<RingMemory>,
    /// Configuration keys of apply class `restart` (schema paths such as
    /// `engine.rx_batch`) that a reload changes. Information only: nothing
    /// restarts for them now, they take effect at the next service restart,
    /// so they do not raise the scope and need no confirmation.
    pub restart_keys: Vec<String>,
    /// [`Impact::token`] of this impact, filled by the daemon so that
    /// clients in any language can send it back.
    pub token: Option<ImpactToken>,
}

impl Impact {
    /// An impact for `command` with `restart`; fill the rest by field.
    #[must_use]
    pub fn new(command: impl Into<String>, restart: RestartImpact) -> Self {
        Self {
            command: command.into(),
            restart,
            ..Self::default()
        }
    }

    /// Does applying it need the user's confirmation?
    #[must_use]
    pub fn needs_confirmation(&self) -> bool {
        self.restart.scope.needs_confirmation()
    }

    /// The digest of what this impact does: the command, the scope, the
    /// reason (it names every value from and to), the devices, flows,
    /// subscriptions, transmit flows and restart keys (as sets), whether
    /// audio clients are detached, and the memory estimate. The downtime
    /// hint and the advice are not part of it. The token is opaque and
    /// short-lived: it only has to be the same between a dry run and its
    /// confirmation on the same daemon.
    #[must_use]
    pub fn token(&self) -> ImpactToken {
        self.token_with(&[])
    }

    /// [`token`](Self::token) over extra `context` too: what the daemon
    /// knows beyond the impact, such as the command's arguments and, for a
    /// reload, the contents of every changed `[[flow]]`/`[[bridge]]` entry.
    #[must_use]
    pub fn token_with(&self, context: &[&str]) -> ImpactToken {
        let mut devices: Vec<&str> = self.restart.devices.iter().map(|d| d.0.as_str()).collect();
        devices.sort_unstable();
        let mut flows: Vec<&str> = self.flows_restarted.iter().map(String::as_str).collect();
        flows.sort_unstable();
        let mut txf: Vec<&str> = self.tx_flows_removed.iter().map(String::as_str).collect();
        txf.sort_unstable();
        let mut keys: Vec<&str> = self.restart_keys.iter().map(String::as_str).collect();
        keys.sort_unstable();
        let mut subs: Vec<String> = self
            .subscriptions_removed
            .iter()
            .map(|s| {
                format!(
                    "{}\u{1}{}\u{1}{}\u{1}{}",
                    s.device.0, s.rx_channel, s.tx_channel, s.tx_device
                )
            })
            .collect();
        subs.sort_unstable();
        let ring = self.ring_memory.map_or_else(String::new, |m| {
            format!("{}/{}/{}", m.before_bytes, m.after_bytes, m.budget_bytes)
        });
        let mut parts: Vec<&str> = Vec::new();
        parts.push(self.command.as_str());
        parts.push(self.restart.scope.as_str());
        parts.push(self.restart.reason.as_str());
        parts.push(ring.as_str());
        parts.push(if self.audio_clients_detached {
            "detach"
        } else {
            "-"
        });
        for (tag, list) in [("d", &devices), ("f", &flows), ("t", &txf), ("k", &keys)] {
            parts.push(tag);
            parts.extend(list.iter().copied());
        }
        parts.push("s");
        parts.extend(subs.iter().map(String::as_str));
        parts.push("c");
        parts.extend(context.iter().copied());
        ImpactToken(format!("v1-{:016x}", fnv1a(&parts)))
    }

    /// This impact with [`token`](Self::token) filled in.
    #[must_use]
    pub fn sealed(self) -> Self {
        self.sealed_with(&[])
    }

    /// This impact with [`token_with`](Self::token_with) filled in.
    #[must_use]
    pub fn sealed_with(mut self, context: &[&str]) -> Self {
        self.token = Some(self.token_with(context));
        self
    }

    /// The whole impact as text for people, one item per line: what
    /// restarts and why, what is dropped, how long, what to do afterwards.
    /// CLI and web show the same text.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut out: Vec<String> = Vec::new();
        let r = &self.restart;
        match r.scope {
            RestartScope::None => out.push(String::from("Nothing restarts.")),
            RestartScope::Device => {
                let names: Vec<String> = r.devices.iter().map(|d| format!("`{}`", d.0)).collect();
                out.push(format!("Restarts device(s) {}.", names.join(", ")));
            }
            RestartScope::Daemon => out.push(String::from("Restarts the whole daemon.")),
        }
        if !r.reason.is_empty() {
            out.push(format!("Why: {}", r.reason));
        }
        for d in &r.what_drops {
            out.push(format!("Drops: {d}"));
        }
        if !self.flows_restarted.is_empty() {
            out.push(format!(
                "Flows that restart: {}",
                self.flows_restarted.join(", ")
            ));
        }
        for s in &self.subscriptions_removed {
            out.push(format!(
                "Deletes the subscription of `{}` RX {} to {}@{}",
                s.device.0, s.rx_channel, s.tx_channel, s.tx_device
            ));
        }
        if !self.restart_keys.is_empty() {
            out.push(format!(
                "Takes effect at the next service restart: {}",
                self.restart_keys.join(", ")
            ));
        }
        if !self.tx_flows_removed.is_empty() {
            out.push(format!(
                "Transmit flows torn down: {}",
                self.tx_flows_removed.join(", ")
            ));
        }
        if self.audio_clients_detached {
            out.push(match self.audio_clients {
                Some(n) => format!("Detaches {n} audio client(s); they must re-attach."),
                None => String::from(
                    "Detaches the device's audio clients (if any); they must re-attach.",
                ),
            });
        }
        if let Some(m) = self.ring_memory {
            out.push(format!(
                "Audio ring memory (one full-width client per direction): {} -> {} bytes of a {} byte budget.",
                m.before_bytes, m.after_bytes, m.budget_bytes
            ));
        }
        if !r.expected_downtime_hint.is_empty() {
            out.push(format!(
                "Expected interruption: {}",
                r.expected_downtime_hint
            ));
        }
        if !r.afterwards.is_empty() {
            out.push(format!("Afterwards: {}", r.afterwards));
        }
        out.join("\n")
    }
}

/// Limits of the daemon's control socket ([`crate::Status::limits`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[non_exhaustive]
pub struct Limits {
    /// Largest request or response frame, bytes (`[control]
    /// max_frame_bytes`). A larger response is refused with
    /// [`crate::CommandError::ResponseTooLarge`].
    pub max_frame_bytes: u64,
}

impl Limits {
    /// Limits with this frame size.
    #[must_use]
    pub fn new(max_frame_bytes: u64) -> Self {
        Self { max_frame_bytes }
    }
}
