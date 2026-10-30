//! Backend-neutral data: what to advertise and what is discovered.
//!
//! These are plain data types. The daemon fills [`DeviceServices`] from its
//! own state; this crate depends on no state types.

use core::fmt;
use std::net::IpAddr;

use crate::config::Limits;
use crate::error::ValidationError;
use crate::names::sanitize;
use crate::provisional as p;
use crate::txt::TxtRecord;

/// The service types of the spec (section 2). Which wire type name each maps
/// to is defined in [`crate::provisional`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum ServiceKind {
    /// Routing control endpoint, one per device.
    Arc,
    /// Control and monitoring endpoint, one per device.
    Cmc,
    /// A transmit channel.
    Chan,
    /// An active multicast flow.
    Bund,
}

impl ServiceKind {
    /// All kinds.
    pub const ALL: [ServiceKind; 4] = [
        ServiceKind::Arc,
        ServiceKind::Cmc,
        ServiceKind::Chan,
        ServiceKind::Bund,
    ];

    /// The DNS-SD service type without domain, e.g. `_netaudio-arc._udp`.
    #[must_use]
    pub fn type_label(self) -> &'static str {
        match self {
            ServiceKind::Arc => p::TYPE_ARC,
            ServiceKind::Cmc => p::TYPE_CMC,
            ServiceKind::Chan => p::TYPE_CHAN,
            ServiceKind::Bund => p::TYPE_BUND,
        }
    }

    /// The service type with the domain, e.g. `_netaudio-arc._udp.local.`.
    #[must_use]
    pub fn type_domain(self) -> String {
        format!("{}.{}", self.type_label(), p::DOMAIN)
    }

    /// Maps a service type (with or without trailing `.local.`, ASCII case
    /// ignored) back to its kind.
    #[must_use]
    pub fn from_type(ty: &str) -> Option<ServiceKind> {
        let ty = ty.to_ascii_lowercase();
        let ty = ty.strip_suffix('.').unwrap_or(&ty);
        let ty = ty.strip_suffix(".local").unwrap_or(ty);
        Self::ALL.into_iter().find(|k| k.type_label() == ty)
    }
}

/// Identity of a service instance: its kind and (unescaped) instance name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ServiceId {
    /// The kind.
    pub kind: ServiceKind,
    /// The instance name, a single DNS label (may contain dots and spaces).
    pub instance: String,
}

impl ServiceId {
    /// Creates an id.
    #[must_use]
    pub fn new(kind: ServiceKind, instance: impl Into<String>) -> Self {
        Self {
            kind,
            instance: instance.into(),
        }
    }
}

impl fmt::Display for ServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{}.{}",
            sanitize(&self.instance),
            self.kind.type_label(),
            p::DOMAIN
        )
    }
}

/// One service instance to advertise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    /// The kind.
    pub kind: ServiceKind,
    /// The requested instance name. If it is taken on the network the
    /// responder picks another name and reports it in a
    /// [`ResponderEvent::Conflict`].
    pub instance: String,
    /// The SRV port.
    pub port: u16,
    /// The TXT record.
    pub txt: TxtRecord,
}

impl ServiceSpec {
    /// The identity of this service (by requested name).
    #[must_use]
    pub fn id(&self) -> ServiceId {
        ServiceId::new(self.kind, self.instance.clone())
    }
}

/// All services of one logical device. A device is registered, updated and
/// withdrawn as a unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceServices {
    /// Stable key of the logical device (not published). Updates and
    /// withdrawals refer to it.
    pub device: String,
    /// The device's friendly hostname without domain; the SRV target is
    /// `<hostname>.local.`.
    pub hostname: String,
    /// Addresses to publish for the hostname. Empty means: the addresses of
    /// the configured interfaces.
    pub addresses: Vec<IpAddr>,
    /// The services.
    pub services: Vec<ServiceSpec>,
}

fn bad_label(s: &str) -> Option<&'static str> {
    if s.is_empty() {
        Some("empty")
    } else if s.chars().any(char::is_control) {
        Some("contains a control character")
    } else {
        None
    }
}

impl DeviceServices {
    /// Checks names, ports, uniqueness and TXT sizes against `limits`.
    ///
    /// # Errors
    /// The first problem found, as a [`ValidationError`].
    pub fn validate(&self, limits: &Limits) -> Result<(), ValidationError> {
        if self.device.is_empty() {
            return Err(ValidationError::EmptyDevice);
        }
        let host_err = |reason| ValidationError::Hostname {
            name: self.hostname.clone(),
            reason,
        };
        if let Some(r) = bad_label(&self.hostname) {
            return Err(host_err(r));
        }
        if self.hostname.contains('.') {
            return Err(host_err("contains a dot"));
        }
        if self.hostname.len() > limits.max_hostname_len {
            return Err(host_err("too long"));
        }
        let mut seen = std::collections::HashSet::new();
        for s in &self.services {
            let inst_err = |reason| ValidationError::Instance {
                name: s.instance.clone(),
                reason,
            };
            if let Some(r) = bad_label(&s.instance) {
                return Err(inst_err(r));
            }
            if s.instance.len() > limits.max_instance_len {
                return Err(inst_err("too long"));
            }
            if s.port == 0 {
                return Err(ValidationError::ZeroPort {
                    instance: s.instance.clone(),
                });
            }
            if !seen.insert((s.kind, s.instance.to_ascii_lowercase())) {
                return Err(ValidationError::DuplicateService {
                    instance: s.instance.clone(),
                });
            }
            s.txt
                .validate(&limits.txt)
                .map_err(|source| ValidationError::Txt {
                    instance: s.instance.clone(),
                    source,
                })?;
        }
        Ok(())
    }

    /// The SRV target host name, `<hostname>.local.`.
    #[must_use]
    pub fn host_fqdn(&self) -> String {
        format!("{}.{}", self.hostname, p::DOMAIN)
    }
}

/// A discovered service instance, merged over all interfaces it was seen on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInstance {
    /// Kind and instance name.
    pub id: ServiceId,
    /// The SRV target, e.g. `studio-a.local.`.
    pub host: String,
    /// The SRV port.
    pub port: u16,
    /// Resolved addresses of the host, sorted.
    pub addresses: Vec<IpAddr>,
    /// The TXT record.
    pub txt: TxtRecord,
}

/// One backend source's observation, without merging its TXT or provenance
/// with another interface. Source identifiers are browser-scoped, not device
/// identities. A backend that cannot establish an interface reports `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ServiceObservation {
    /// Opaque source within this browser.
    pub source: String,
    /// Browser-scoped incarnation of this source's service. Removal and
    /// identical reappearance change this value; ordinary refreshes do not.
    /// Zero means lifecycle information was not supplied.
    pub incarnation: u64,
    /// Observing interface, when established by the backend.
    pub interface: Option<String>,
    /// The service as seen by this source only.
    pub service: ServiceInstance,
}

impl ServiceObservation {
    /// One source view; use None when interface provenance is unavailable.
    #[must_use]
    pub fn new(
        source: impl Into<String>,
        interface: Option<String>,
        service: ServiceInstance,
    ) -> Self {
        Self {
            source: source.into(),
            incarnation: 0,
            interface,
            service,
        }
    }
}

/// One established service from authoritative responder state.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PublishedService {
    /// Logical device that owns this advertisement.
    pub device: String,
    /// Actual established instance/hostname, including conflict renames.
    pub service: ServiceInstance,
}

/// What a [`crate::Browser`] reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowseEvent {
    /// A new instance was found and resolved.
    Appeared(ServiceInstance),
    /// A known instance or one of its source views changed. The merged
    /// payload may be unchanged; use Browser::observations for source detail.
    Updated(ServiceInstance),
    /// An instance went away (goodbye or TTL expiry).
    Disappeared(ServiceId),
    /// The consumer was too slow and `lost` events were dropped so far. The
    /// stream is no longer complete: resynchronise with
    /// [`crate::Browser::snapshot`].
    Overflow {
        /// Events lost since the browser started.
        lost: u64,
    },
}

impl fmt::Display for BrowseEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (tag, i) = match self {
            BrowseEvent::Appeared(i) => ("appeared", i),
            BrowseEvent::Updated(i) => ("updated", i),
            BrowseEvent::Disappeared(id) => return write!(f, "disappeared {id}"),
            BrowseEvent::Overflow { lost } => return write!(f, "overflow lost={lost}"),
        };
        write!(
            f,
            "{tag} {} host={} port={}",
            i.id,
            sanitize(&i.host),
            i.port
        )?;
        for a in &i.addresses {
            write!(f, " addr={a}")?;
        }
        for e in i.txt.entries() {
            match e.value_str() {
                Some(v) => write!(f, " txt:{}={}", sanitize(&e.key), sanitize(v))?,
                None => write!(f, " txt:{}", sanitize(&e.key))?,
            }
        }
        Ok(())
    }
}

/// What was in conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictScope {
    /// A service instance name.
    Instance,
    /// The host name of the device.
    Host,
}

/// What a [`crate::Responder`] reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponderEvent {
    /// Probing finished and the service is announced under `instance`.
    Announced {
        /// The device key.
        device: String,
        /// The service as requested.
        id: ServiceId,
        /// The name it is announced under (differs from the requested one
        /// after a conflict).
        instance: String,
    },
    /// The requested name was already in use on the network. The responder
    /// has picked `assigned` and carries on with it.
    Conflict {
        /// The device key.
        device: String,
        /// Whether an instance name or the host name conflicted.
        scope: ConflictScope,
        /// The service as requested (for a host conflict: the first service
        /// of the device, as the host is shared).
        id: Option<ServiceId>,
        /// The name that was requested.
        requested: String,
        /// The name now in use.
        assigned: String,
    },
    /// The service was withdrawn (goodbye sent).
    Withdrawn {
        /// The device key.
        device: String,
        /// The service as requested.
        id: ServiceId,
    },
    /// The backend failed for this device.
    Failed {
        /// The device key.
        device: String,
        /// What happened.
        message: String,
    },
}

impl fmt::Display for ResponderEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResponderEvent::Announced {
                device,
                id,
                instance,
            } => write!(
                f,
                "announced device={} service={id} instance={}",
                sanitize(device),
                sanitize(instance)
            ),
            ResponderEvent::Conflict {
                device,
                scope,
                requested,
                assigned,
                ..
            } => write!(
                f,
                "conflict device={} scope={scope:?} requested={} assigned={}",
                sanitize(device),
                sanitize(requested),
                sanitize(assigned)
            ),
            ResponderEvent::Withdrawn { device, id } => {
                write!(f, "withdrawn device={} service={id}", sanitize(device))
            }
            ResponderEvent::Failed { device, message } => {
                write!(
                    f,
                    "failed device={} {}",
                    sanitize(device),
                    sanitize(message)
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev() -> DeviceServices {
        DeviceServices {
            device: "d1".into(),
            hostname: "studio-a".into(),
            addresses: vec![],
            services: vec![ServiceSpec {
                kind: ServiceKind::Arc,
                instance: "studio-a".into(),
                port: 4440,
                txt: TxtRecord::new(),
            }],
        }
    }

    #[test]
    fn kinds_map_to_types_and_back() {
        for k in ServiceKind::ALL {
            assert_eq!(ServiceKind::from_type(k.type_label()), Some(k));
            assert_eq!(ServiceKind::from_type(&k.type_domain()), Some(k));
            assert_eq!(
                ServiceKind::from_type(&k.type_domain().to_uppercase()),
                Some(k)
            );
        }
        assert_eq!(ServiceKind::from_type("_http._tcp"), None);
        assert_eq!(
            ServiceKind::Chan.type_domain(),
            "_netaudio-chan._udp.local."
        );
    }

    #[test]
    fn names_from_the_network_are_escaped_when_displayed() {
        let mut txt = TxtRecord::new();
        txt.push("k", "v\u{202e}\nx").unwrap();
        let ev = BrowseEvent::Appeared(ServiceInstance {
            id: ServiceId::new(ServiceKind::Arc, "a\u{7}b\u{2066}"),
            host: "h\r.local.".into(),
            port: 1,
            addresses: vec![],
            txt,
        });
        let s = ev.to_string();
        assert!(
            !s.chars().any(|c| c.is_control()
                || ('\u{202a}'..='\u{202e}').contains(&c)
                || c == '\u{2066}'),
            "{s:?}"
        );
        assert!(s.contains("\\u{7}") && s.contains("\\u{202e}"), "{s}");
    }

    #[test]
    fn service_id_displays_full_name() {
        let id = ServiceId::new(ServiceKind::Cmc, "a b");
        assert_eq!(id.to_string(), "a b._netaudio-cmc._udp.local.");
    }

    #[test]
    fn validation() {
        let l = Limits::default();
        assert!(dev().validate(&l).is_ok());
        let mut d = dev();
        d.device.clear();
        assert_eq!(d.validate(&l), Err(ValidationError::EmptyDevice));
        let mut d = dev();
        d.hostname = "a.b".into();
        assert!(matches!(
            d.validate(&l),
            Err(ValidationError::Hostname { .. })
        ));
        let mut d = dev();
        d.hostname = "x".repeat(32);
        assert!(matches!(
            d.validate(&l),
            Err(ValidationError::Hostname {
                reason: "too long",
                ..
            })
        ));
        let relaxed = Limits {
            max_hostname_len: 63,
            ..l
        };
        assert!(d.validate(&relaxed).is_ok());
        let mut d = dev();
        d.services[0].instance = "x".repeat(64);
        assert!(matches!(
            d.validate(&l),
            Err(ValidationError::Instance {
                reason: "too long",
                ..
            })
        ));
        let mut d = dev();
        d.services[0].port = 0;
        assert!(matches!(
            d.validate(&l),
            Err(ValidationError::ZeroPort { .. })
        ));
        let mut d = dev();
        d.services.push(d.services[0].clone());
        d.services[1].instance = "STUDIO-A".into();
        assert!(matches!(
            d.validate(&l),
            Err(ValidationError::DuplicateService { .. })
        ));
        let mut d = dev();
        d.services[0].txt = TxtRecord::new().with("k", "x".repeat(300)).unwrap();
        assert!(matches!(d.validate(&l), Err(ValidationError::Txt { .. })));
    }
}
