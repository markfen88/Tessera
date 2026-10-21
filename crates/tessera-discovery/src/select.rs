//! Choosing between the built-in responder and Avahi.
//!
//! Two responders on UDP port 5353 of the same interface answer every query
//! twice and fight over names, so exactly one backend is chosen per
//! [`crate::Discovery`] and used for everything it advertises and browses.
//!
//! * [`Mode::Builtin`]: built-in, always. If `avahi-daemon` is on the system
//!   bus a warning is logged (the operator asked for two responders), nothing
//!   fails.
//! * [`Mode::Avahi`]: Avahi, or an error if it is not on the bus.
//! * [`Mode::Auto`]: Avahi if `org.freedesktop.Avahi` has an owner on the
//!   system bus, built-in if the bus is unreachable or the name has no owner.
//!   If the bus is reachable but cannot be queried this is an error rather
//!   than a guess, because guessing wrong creates the duplicate responder.
//!   Bus activation is deliberately not used: a daemon that is installed but
//!   not running does not count as present.
//!
//! The bus check cannot tell *which network namespace* the Avahi daemon runs
//! in: a process in another namespace sees the same system bus socket. Use an
//! explicit mode there.
//!
//! Once Avahi is chosen, a failure to talk to it is an error. It never falls
//! back to the built-in responder.

use std::future::Future;

use crate::config::{BackendKind, Mode};
use crate::error::Error;

/// Well-known bus name of `avahi-daemon`.
pub const AVAHI_BUS_NAME: &str = "org.freedesktop.Avahi";

/// Why the system bus could not tell whether Avahi is present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    /// There is no system bus to connect to.
    BusUnreachable(String),
    /// The bus is there but the name query failed.
    Query(String),
}

/// Answers "is Avahi on the system bus?". The real implementation is
/// [`SystemBus`]; tests use a fake.
pub trait BusProbe {
    /// Whether `org.freedesktop.Avahi` currently has an owner.
    fn avahi_present(&self) -> impl Future<Output = Result<bool, ProbeError>> + Send;
}

/// The real system bus.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemBus;

impl BusProbe for SystemBus {
    async fn avahi_present(&self) -> Result<bool, ProbeError> {
        let conn = zbus::Connection::system()
            .await
            .map_err(|e| classify_connect_error(&e))?;
        let dbus = zbus::fdo::DBusProxy::new(&conn)
            .await
            .map_err(|e| ProbeError::Query(e.to_string()))?;
        let name = zbus::names::BusName::try_from(AVAHI_BUS_NAME)
            .map_err(|e| ProbeError::Query(e.to_string()))?;
        dbus.name_has_owner(name)
            .await
            .map_err(|e| ProbeError::Query(e.to_string()))
    }
}

/// Only "nothing there" (`ENOENT`: no socket, `ECONNREFUSED`: nobody listens)
/// counts as Avahi not being present. Anything else (permission denied, a
/// malformed bus address, protocol errors) is an error: guessing "not there"
/// would start a second responder next to a running Avahi.
pub(crate) fn classify_connect_error(e: &zbus::Error) -> ProbeError {
    use std::io::ErrorKind;
    if let zbus::Error::InputOutput(io) = e {
        if matches!(
            io.kind(),
            ErrorKind::NotFound | ErrorKind::ConnectionRefused
        ) {
            return ProbeError::BusUnreachable(e.to_string());
        }
    }
    ProbeError::Query(e.to_string())
}

/// Logs (and records in `flag`) when Avahi takes its bus name while the
/// built-in responder is running: from then on two responders share port
/// 5353. Returns when the bus cannot be watched.
pub(crate) async fn watch_avahi_appears(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use futures_util::StreamExt;
    let Ok(conn) = zbus::Connection::system().await else {
        return;
    };
    let Ok(dbus) = zbus::fdo::DBusProxy::new(&conn).await else {
        return;
    };
    let Ok(mut stream) = dbus
        .receive_name_owner_changed_with_args(&[(0, AVAHI_BUS_NAME)])
        .await
    else {
        return;
    };
    while let Some(sig) = stream.next().await {
        if sig.args().is_ok_and(|a| a.new_owner().is_some()) {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            tracing::warn!(
                "avahi-daemon appeared on the system bus while the built-in mDNS \
                 responder is running: two responders now share UDP port 5353"
            );
        }
    }
}

/// Outcome of [`select_backend`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// The backend to use.
    pub kind: BackendKind,
    /// Set when the choice leaves two responders on the network.
    pub warning: Option<String>,
}

/// Applies the rules in the module docs.
///
/// # Errors
/// [`Error::AvahiUnavailable`] for `Mode::Avahi` without Avahi on the bus;
/// [`Error::Bus`] for `Mode::Auto` when the bus is reachable but the query
/// failed.
pub async fn select_backend(mode: Mode, probe: &impl BusProbe) -> Result<Selection, Error> {
    match mode {
        Mode::Builtin => {
            let warning = match probe.avahi_present().await {
                Ok(true) => Some(
                    "built-in mDNS responder forced while avahi-daemon is on the system bus: \
                     two responders will share UDP port 5353"
                        .to_owned(),
                ),
                _ => None,
            };
            if let Some(w) = &warning {
                tracing::warn!("{w}");
            }
            Ok(Selection {
                kind: BackendKind::Builtin,
                warning,
            })
        }
        Mode::Avahi => match probe.avahi_present().await {
            Ok(true) => Ok(Selection {
                kind: BackendKind::Avahi,
                warning: None,
            }),
            Ok(false) => Err(Error::AvahiUnavailable(format!(
                "{AVAHI_BUS_NAME} has no owner on the system bus"
            ))),
            Err(ProbeError::BusUnreachable(e) | ProbeError::Query(e)) => {
                Err(Error::AvahiUnavailable(e))
            }
        },
        Mode::Auto => match probe.avahi_present().await {
            Ok(true) => Ok(Selection {
                kind: BackendKind::Avahi,
                warning: None,
            }),
            Ok(false) | Err(ProbeError::BusUnreachable(_)) => Ok(Selection {
                kind: BackendKind::Builtin,
                warning: None,
            }),
            Err(ProbeError::Query(e)) => Err(Error::Bus(e)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake bus with a canned answer.
    struct Fake(Result<bool, ProbeError>);

    impl BusProbe for Fake {
        async fn avahi_present(&self) -> Result<bool, ProbeError> {
            self.0.clone()
        }
    }

    fn run<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn pick(mode: Mode, bus: Result<bool, ProbeError>) -> Result<Selection, Error> {
        run(select_backend(mode, &Fake(bus)))
    }

    fn unreachable() -> ProbeError {
        ProbeError::BusUnreachable("no socket".into())
    }

    #[test]
    fn auto_prefers_avahi_when_on_the_bus() {
        let s = pick(Mode::Auto, Ok(true)).unwrap();
        assert_eq!(s.kind, BackendKind::Avahi);
        assert!(s.warning.is_none());
    }

    #[test]
    fn auto_uses_builtin_without_avahi_or_bus() {
        assert_eq!(
            pick(Mode::Auto, Ok(false)).unwrap().kind,
            BackendKind::Builtin
        );
        assert_eq!(
            pick(Mode::Auto, Err(unreachable())).unwrap().kind,
            BackendKind::Builtin
        );
    }

    #[test]
    fn auto_does_not_guess_when_the_query_fails() {
        assert!(matches!(
            pick(Mode::Auto, Err(ProbeError::Query("denied".into()))),
            Err(Error::Bus(_))
        ));
    }

    #[test]
    fn explicit_avahi_requires_avahi() {
        assert_eq!(
            pick(Mode::Avahi, Ok(true)).unwrap().kind,
            BackendKind::Avahi
        );
        assert!(matches!(
            pick(Mode::Avahi, Ok(false)),
            Err(Error::AvahiUnavailable(_))
        ));
        assert!(matches!(
            pick(Mode::Avahi, Err(unreachable())),
            Err(Error::AvahiUnavailable(_))
        ));
    }

    #[test]
    fn explicit_builtin_warns_but_works_alongside_avahi() {
        let s = pick(Mode::Builtin, Ok(true)).unwrap();
        assert_eq!(s.kind, BackendKind::Builtin);
        assert!(s.warning.is_some());
        let s = pick(Mode::Builtin, Ok(false)).unwrap();
        assert!(s.warning.is_none());
        let s = pick(Mode::Builtin, Err(unreachable())).unwrap();
        assert_eq!(s.kind, BackendKind::Builtin);
    }

    /// Every combination of mode and bus state, with the exact outcome.
    #[test]
    fn selection_table() {
        use BackendKind::{Avahi, Builtin};
        let q = || ProbeError::Query("denied".into());
        // (mode, bus, expected kind or None for an error, expected warning)
        #[allow(clippy::type_complexity)]
        let cases: Vec<(Mode, Result<bool, ProbeError>, Option<BackendKind>, bool)> = vec![
            (Mode::Auto, Ok(true), Some(Avahi), false),
            (Mode::Auto, Ok(false), Some(Builtin), false),
            (Mode::Auto, Err(unreachable()), Some(Builtin), false),
            (Mode::Auto, Err(q()), None, false),
            (Mode::Builtin, Ok(true), Some(Builtin), true),
            (Mode::Builtin, Ok(false), Some(Builtin), false),
            (Mode::Builtin, Err(unreachable()), Some(Builtin), false),
            (Mode::Builtin, Err(q()), Some(Builtin), false),
            (Mode::Avahi, Ok(true), Some(Avahi), false),
            (Mode::Avahi, Ok(false), None, false),
            (Mode::Avahi, Err(unreachable()), None, false),
            (Mode::Avahi, Err(q()), None, false),
        ];
        for (mode, bus, kind, warning) in cases {
            let r = pick(mode, bus.clone());
            match (kind, r) {
                (Some(k), Ok(s)) => {
                    assert_eq!(s.kind, k, "{mode:?} {bus:?}");
                    assert_eq!(s.warning.is_some(), warning, "{mode:?} {bus:?}");
                }
                (None, Err(_)) => {}
                (k, r) => panic!("{mode:?} {bus:?}: expected {k:?}, got {r:?}"),
            }
        }
    }

    #[test]
    fn only_a_missing_socket_or_refusal_means_avahi_is_not_there() {
        use std::io::{Error as IoError, ErrorKind};
        let io = |k| zbus::Error::InputOutput(std::sync::Arc::new(IoError::from(k)));
        for k in [ErrorKind::NotFound, ErrorKind::ConnectionRefused] {
            assert!(matches!(
                classify_connect_error(&io(k)),
                ProbeError::BusUnreachable(_)
            ));
        }
        for k in [
            ErrorKind::PermissionDenied,
            ErrorKind::TimedOut,
            ErrorKind::BrokenPipe,
        ] {
            assert!(matches!(
                classify_connect_error(&io(k)),
                ProbeError::Query(_)
            ));
        }
        assert!(matches!(
            classify_connect_error(&zbus::Error::Address("bad".into())),
            ProbeError::Query(_)
        ));
    }

    /// Real connection attempts to a missing socket and to an unreadable path.
    #[tokio::test(flavor = "current_thread")]
    async fn real_connect_errors_are_classified() {
        let build = |addr: String| async move {
            zbus::connection::Builder::address(addr.as_str())
                .unwrap()
                .build()
                .await
        };
        let e = build("unix:path=/nonexistent-tessera/bus".into())
            .await
            .unwrap_err();
        assert!(
            matches!(classify_connect_error(&e), ProbeError::BusUnreachable(_)),
            "{e:?}"
        );
        // A directory without permission: EACCES, which is not "not there".
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tessera-noperm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable = std::fs::read_dir(&dir).is_ok(); // root ignores modes
        if !readable {
            let e = build(format!("unix:path={}/bus", dir.display()))
                .await
                .unwrap_err();
            assert!(
                matches!(classify_connect_error(&e), ProbeError::Query(_)),
                "{e:?}"
            );
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }
}
