//! Error types.

use crate::config::BackendKind;
use crate::txt::TxtError;

/// Why service data was rejected before it reached a backend.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ValidationError {
    /// The device key is empty.
    #[error("device key is empty")]
    EmptyDevice,
    /// The hostname is empty, contains `.` or control characters, or is too long.
    #[error("invalid hostname {name:?}: {reason}")]
    Hostname {
        /// The offending hostname.
        name: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The instance name is empty, contains control characters, or is too long.
    #[error("invalid instance name {name:?}: {reason}")]
    Instance {
        /// The offending instance name.
        name: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// Port 0 means "service not available" in DNS-SD and is refused.
    #[error("service {instance:?} has port 0")]
    ZeroPort {
        /// The instance with the bad port.
        instance: String,
    },
    /// Two services of one device share kind and instance name.
    #[error("duplicate service {instance:?} of the same kind")]
    DuplicateService {
        /// The duplicated instance name.
        instance: String,
    },
    /// The TXT record of a service violates the limits.
    #[error("service {instance:?}: {source}")]
    Txt {
        /// The instance whose TXT record is bad.
        instance: String,
        /// The TXT problem.
        #[source]
        source: TxtError,
    },
}

/// Errors of the discovery API.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The data given to the API is invalid.
    #[error(transparent)]
    Invalid(#[from] ValidationError),
    /// A TXT record could not be built.
    #[error(transparent)]
    Txt(#[from] TxtError),
    /// A device with this key is already registered.
    #[error("device {0:?} is already registered")]
    AlreadyRegistered(String),
    /// No device with this key is registered.
    #[error("device {0:?} is not registered")]
    NotRegistered(String),
    /// The update tried to change something that cannot change in place.
    #[error("device {device:?}: {reason}")]
    UnsupportedUpdate {
        /// The device key.
        device: String,
        /// What cannot change.
        reason: &'static str,
    },
    /// A configured network interface does not exist.
    #[error("unknown network interface {0:?}")]
    UnknownInterface(String),
    /// No interface is configured and there is no wired interface with carrier
    /// to default to.
    #[error("no interface configured and no wired interface with carrier to default to")]
    NoDefaultInterface,
    /// Another process has UDP port 5353 on one of our interfaces and
    /// [`crate::PortConflict::Refuse`] is configured.
    #[error("UDP port 5353 is already bound by another process: {0}")]
    PortInUse(String),
    /// `avahi` mode was requested (or Avahi was selected) but it cannot be used.
    #[error("Avahi unavailable: {0}")]
    AvahiUnavailable(String),
    /// The system bus could not be queried.
    #[error("cannot query the system bus: {0}")]
    Bus(String),
    /// The backend failed.
    #[error("{backend} backend: {message}")]
    Backend {
        /// Which backend.
        backend: BackendKind,
        /// What happened.
        message: String,
    },
    /// The discovery instance has been shut down.
    #[error("discovery has been shut down")]
    Closed,
}
