//! Password, sessions and login rate limiting (ADR 0013).

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};

/// Name of the session cookie.
pub const COOKIE: &str = "tessera_session";

/// Shortest password `set-password` accepts.
pub const MIN_PASSWORD_LEN: usize = 8;

/// `n` random bytes as lowercase hex.
///
/// # Panics
/// If the operating system has no randomness to give (nothing sensible can
/// continue without it).
#[must_use]
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("the operating system provides randomness");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// An Argon2id hash (PHC string) of `password` with a random salt.
///
/// # Errors
/// The hasher's message.
pub fn hash_password(password: &str) -> Result<String, String> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|e| e.to_string())?;
    let salt = SaltString::encode_b64(&salt).map_err(|e| e.to_string())?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

/// Does `password` match the PHC string `stored`?
#[must_use]
pub fn verify_password(password: &str, stored: &str) -> bool {
    PasswordHash::new(stored.trim()).is_ok_and(|h| {
        Argon2::default()
            .verify_password(password.as_bytes(), &h)
            .is_ok()
    })
}

/// The password file, read at every login so that `set-password` takes
/// effect without a restart.
#[derive(Debug, Clone)]
pub struct PasswordFile(pub PathBuf);

/// The state of the password file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stored {
    /// A hash to check against.
    Hash(String),
    /// No file: nobody can log in yet.
    Missing,
    /// The file exists but cannot be read or holds no hash.
    Unreadable(String),
}

impl PasswordFile {
    /// Read the stored hash.
    #[must_use]
    pub fn read(&self) -> Stored {
        match std::fs::read_to_string(&self.0) {
            Ok(t) if t.trim().starts_with("$argon2") => Stored::Hash(t.trim().to_owned()),
            Ok(_) => Stored::Unreadable(format!("{} holds no password hash", self.0.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Stored::Missing,
            Err(e) => Stored::Unreadable(format!("{}: {e}", self.0.display())),
        }
    }

    /// The path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// One logged-in browser or API client.
#[derive(Debug, Clone)]
pub struct Session {
    /// Token for state-changing requests.
    pub csrf: String,
    /// Short id for logs (never the token).
    pub id: String,
    last: Instant,
}

/// The sessions, in memory.
#[derive(Debug)]
pub struct Sessions {
    map: Mutex<HashMap<String, Session>>,
    idle: Duration,
    max: usize,
}

impl Sessions {
    /// Sessions ending after `idle` without use; at most `max` at a time (the
    /// oldest is dropped).
    #[must_use]
    pub fn new(idle: Duration, max: usize) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            idle,
            max: max.max(1),
        }
    }

    /// Start a session; returns its token and the session.
    pub fn create(&self, now: Instant) -> (String, Session) {
        let token = random_hex(32);
        let s = Session {
            csrf: random_hex(32),
            id: random_hex(4),
            last: now,
        };
        if let Ok(mut m) = self.map.lock() {
            m.retain(|_, v| now.duration_since(v.last) < self.idle);
            if m.len() >= self.max {
                if let Some(oldest) = m.iter().min_by_key(|(_, v)| v.last).map(|(k, _)| k.clone()) {
                    m.remove(&oldest);
                }
            }
            m.insert(token.clone(), s.clone());
        }
        (token, s)
    }

    /// The session for `token`, if it is still valid; marks it used.
    pub fn get(&self, token: &str, now: Instant) -> Option<Session> {
        let mut m = self.map.lock().ok()?;
        let s = m.get_mut(token)?;
        if now.duration_since(s.last) >= self.idle {
            m.remove(token);
            return None;
        }
        s.last = now;
        Some(s.clone())
    }

    /// End a session.
    pub fn remove(&self, token: &str) {
        if let Ok(mut m) = self.map.lock() {
            m.remove(token);
        }
    }
}

/// Failure times per address, and all failure times.
type Failures = (HashMap<IpAddr, Vec<Instant>>, Vec<Instant>);

/// Failed logins: per address and overall, in a sliding window.
#[derive(Debug)]
pub struct LoginLimiter {
    inner: Mutex<Failures>,
    window: Duration,
    per_addr: usize,
    total: usize,
}

impl LoginLimiter {
    /// At most `per_addr` failures per address and `total` overall per
    /// `window`.
    #[must_use]
    pub fn new(window: Duration, per_addr: usize, total: usize) -> Self {
        Self {
            inner: Mutex::new((HashMap::new(), Vec::new())),
            window,
            per_addr,
            total,
        }
    }

    /// May `addr` try now?
    pub fn allowed(&self, addr: IpAddr, now: Instant) -> bool {
        let Ok(mut g) = self.inner.lock() else {
            return false;
        };
        let (per, all) = &mut *g;
        let w = self.window;
        all.retain(|t| now.duration_since(*t) < w);
        per.retain(|_, v| {
            v.retain(|t| now.duration_since(*t) < w);
            !v.is_empty()
        });
        all.len() < self.total && per.get(&addr).is_none_or(|v| v.len() < self.per_addr)
    }

    /// Count a failure.
    pub fn failed(&self, addr: IpAddr, now: Instant) {
        if let Ok(mut g) = self.inner.lock() {
            g.0.entry(addr).or_default().push(now);
            g.1.push(now);
        }
    }
}

/// Constant-time comparison of two tokens.
#[must_use]
pub fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hash_verifies_its_password_only() {
        let h = hash_password("correct horse").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify_password("correct horse", &h));
        assert!(!verify_password("wrong horse", &h));
        assert!(!verify_password("correct horse", "garbage"));
    }

    #[test]
    fn sessions_expire_when_idle() {
        let s = Sessions::new(Duration::from_secs(10), 4);
        let t0 = Instant::now();
        let (tok, sess) = s.create(t0);
        assert_eq!(
            s.get(&tok, t0 + Duration::from_secs(5)).unwrap().csrf,
            sess.csrf
        );
        // Used at 5 s, so still valid at 14 s; gone 10 s after the last use.
        assert!(s.get(&tok, t0 + Duration::from_secs(14)).is_some());
        assert!(s.get(&tok, t0 + Duration::from_secs(24)).is_none());
        assert!(s.get("nope", t0).is_none());
    }

    #[test]
    fn the_oldest_session_makes_room() {
        let s = Sessions::new(Duration::from_secs(100), 2);
        let t0 = Instant::now();
        let (a, _) = s.create(t0);
        let (b, _) = s.create(t0 + Duration::from_secs(1));
        let (c, _) = s.create(t0 + Duration::from_secs(2));
        let now = t0 + Duration::from_secs(3);
        assert!(s.get(&a, now).is_none());
        assert!(s.get(&b, now).is_some() && s.get(&c, now).is_some());
    }

    #[test]
    fn failures_are_limited_per_address_and_overall() {
        let l = LoginLimiter::new(Duration::from_secs(60), 2, 3);
        let t0 = Instant::now();
        let a: IpAddr = [10, 0, 0, 1].into();
        let b: IpAddr = [10, 0, 0, 2].into();
        l.failed(a, t0);
        assert!(l.allowed(a, t0));
        l.failed(a, t0);
        assert!(!l.allowed(a, t0));
        assert!(l.allowed(b, t0));
        l.failed(b, t0);
        assert!(!l.allowed(b, t0), "the overall limit is reached");
        assert!(l.allowed(a, t0 + Duration::from_secs(61)));
    }

    #[test]
    fn tokens_compare_exactly() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "ab"));
        assert_eq!(random_hex(16).len(), 32);
        assert_ne!(random_hex(16), random_hex(16));
    }
}
