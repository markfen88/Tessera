//! Explicit peer endpoints and references to provisioned secrets/certificates.
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;

/// Optional routing federation; an empty configuration enables no peers.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Separate HTTPS listener for machine requests; the HTTP portal stays as configured.
    pub listen: Option<SocketAddr>,
    /// PEM certificate chain for the machine listener.
    pub certificate_file: Option<PathBuf>,
    /// PEM private key, readable only by the service account/administrator.
    pub private_key_file: Option<PathBuf>,
    /// Named revocable machine credentials, stored as SHA256 hashes.
    pub credential: Vec<Credential>,
    /// Explicitly provisioned receiver hosts; never populated from discovery.
    pub peer: Vec<Peer>,
}

/// A high-entropy machine credential; its value never appears in configuration.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    /// Audit principal, unique on this host.
    pub id: String,
    /// Only this host's receiver with this native identity can be operated.
    pub receiver_wire_id: String,
    /// File holding the lowercase SHA256 of the complete bearer value.
    pub hash_file: PathBuf,
}

/// A pinned receiver-host portal. Certificate rotation requires explicit reprovisioning.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    /// Opaque local configuration reference, safe for browser/API use.
    pub id: String,
    /// HTTPS origin only, with no path, userinfo, query or fragment.
    pub origin: String,
    /// Expected native receiver wire identity, also checked against discovery.
    pub wire_id: String,
    /// The peer's PEM certificate, used as the sole TLS trust anchor.
    pub certificate_file: PathBuf,
    /// SHA256 of the exact DER leaf certificate (not a CA or WebPKI policy).
    pub server_sha256: String,
    /// Protected file containing `principal.64-hex-random-bytes` bearer value.
    pub credential_file: PathBuf,
}

pub(super) fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

impl Config {
    /// Validate without reading credentials or binding a socket.
    pub fn validate(&self) -> Result<(), String> {
        if self.peer.len() > 16 || self.credential.len() > 16 {
            return Err("routing supports at most 16 peers and 16 credentials".into());
        }
        if self.listen.is_some()
            != (self.certificate_file.is_some() && self.private_key_file.is_some())
            || (self.listen.is_none()
                && (!self.credential.is_empty()
                    || self.certificate_file.is_some()
                    || self.private_key_file.is_some()))
        {
            return Err(
                "routing credentials require listen, certificate_file and private_key_file".into(),
            );
        }
        let mut ids = std::collections::BTreeSet::new();
        for credential in &self.credential {
            if !name(&credential.id)
                || !ids.insert(&credential.id)
                || !credential.hash_file.is_absolute()
                || credential.receiver_wire_id.is_empty()
                || credential.receiver_wire_id.len() > 128
                || !credential
                    .receiver_wire_id
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit())
            {
                return Err("invalid or duplicate routing credential reference".into());
            }
        }
        ids.clear();
        let mut wires = std::collections::BTreeSet::new();
        for peer in &self.peer {
            let uri: axum::http::Uri = peer
                .origin
                .parse()
                .map_err(|_| "invalid routing peer origin")?;
            if !name(&peer.id)
                || !ids.insert(&peer.id)
                || !wires.insert(&peer.wire_id)
                || peer.wire_id.is_empty()
                || peer.wire_id.len() > 128
                || !peer.wire_id.bytes().all(|b| b.is_ascii_hexdigit())
                || uri.scheme_str() != Some("https")
                || uri.host().is_none()
                || uri.authority().is_none_or(|a| a.as_str().contains('@'))
                || uri.path_and_query().is_some_and(|p| p.as_str() != "/")
                || peer.origin.contains('#')
                || peer.server_sha256.len() != 64
                || !peer
                    .server_sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                || !peer.certificate_file.is_absolute()
                || !peer.credential_file.is_absolute()
            {
                return Err("invalid or duplicate routing peer configuration".into());
            }
        }
        for path in [&self.certificate_file, &self.private_key_file]
            .into_iter()
            .flatten()
        {
            if !path.is_absolute() {
                return Err("routing certificate/key paths must be absolute".into());
            }
        }
        Ok(())
    }
}
