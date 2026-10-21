//! Who is on the other end of a local socket, and what may they do
//! (ADR 0010).
//!
//! The kernel records the peer's uid, effective gid and supplementary groups
//! when it connects (`SO_PEERCRED`, `SO_PEERGROUPS`). Reading is open to every
//! peer that can open the socket (0660, group `tessera`: the first gate).
//! Mutating commands need uid 0 or the admin group (`[daemon] admin_group`,
//! default `tessera-admin`), matched against the peer's effective gid or any
//! supplementary group.
//!
//! **No user-database lookup happens on the connection path.** The admin
//! group name is resolved to a gid by the state thread (which may block on
//! NSS) at start-up and on every reload; the connection tasks only compare
//! numbers. Access is evaluated per request against the *current* admin gid,
//! so a reload that changes `admin_group` takes effect for connections that
//! are already open. A numeric `admin_group` is taken as a gid.

use nix::unistd::{Gid, Group};

/// What a peer may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// May run mutating commands.
    Admin,
    /// May only query and watch.
    ReadOnly,
}

impl Access {
    /// May this peer change state?
    #[must_use]
    pub fn can_mutate(self) -> bool {
        self == Access::Admin
    }
}

/// Kernel-recorded credentials of a connected peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerCred {
    /// Effective uid at connect time.
    pub uid: u32,
    /// Effective gid at connect time.
    pub gid: u32,
    /// Process id (informational; may be reused after the peer exits).
    pub pid: Option<i32>,
    /// Supplementary groups at connect time (empty if the kernel could not
    /// say).
    pub groups: Vec<u32>,
}

/// Decide the access class of a peer for the given admin gid (`None` = the
/// admin group does not exist: only uid 0 is an administrator).
#[must_use]
pub fn classify(cred: &PeerCred, admin_gid: Option<u32>) -> Access {
    if cred.uid == 0 {
        return Access::Admin;
    }
    match admin_gid {
        Some(g) if cred.gid == g || cred.groups.contains(&g) => Access::Admin,
        _ => Access::ReadOnly,
    }
}

/// Turn the configured admin group (a name, or a number taken as a gid) into a
/// gid. Blocking (user database): call from a thread that may block.
#[must_use]
pub fn resolve_admin_group(spec: &str) -> Option<u32> {
    if let Ok(n) = spec.parse::<u32>() {
        return Some(n);
    }
    match Group::from_name(spec) {
        Ok(Some(g)) => Some(g.gid.as_raw()),
        _ => None,
    }
}

/// Resolve a group given as a name or a number.
///
/// # Errors
/// A message if the group does not exist.
pub fn resolve_group(spec: &str) -> Result<Gid, String> {
    if let Ok(n) = spec.parse::<u32>() {
        return Ok(Gid::from_raw(n));
    }
    match Group::from_name(spec) {
        Ok(Some(g)) => Ok(g.gid),
        Ok(None) => Err(format!("group `{spec}` does not exist")),
        Err(e) => Err(format!("cannot look up group `{spec}`: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred(uid: u32, gid: u32, groups: &[u32]) -> PeerCred {
        PeerCred {
            uid,
            gid,
            pid: None,
            groups: groups.to_vec(),
        }
    }

    #[test]
    fn root_is_admin_even_without_the_group() {
        assert_eq!(classify(&cred(0, 0, &[]), None), Access::Admin);
        assert_eq!(classify(&cred(0, 5, &[]), Some(99)), Access::Admin);
    }

    #[test]
    fn effective_gid_and_supplementary_groups_both_count() {
        assert_eq!(classify(&cred(1000, 99, &[]), Some(99)), Access::Admin);
        assert_eq!(
            classify(&cred(1000, 1000, &[4, 99, 7]), Some(99)),
            Access::Admin
        );
        assert_eq!(
            classify(&cred(1000, 1000, &[4, 7]), Some(99)),
            Access::ReadOnly
        );
    }

    #[test]
    fn a_missing_admin_group_means_nobody_but_root() {
        assert_eq!(
            classify(&cred(1000, 1000, &[0, 1000]), None),
            Access::ReadOnly
        );
        // Group 0 membership is not root.
        assert_eq!(classify(&cred(1000, 0, &[0]), Some(1)), Access::ReadOnly);
    }

    #[test]
    fn numeric_group_names_are_gids() {
        assert_eq!(resolve_admin_group("4242"), Some(4242));
        assert_eq!(resolve_admin_group("root"), Some(0));
        assert_eq!(resolve_admin_group("no-such-group-xyz"), None);
        assert_eq!(resolve_group("1234").unwrap().as_raw(), 1234);
        assert!(resolve_group("no-such-group-xyz").is_err());
    }
}
