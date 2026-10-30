//! The device access lock (`spec/protocol/device-lock.md` §§9.1-9.2): which
//! devices are locked, their PIN hashes, and the unlock rate limit.
//!
//! * **State** (L1): per device key, absent (unlocked) or locked with the
//!   Argon2id hash of its PIN (a PHC string), or locked with no usable hash
//!   when the file could not be read: then only a local reset unlocks it
//!   (fail closed).
//! * **File** (L3): `access-lock.toml` beside `state.toml`, mode 0600,
//!   replaced atomically before a change becomes visible. `state.toml` is
//!   0640 and group-readable, so it never holds a hash; the name differs from
//!   the daemon's instance lock `tesserad.lock`. Not touched by reloads or by
//!   `ClearConfiguration` (`guess` that a device keeps its lock through a
//!   reset, §6.4).
//! * **Secrecy** (L4): the PIN and the hashes never leave this module. The
//!   hash only keeps the digits out of casual view (a four-digit PIN has
//!   10 000 values); the file mode is the protection.
//! * **Rate limit** (L2, policy): five wrong PINs for one device refuse
//!   unlocking it from the same class of origin for 60 s. Counted in memory
//!   on the engine clock; a daemon restart forgets the count.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};
use tessera_control_types::{AccessPin, CommandError, Origin};

use crate::env::PinCost;
use crate::error::Issue;
use crate::persist::write_atomic_mode;

/// The file beside `state.toml`.
pub const FILE_NAME: &str = "access-lock.toml";
/// Its mode: only the daemon user reads the hashes.
pub const FILE_MODE: u32 = 0o600;
/// Wrong PINs for one device before unlocking it pauses (policy, L2).
pub const MAX_WRONG_PINS: u32 = 5;
/// How long unlocking pauses after [`MAX_WRONG_PINS`] (policy, L2).
pub const LOCKOUT: Duration = Duration::from_secs(60);

const HEADER: &str = "# Device access lock of tesserad. Written by the\n\
                      # daemon; do not edit. Keep this file mode 0600.\n";

/// A stored PIN hash. Never printed.
#[derive(Clone, PartialEq, Eq)]
struct Verifier(String);

impl fmt::Debug for Verifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Verifier(<redacted>)")
    }
}

/// Who may keep trying: wrong PINs are counted per device and per kind of
/// origin, so a guessing portal session does not lock the CLI out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum OriginClass {
    Cli,
    Api,
    Protocol,
    Other,
}

impl OriginClass {
    fn of(origin: &Origin) -> Self {
        match origin {
            Origin::Cli { .. } => Self::Cli,
            Origin::Api { .. } => Self::Api,
            Origin::Protocol { .. } => Self::Protocol,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Failures {
    wrong: u32,
    /// Unlocking pauses until then (engine clock).
    until: Option<Duration>,
}

#[derive(Default, Serialize, Deserialize)]
struct FileRepr {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    device: BTreeMap<String, DeviceRepr>,
}

#[derive(Default, Serialize, Deserialize)]
struct DeviceRepr {
    /// Argon2id PHC string; absent: locked, PIN unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verifier: Option<String>,
}

/// The access-lock state of every device. See the module documentation.
#[derive(Debug)]
pub(crate) struct AccessLocks {
    path: PathBuf,
    cost: PinCost,
    /// Locked devices; `None`: locked, but its hash could not be read.
    locked: BTreeMap<String, Option<Verifier>>,
    failures: BTreeMap<(String, OriginClass), Failures>,
    /// Why the file could not be used at start (shown with the warnings).
    problem: Option<String>,
}

impl AccessLocks {
    /// Read the file at `path`. Missing: nothing is locked. Unreadable or
    /// not understood: every device in `devices` is locked without a usable
    /// PIN and a warning says so. A file others can read is made 0600.
    pub(crate) fn load<'a>(
        path: PathBuf,
        devices: impl IntoIterator<Item = &'a str>,
        cost: PinCost,
    ) -> Self {
        let mut me = Self {
            path,
            cost,
            locked: BTreeMap::new(),
            failures: BTreeMap::new(),
            problem: None,
        };
        match std::fs::read_to_string(&me.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return me,
            Err(e) => {
                let why = format!("cannot read {}: {e}", me.path.display());
                me.fail_closed(devices, why);
            }
            // The parser's message quotes the file: never show it.
            Ok(text) => match toml::from_str::<FileRepr>(&text) {
                Ok(repr) => {
                    for (device, d) in repr.device {
                        let verifier = d
                            .verifier
                            .filter(|v| PasswordHash::new(v).is_ok())
                            .map(Verifier);
                        if verifier.is_none() {
                            me.problem = Some(format!(
                                "the access lock of device `{device}` has no usable PIN hash in {}",
                                me.path.display()
                            ));
                        }
                        me.locked.insert(device, verifier);
                    }
                }
                Err(_) => {
                    let why = format!("{} is not a valid access lock file", me.path.display());
                    me.fail_closed(devices, why);
                }
            },
        }
        tighten_mode(&me.path);
        me
    }

    fn fail_closed<'a>(&mut self, devices: impl IntoIterator<Item = &'a str>, why: String) {
        for d in devices {
            self.locked.insert(d.to_owned(), None);
        }
        self.problem = Some(why);
    }

    /// Is `device` locked?
    pub(crate) fn is_locked(&self, device: &str) -> bool {
        self.locked.contains_key(device)
    }

    /// The first locked device, if any.
    pub(crate) fn any_locked(&self) -> Option<&str> {
        self.locked.keys().next().map(String::as_str)
    }

    /// The warning about an unusable file, while a device it locked stays
    /// locked without a PIN.
    pub(crate) fn warning(&self) -> Option<Issue> {
        let problem = self.problem.as_ref()?;
        self.locked.values().any(Option::is_none).then(|| {
            Issue::warning(
                "access_lock",
                format!(
                    "{problem}; the affected devices stay locked and no PIN unlocks them: \
                     a local administrator can reset the lock (`tesseractl access reset`)"
                ),
            )
        })
    }

    /// Lock `device` with `pin` (checked well formed by the caller; the
    /// caller also refuses a device that is already locked). Persisted first.
    pub(crate) fn lock(&mut self, device: &str, pin: &AccessPin) -> Result<(), CommandError> {
        let verifier = hash(pin.reveal(), self.cost).map_err(|e| CommandError::Internal {
            message: format!("cannot hash the PIN: {e}"),
        })?;
        let mut next = self.locked.clone();
        next.insert(device.to_owned(), Some(Verifier(verifier)));
        self.commit(next)?;
        self.forget_failures(device);
        Ok(())
    }

    /// Unlock `device` with `pin` from `origin` at `now` (engine clock).
    pub(crate) fn unlock(
        &mut self,
        device: &str,
        pin: &AccessPin,
        origin: &Origin,
        now: Duration,
    ) -> Result<(), CommandError> {
        let Some(verifier) = self.locked.get(device) else {
            return Err(CommandError::Invalid {
                field: "device".into(),
                reason: format!("device `{device}` is not locked"),
            });
        };
        let key = (device.to_owned(), OriginClass::of(origin));
        let failures = self.failures.entry(key).or_default();
        if let Some(until) = failures.until {
            if now < until {
                return Err(CommandError::RateLimited);
            }
            *failures = Failures::default();
        }
        // Not a guess: no stored PIN has this form. Not counted.
        if !pin.is_well_formed() {
            return Err(malformed());
        }
        let right = verifier
            .as_ref()
            .is_some_and(|v| verify(pin.reveal(), &v.0));
        if !right {
            failures.wrong += 1;
            if failures.wrong >= MAX_WRONG_PINS {
                *failures = Failures {
                    wrong: 0,
                    until: Some(now.saturating_add(LOCKOUT)),
                };
            }
            return Err(CommandError::IncorrectPin);
        }
        let mut next = self.locked.clone();
        next.remove(device);
        self.commit(next)?;
        self.forget_failures(device);
        Ok(())
    }

    /// Unlock `device` without the PIN; whether it was locked.
    pub(crate) fn reset(&mut self, device: &str) -> Result<bool, CommandError> {
        if !self.is_locked(device) {
            return Ok(false);
        }
        let mut next = self.locked.clone();
        next.remove(device);
        self.commit(next)?;
        self.forget_failures(device);
        Ok(true)
    }

    fn forget_failures(&mut self, device: &str) {
        self.failures.retain(|(d, _), _| d != device);
    }

    /// Write `next`, then make it the state; on a failed write nothing changes.
    fn commit(&mut self, next: BTreeMap<String, Option<Verifier>>) -> Result<(), CommandError> {
        let repr = FileRepr {
            version: 1,
            device: next
                .iter()
                .map(|(d, v)| {
                    (
                        d.clone(),
                        DeviceRepr {
                            verifier: v.as_ref().map(|v| v.0.clone()),
                        },
                    )
                })
                .collect(),
        };
        let body = toml::to_string(&repr).map_err(|e| CommandError::Internal {
            message: format!("cannot render the access lock: {e}"),
        })?;
        write_atomic_mode(&self.path, format!("{HEADER}{body}").as_bytes(), FILE_MODE).map_err(
            |e| CommandError::Internal {
                message: format!("cannot save the access lock: {e}"),
            },
        )?;
        self.locked = next;
        if self.locked.values().all(Option::is_some) {
            self.problem = None;
        }
        Ok(())
    }
}

/// The refusal of a PIN that is not four digits (never quoting it).
pub(crate) fn malformed() -> CommandError {
    CommandError::Invalid {
        field: "pin".into(),
        reason: "the PIN must be exactly four digits 0-9".into(),
    }
}

/// Make an existing file 0600 if others could read it (best effort).
fn tighten_mode(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.permissions().mode() & 0o077 != 0 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(FILE_MODE));
        }
    }
}

/// An Argon2id PHC string of `pin` with a random 16-byte salt.
fn hash(pin: &str, cost: PinCost) -> Result<String, String> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|e| e.to_string())?;
    let salt = SaltString::encode_b64(&salt).map_err(|e| e.to_string())?;
    let params =
        Params::new(cost.memory_kib, cost.iterations, 1, None).map_err(|e| e.to_string())?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password(pin.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

/// Does `pin` match the PHC string `stored`? Its own parameters apply.
fn verify(pin: &str, stored: &str) -> bool {
    PasswordHash::new(stored).is_ok_and(|h| {
        Argon2::default()
            .verify_password(pin.as_bytes(), &h)
            .is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hash_verifies_its_pin_only_and_never_contains_it() {
        let h = hash("0429", PinCost::MINIMAL).unwrap();
        assert!(h.starts_with("$argon2id$"), "{h}");
        assert!(!h.contains("0429"));
        assert!(verify("0429", &h));
        assert!(!verify("0428", &h));
        // Salted: the same PIN hashes differently each time.
        assert_ne!(h, hash("0429", PinCost::MINIMAL).unwrap());
        assert_eq!(
            format!("{:?}", Verifier(h)),
            "Verifier(<redacted>)",
            "never printed"
        );
    }
}
