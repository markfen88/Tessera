//! Enforcement of the device access lock (`device-lock.md` §9.2 L5) and its
//! three commands (§9.1 L2). The lock state itself is [`crate::access`].
//!
//! The guard runs before any other validation, for every origin except
//! `Internal` (the daemon's own bookkeeping, such as the device role's flow
//! installs, which keep existing media running: L6) and `Config` (file loads
//! and reloads). It is default-deny: a command this table does not name is
//! refused while any device is locked.

use std::collections::BTreeSet;

use tessera_control_types::{Command, CommandError, DeviceKey, Event, FlowSpec, Origin};

use super::{Applied, StateEngine, invalid};

/// What part of the host a command could change, for the access lock.
enum Covered<'a> {
    /// Allowed while locked: unlocking, and reloading the host's own files
    /// (policy, as for the configuration's `locked` lists).
    Nothing,
    /// The device's settings, configuration or saved subscriptions.
    Device(&'a DeviceKey),
    /// The device's settings and host-wide values (clock, primary interface).
    DeviceAndHost(&'a DeviceKey),
    /// Host-wide settings that change every device's behaviour (policy), and
    /// every command not named here (default deny).
    Host,
    /// The controller flow set: the devices whose flows it changes.
    Flows(&'a [FlowSpec]),
    /// Carrier bookkeeping and receiver operations of the local receiver,
    /// the first device (policy, conservative: they can lead to a receiver
    /// change).
    LocalReceiver,
}

fn covered(cmd: &Command) -> Covered<'_> {
    match cmd {
        Command::UnlockAccess { .. } | Command::ResetAccessLock { .. } | Command::ReloadConfig => {
            Covered::Nothing
        }
        Command::LockAccess { device, .. }
        | Command::SetDeviceName { device, .. }
        | Command::SetChannelLabel { device, .. }
        | Command::Subscribe { device, .. }
        | Command::Unsubscribe { device, .. }
        | Command::SetSampleRate { device, .. }
        | Command::SetEncoding { device, .. }
        | Command::SetLatency { device, .. }
        | Command::SetDeviceChannels { device, .. } => Covered::Device(device),
        // Also clears the clock settings and, unless kept, the primary
        // interface selection.
        Command::ClearConfiguration { device, .. } => Covered::DeviceAndHost(device),
        Command::SetProtocolFlows { flows } => Covered::Flows(flows),
        Command::RouteReceiver { .. }
        | Command::ReconcileReceiverOperation { .. }
        | Command::ReleaseRuntimeCarriers { .. }
        | Command::GrantRuntimeCarriersToNextRun { .. } => Covered::LocalReceiver,
        Command::SetClockRole { .. }
        | Command::SetClockPriority { .. }
        | Command::SetPrimaryInterface { .. } => Covered::Host,
        _ => Covered::Host,
    }
}

/// Is `cmd` one of the access-lock commands (not saved intent)?
pub(super) fn is_access_command(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::LockAccess { .. } | Command::UnlockAccess { .. } | Command::ResetAccessLock { .. }
    )
}

fn refused(device: &str) -> CommandError {
    CommandError::AccessLocked {
        device: DeviceKey(device.to_owned()),
    }
}

impl StateEngine {
    /// Is the device access lock of `device` set?
    #[must_use]
    pub fn access_locked(&self, device: &str) -> bool {
        self.access.is_locked(device)
    }

    /// Refuse a change of the local receiver (the first device) while its
    /// access lock is set: receiver operations and carrier bookkeeping that
    /// do not pass through [`apply`](Self::apply).
    ///
    /// # Errors
    /// [`CommandError::AccessLocked`].
    pub fn check_local_receiver_access(&self) -> Result<(), CommandError> {
        match self.model.devices.first() {
            Some(d) if self.access.is_locked(&d.key.0) => Err(refused(&d.key.0)),
            _ => Ok(()),
        }
    }

    /// The first configured device whose access lock is set.
    fn first_locked_device(&self) -> Option<&str> {
        self.model
            .devices
            .iter()
            .map(|d| d.key.0.as_str())
            .find(|d| self.access.is_locked(d))
    }

    /// The access-lock refusal of `cmd` from `origin`, if any (L5).
    pub(super) fn access_guard(&self, origin: &Origin, cmd: &Command) -> Result<(), CommandError> {
        if matches!(origin, Origin::Internal | Origin::Config) || self.access.any_locked().is_none()
        {
            return Ok(());
        }
        self.access_refusal(cmd)
    }

    /// The refusal of `cmd` from an origin the lock binds.
    pub(super) fn access_refusal(&self, cmd: &Command) -> Result<(), CommandError> {
        let host = || {
            self.first_locked_device()
                .map_or(Ok(()), |d| Err(refused(d)))
        };
        match covered(cmd) {
            Covered::Nothing => Ok(()),
            Covered::Device(device) if self.access.is_locked(&device.0) => Err(refused(&device.0)),
            Covered::Device(_) => Ok(()),
            Covered::DeviceAndHost(device) if self.access.is_locked(&device.0) => {
                Err(refused(&device.0))
            }
            Covered::DeviceAndHost(_) | Covered::Host => host(),
            Covered::Flows(flows) => self
                .flow_devices_changed(flows)
                .into_iter()
                .find(|d| self.access.is_locked(d))
                .map_or(Ok(()), |d| Err(refused(&d))),
            Covered::LocalReceiver => self.check_local_receiver_access(),
        }
    }

    /// The devices whose controller flows `flows` would change (added,
    /// removed, edited or reordered).
    fn flow_devices_changed(&self, flows: &[FlowSpec]) -> BTreeSet<String> {
        let mut changed = BTreeSet::new();
        let old: Vec<FlowSpec> = match &self.intent.protocol_flows {
            Some(stored) => stored.clone(),
            None => {
                let mut old = Vec::new();
                for f in &self.config.config.legacy_protocol_flows {
                    match crate::flows::to_spec(f) {
                        Ok(spec) => old.push(spec),
                        // Unreadable legacy content: its device changes.
                        Err(_) => {
                            changed.insert(f.device.clone());
                        }
                    }
                }
                old
            }
        };
        let devices: BTreeSet<&str> = old
            .iter()
            .chain(flows)
            .map(|f| f.device.0.as_str())
            .collect();
        for d in devices {
            let of = |set: &[FlowSpec]| -> Vec<FlowSpec> {
                set.iter().filter(|f| f.device.0 == d).cloned().collect()
            };
            if of(&old) != of(flows) {
                changed.insert(d.to_owned());
            }
        }
        changed
    }

    /// `LockAccess`, `UnlockAccess` and `ResetAccessLock` (L2): the lock
    /// file is written before the change is visible; `AccessLockChanged`
    /// is published after it. Nothing here goes into `state.toml`.
    pub(super) fn apply_access(
        &mut self,
        origin: Origin,
        cmd: &Command,
    ) -> Result<Applied, CommandError> {
        let user = matches!(
            origin,
            Origin::Cli { .. } | Origin::Api { .. } | Origin::Protocol { .. }
        );
        let (device, locked) = match cmd {
            Command::LockAccess { device, pin } => {
                if !user {
                    return Err(CommandError::PermissionDenied);
                }
                self.device_for(device)?;
                if !pin.is_well_formed() {
                    return Err(crate::access::malformed());
                }
                self.access.lock(&device.0, pin)?;
                (device, true)
            }
            Command::UnlockAccess { device, pin } => {
                if !user {
                    return Err(CommandError::PermissionDenied);
                }
                let now = self.clock.now();
                self.access.unlock(&device.0, pin, &origin, now)?;
                (device, false)
            }
            // Recovery without the PIN: a local administrator only (policy;
            // the control socket admits commands from root or the admin group
            // alone, and the portal's account arrives as `Api`).
            Command::ResetAccessLock { device } => {
                if !matches!(origin, Origin::Cli { .. }) {
                    return Err(CommandError::PermissionDenied);
                }
                if !self.access.reset(&device.0)? {
                    self.device_for(device)?;
                    return Ok(Applied {
                        impact: Some(crate::impact::no_impact(cmd)),
                        ..Applied::default()
                    });
                }
                (device, false)
            }
            _ => return Err(invalid("command", "not an access lock command")),
        };
        let event = Event::AccessLockChanged {
            device: device.clone(),
            locked,
            origin,
        };
        self.bus.publish(&event);
        Ok(Applied {
            changed: true,
            events: vec![event],
            effects: Vec::new(),
            impact: Some(crate::impact::no_impact(cmd)),
        })
    }
}
