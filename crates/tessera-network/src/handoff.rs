//! NE-9.5–9.10: independent root-only intent exchange and recoverable ownership.
//! Peer paths are derived from validated product parameters, never caller paths.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tessera_state::network_profiles::{
    Backend, ConfirmPolicy, ConfirmationError, Network, Profiles, Reply,
};

use crate::backend;
use crate::engine::{ConfirmationFailure, Engine, Journal};
use crate::host::{FileChange, Host, Plan, firewall_rules, interfaces};
use crate::store::{Store, token, valid_token};

pub type Result<T> = std::result::Result<T, Failure>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Product {
    Tessera,
    Ndimon,
}
impl Product {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Tessera => "tessera",
            Self::Ndimon => "ndimon",
        }
    }
    pub fn table(self) -> &'static str {
        match self {
            Self::Tessera => "tessera_network",
            Self::Ndimon => "ndimon_management",
        }
    }
    fn store(self, host: &Host) -> Store {
        Store(host.path(&format!("/var/lib/{}-network", self.prefix())))
    }
    fn exchange(self, host: &Host) -> Store {
        Store(self.store(host).0.join("handoff"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policies {
    pub confirm: ConfirmPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub version: u32,
    pub revision: u64,
    pub interface: String,
    pub backend: Backend,
    pub networks: Vec<Network>,
    pub management: String,
    pub audio: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ndi: Option<String>,
    pub policies: Policies,
}
impl Intent {
    fn new(p: Profiles, confirm: ConfirmPolicy) -> Self {
        Self {
            version: p.version,
            revision: p.revision,
            interface: p.interface,
            backend: p.backend,
            networks: p.networks,
            management: p.management,
            audio: p.audio,
            ndi: None,
            policies: Policies { confirm },
        }
    }
    pub fn profiles(&self) -> Profiles {
        Profiles {
            version: self.version,
            revision: self.revision,
            interface: self.interface.clone(),
            backend: self.backend,
            networks: self.networks.clone(),
            management: self.management.clone(),
            audio: self.audio.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Adoption {
    pub interface: String,
    pub nm_parent_uuid: Option<String>,
    pub nm_owned: BTreeMap<String, String>,
    pub parent_definition: String,
    pub dhcp_metric: u32,
    pub static_metric: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub path: String,
    pub sha256: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub schema: u32,
    pub id: String,
    pub product: Product,
    pub version: String,
    pub network: Intent,
    pub backend: Backend,
    pub adoption: Adoption,
    pub artifacts: Vec<Artifact>,
    pub nftables_table: String,
    pub bindings: BTreeMap<String, String>,
    pub created_at: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quiesce {
    pub id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ownership {
    pub id: String,
    pub receiver: Product,
    pub transaction: String,
    pub document_sha256: String,
}
/// Embedded in Tessera's ordinary journal, never substituted for that journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transfer {
    pub document: Document,
    pub document_sha256: String,
    pub source_table: String,
    pub services: BTreeMap<String, bool>,
    pub transaction: String,
}
impl Transfer {
    fn ownership(&self) -> Ownership {
        Ownership {
            id: self.document.id.clone(),
            receiver: Product::Tessera,
            transaction: self.transaction.clone(),
            document_sha256: self.document_sha256.clone(),
        }
    }
    fn store(&self, host: &Host) -> Store {
        self.document.product.store(host)
    }
}
pub use tessera_control_types::Impact;
use tessera_control_types::{ImpactToken, RestartImpact, RestartScope};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub document: Document,
    pub write_files: Vec<String>,
    pub remove_files: Vec<String>,
    pub replace_table: String,
    pub install_table: String,
    pub stop_services: Vec<String>,
    pub restart_services: Vec<String>,
    pub impact: Impact,
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ReleaseStatus {
    pub externally_managed: bool,
    pub quiesced: bool,
    pub stale_done: Vec<String>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Error {
    NotFound { what: String },
    Drift { domain: String, path: String },
    ConfirmationRequired { impact: Box<Impact> },
    Internal { message: String },
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ErrorKind {
    Network(ConfirmationError),
    Handoff(Error),
}
#[derive(Debug, Serialize)]
pub struct Failure {
    pub error: ErrorKind,
    pub message: String,
    pub http_status: u16,
}
impl From<ConfirmationFailure> for Failure {
    fn from(f: ConfirmationFailure) -> Self {
        if let Some(kind) = f.kind {
            let status = match &kind {
                ConfirmationError::Locked { .. }
                | ConfirmationError::RevisionConflict { .. }
                | ConfirmationError::StillApplying
                | ConfirmationError::WrongAddress => 409,
                ConfirmationError::Unsupported { .. } => 501,
                ConfirmationError::Invalid { .. } => 400,
                ConfirmationError::NotPending | ConfirmationError::Expired => 410,
                ConfirmationError::TokenMismatch => 403,
                ConfirmationError::RateLimited => 429,
                _ => 500,
            };
            Self {
                error: ErrorKind::Network(kind),
                message: f.message,
                http_status: status,
            }
        } else {
            Self::from(f.message)
        }
    }
}
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self {
            error: ErrorKind::Handoff(Error::Internal {
                message: message.clone(),
            }),
            message,
            http_status: 500,
        }
    }
}
impl From<backend::Failure> for Failure {
    fn from(f: backend::Failure) -> Self {
        ConfirmationFailure::from(f).into()
    }
}
fn locked(by: &str) -> Failure {
    ConfirmationFailure {
        kind: Some(ConfirmationError::Locked {
            field: "network".into(),
            by: by.into(),
        }),
        message: format!("Network operation locked by {by}."),
    }
    .into()
}
fn invalid(field: &str, reason: &str) -> Failure {
    ConfirmationFailure {
        kind: Some(ConfirmationError::Invalid {
            field: field.into(),
            reason: reason.into(),
        }),
        message: format!("Invalid {field}: {reason}."),
    }
    .into()
}
fn unsupported(what: &str) -> Failure {
    backend::unsupported(what).into()
}
fn drift(path: &str) -> Failure {
    Failure {
        error: ErrorKind::Handoff(Error::Drift {
            domain: "network".into(),
            path: path.into(),
        }),
        message: format!("Handoff artifact changed: {path}"),
        http_status: 409,
    }
}
pub fn sha256(text: &[u8]) -> String {
    format!("{:x}", Sha256::digest(text))
}
pub fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn check_id(id: &str) -> Result<()> {
    if valid_id(id) {
        Ok(())
    } else {
        Err(invalid("id", "invalid_handoff_id"))
    }
}
fn encode(value: &impl Serialize) -> crate::Result<String> {
    serde_json::to_string(value).map_err(|e| e.to_string())
}

/// NE-9.10 exact allowlist; includes retained module configuration without VLANs.
pub fn artifact_paths(p: &Profiles, product: Product) -> Vec<String> {
    let prefix = product.prefix();
    let mut paths = vec![format!("/etc/modules-load.d/{prefix}-8021q.conf")];
    match p.backend {
        Backend::Networkd | Backend::Netplan => {
            for name in interfaces(p) {
                paths.push(format!("/etc/systemd/network/00-{prefix}-{name}.network"));
                if p.backend == Backend::Networkd && name != p.interface {
                    paths.push(format!("/etc/systemd/network/00-{prefix}-{name}.netdev"));
                }
            }
            if p.backend == Backend::Netplan {
                paths.extend(match product {
                    Product::Tessera => vec!["/etc/netplan/90-tessera-network.yaml".into()],
                    Product::Ndimon => vec![
                        "/etc/netplan/90-ndimon.yaml".into(),
                        "/etc/netplan/91-ndimon-vlans.yaml".into(),
                    ],
                });
            }
        }
        Backend::NetworkManager => {
            for n in p.networks.iter().filter(|n| n.vlan_id.is_some()) {
                paths.push(format!(
                    "/etc/NetworkManager/system-connections/{prefix}-vlan-{}.nmconnection",
                    p.ifname(n)
                ));
            }
        }
        Backend::Ifupdown => paths.extend([
            format!("/etc/network/interfaces.d/{prefix}-{}", p.interface),
            format!("/etc/network/interfaces.d/{prefix}-vlans"),
            format!("/etc/dhcp/dhclient-enter-hooks.d/{prefix}-vlans"),
        ]),
    }
    paths.extend(match product {
        Product::Tessera => vec!["/etc/tessera/config.d/59-standalone-network.toml".into()],
        Product::Ndimon => [
            "/etc/ndimon-network-interfaces.json",
            "/etc/tessera/config.d/62-ndimon-network.toml",
            "/etc/systemd/system/ndimonndi.slice",
            "/etc/systemd/system/ndimontessera.slice",
            "/etc/systemd/system/ndimon-r.service.d/70-ndimon-network.conf",
            "/etc/systemd/system/ndimon-finder.service.d/70-ndimon-network.conf",
            "/etc/systemd/system/tesserad.service.d/70-ndimon-network.conf",
        ]
        .map(str::to_owned)
        .to_vec(),
    });
    paths.sort();
    paths
}
fn bindings(p: &Profiles, ndi: Option<&str>) -> Result<BTreeMap<String, String>> {
    let mut roles = vec![
        ("management", p.management.as_str()),
        ("audio", p.audio.as_str()),
    ];
    if let Some(ndi) = ndi {
        roles.push(("ndi", ndi));
    }
    roles
        .into_iter()
        .map(|(role, id)| {
            Ok((
                role.into(),
                p.ifname(
                    p.assigned(id)
                        .map_err(|_| unsupported("unrepresentable_configuration"))?,
                ),
            ))
        })
        .collect()
}
fn snapshot(host: &Host, path: &str, after: Option<String>) -> Result<FileChange> {
    let before = host.read(path)?;
    let metadata = if before.is_some() {
        Some(fs::symlink_metadata(host.path(path)).map_err(|e| e.to_string())?)
    } else {
        None
    };
    Ok(FileChange {
        path: path.into(),
        before,
        after,
        before_mode: metadata.as_ref().map_or(0o600, |m| m.mode() & 0o7777),
        before_uid: metadata.as_ref().map_or(0, MetadataExt::uid),
        before_gid: metadata.as_ref().map_or(0, MetadataExt::gid),
    })
}
fn load(host: &Host, product: Product, id: &str) -> Result<(Document, String)> {
    check_id(id)?;
    let text = product
        .exchange(host)
        .text_existing(&format!("{id}.json"))?
        .ok_or_else(|| Failure {
            error: ErrorKind::Handoff(Error::NotFound {
                what: "network_handoff".into(),
            }),
            message: "Network handoff not found.".into(),
            http_status: 404,
        })?;
    let raw: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| invalid("handoff", "invalid_document"))?;
    if !["networkd", "netplan", "networkmanager", "ifupdown"]
        .contains(&raw["backend"].as_str().unwrap_or(""))
        || !["networkd", "netplan", "networkmanager", "ifupdown"]
            .contains(&raw["network"]["backend"].as_str().unwrap_or(""))
        || raw["adoption"].get("nm_parent_uuid").is_none()
        || (raw["product"] == "tessera" && raw["network"].get("ndi").is_some())
        || raw["network"]["networks"].as_array().is_some_and(|rows| {
            rows.iter().any(|n| {
                n.get("vlan_id").is_none()
                    || (n["ipv4"]["mode"] == "static" && n["ipv4"].get("gateway").is_none())
            })
        })
    {
        return Err(invalid("handoff", "missing_required_field"));
    }
    let d: Document =
        serde_json::from_str(&text).map_err(|_| invalid("handoff", "invalid_document"))?;
    Ok((d, sha256(text.as_bytes())))
}
fn validate(host: &Host, d: &Document, product: Product, id: &str) -> Result<Profiles> {
    let date = chrono::DateTime::parse_from_rfc3339(&d.created_at)
        .map_err(|_| invalid("created_at", "invalid_timestamp"))?;
    if d.schema != 1
        || d.id != id
        || d.product != product
        || d.version.is_empty()
        || d.version.len() > 128
        || d.network.backend != d.backend
        || d.adoption.interface != d.network.interface
        || d.nftables_table != product.table()
        || date.offset().local_minus_utc() != 0
    {
        return Err(invalid("handoff", "invalid_document"));
    }
    let p = d.network.profiles();
    let mut initial = p.clone();
    initial.networks.clear();
    p.validate(&initial)
        .map_err(|_| unsupported("unrepresentable_configuration"))?;
    if p.revision == u64::MAX
        || d.network.networks.iter().any(|n| n.name.trim() != n.name)
        || (product == Product::Ndimon) != d.network.ndi.is_some()
        || d.network.ndi.as_ref().is_some_and(|ndi| ndi != &p.audio)
        || (product == Product::Ndimon
            && (p.networks.len() > 9
                || p.networks.iter().filter(|n| n.vlan_id.is_some()).count() > 8))
        || d.bindings != bindings(&p, d.network.ndi.as_deref())?
    {
        return Err(unsupported("unrepresentable_configuration"));
    }
    let allowed = artifact_paths(&p, product);
    let mut listed = BTreeSet::new();
    for artifact in &d.artifacts {
        if !allowed.contains(&artifact.path)
            || !valid_token(&artifact.sha256)
            || !listed.insert(artifact.path.clone())
        {
            return Err(unsupported("unsafe_handoff_artifact"));
        }
    }
    for path in allowed {
        let exists = match fs::symlink_metadata(host.path(&path)) {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.to_string().into()),
        };
        if exists && !listed.contains(&path) {
            return Err(unsupported("incomplete_handoff_offer"));
        }
        if !exists && listed.contains(&path) {
            return Err(drift(&path));
        }
    }
    let q: Quiesce = product
        .store(host)
        .read_existing("quiesce.json")?
        .ok_or_else(|| locked("handoff_not_prepared"))?;
    if q.id != id {
        return Err(locked("handoff"));
    }
    Ok(p)
}

impl Engine {
    /// Publish immutable intent before quiescing. An orphan offer is inert.
    pub fn prepare_handoff(&self, new_owner: &str) -> Result<Document> {
        if new_owner != "ndimon" {
            return Err(unsupported("unrepresentable_configuration"));
        }
        let _lock = self.store.lock_typed(false).map_err(|e| match e {
            crate::store::LockError::Busy(_) => locked("pending_transaction"),
            crate::store::LockError::Other(e) => e.into(),
        })?;
        let status = self.reconcile_release_locked()?;
        if status.externally_managed || self.host.external() {
            return Err(locked("external_owner"));
        }
        self.ensure_idle()?;
        if let Some(q) = self.store.read_existing::<Quiesce>("quiesce.json")? {
            return Ok(load(&self.host, Product::Tessera, &q.id)?.0);
        }
        // A release is of committed ownership, never inferred first adoption.
        let p = self
            .store
            .read_existing::<Profiles>("committed.json")?
            .ok_or_else(|| unsupported("unrepresentable_configuration"))?;
        let plan = self.host.plan(p.clone(), p.clone())?;
        let mut artifacts = Vec::new();
        for path in artifact_paths(&p, Product::Tessera) {
            if let Some(text) = self.host.read(&path)? {
                artifacts.push(Artifact {
                    path,
                    sha256: sha256(text.as_bytes()),
                });
            }
        }
        let id = token()?[..32].to_owned();
        let parent_definition = if p.backend == Backend::Netplan {
            self.host.netplan_entry(&p.interface)?.0
        } else {
            String::new()
        };
        let mut adoption = Adoption {
            interface: p.interface.clone(),
            nm_parent_uuid: None,
            nm_owned: BTreeMap::new(),
            parent_definition,
            dhcp_metric: plan.metadata.metric,
            static_metric: plan.metadata.metric,
        };
        // Only management can own a default route. Non-management static
        // links may report zero while management inherited an explicit metric.
        let management_metric = plan.metadata.metric(
            &p.ifname(
                p.assigned(&p.management)
                    .map_err(|_| unsupported("unrepresentable_configuration"))?,
            ),
        );
        adoption.dhcp_metric = management_metric;
        adoption.static_metric = management_metric;
        for c in &plan.metadata.connections {
            if c.interface == p.interface {
                adoption.nm_parent_uuid = Some(c.uuid.clone());
            } else {
                adoption
                    .nm_owned
                    .insert(c.interface.clone(), c.uuid.clone());
            }
        }
        let d = Document {
            schema: 1,
            id: id.clone(),
            product: Product::Tessera,
            version: env!("CARGO_PKG_VERSION").into(),
            backend: p.backend,
            bindings: bindings(&p, None)?,
            network: Intent::new(p, plan.confirm_policy),
            adoption,
            artifacts,
            nftables_table: "tessera_network".into(),
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        };
        Store(self.store.0.join("handoff")).write(&format!("{id}.json"), &d)?;
        self.store.write("quiesce.json", &Quiesce { id })?;
        Ok(d)
    }

    pub fn cancel_handoff(&self, id: &str) -> Result<()> {
        check_id(id)?;
        let _lock = self.store.lock(false)?;
        if self
            .store
            .read_existing::<Ownership>("external.json")?
            .is_some()
        {
            return Err(locked("handoff"));
        }
        let Some(q) = self.store.read_existing::<Quiesce>("quiesce.json")? else {
            return Ok(());
        };
        if q.id != id {
            return Err(locked("handoff"));
        }
        Product::Tessera
            .exchange(&self.host)
            .remove(&format!("{id}.json"))?;
        self.store.remove("quiesce.json")?;
        Ok(())
    }

    pub fn release_status(&self) -> Result<ReleaseStatus> {
        let q = self.store.read_existing::<Quiesce>("quiesce.json")?;
        let marker = self.store.read_existing::<Ownership>("external.json")?;
        let mut status = ReleaseStatus {
            quiesced: q.is_some(),
            ..ReleaseStatus::default()
        };
        for name in self.host.entries("/var/lib/tessera-network/handoff")? {
            let Some(id) = name.strip_suffix(".done") else {
                continue;
            };
            if !valid_id(id) {
                status.stale_done.push(name);
                continue;
            }
            let done = match Product::Tessera
                .exchange(&self.host)
                .read_existing::<Ownership>(&name)
            {
                Ok(done) => done,
                Err(_) => {
                    status.stale_done.push(name);
                    continue;
                }
            };
            let valid = if let (Some(m), Some(done)) = (marker.as_ref(), done.as_ref()) {
                m == done
                    && m.id == id
                    && q.as_ref().is_none_or(|q| q.id == id)
                    && m.receiver == Product::Ndimon
                    && valid_token(&m.transaction)
                    && valid_token(&m.document_sha256)
                    && load(&self.host, Product::Tessera, id)
                        .is_ok_and(|(_, digest)| digest == m.document_sha256)
            } else {
                false
            };
            if valid {
                status.externally_managed = true;
            } else {
                status.stale_done.push(name);
            }
        }
        if marker.is_some() && q.is_none() && !status.externally_managed {
            return Err(locked("handoff"));
        }
        Ok(status)
    }

    fn reconcile_release_locked(&self) -> Result<ReleaseStatus> {
        let mut status = self.release_status()?;
        if status.externally_managed && status.quiesced {
            let m: Ownership = self
                .store
                .read_existing("external.json")?
                .ok_or_else(|| locked("handoff"))?;
            let archives = Store(self.store.0.join("archive"));
            archives.prepare()?;
            let archive = Store(archives.0.join(&m.id));
            archive.prepare()?;
            for name in [
                "pending.json",
                "committed.json",
                "last.json",
                "backend.json",
                "received.json",
                "handoff-plan.json",
            ] {
                self.store.archive(name, &archive)?;
            }
            self.store.remove("quiesce.json")?;
            status.quiesced = false;
        }
        Ok(status)
    }
    pub fn reconcile_handoff(&self) -> Result<ReleaseStatus> {
        let status = self.release_status()?;
        if !status.externally_managed || !status.quiesced {
            return Ok(status);
        }
        let _lock = self.store.lock(false)?;
        self.reconcile_release_locked()
    }

    pub fn import_handoff(&self, product: Product, id: &str) -> Result<Review> {
        if product != Product::Ndimon {
            return Err(unsupported("handoff_same_owner"));
        }
        check_id(id)?;
        let _peer_lock = product
            .store(&self.host)
            .lock_existing()
            .map_err(|e| match e {
                crate::store::LockError::Busy(_) => locked("handoff"),
                crate::store::LockError::Other(e) => e.into(),
            })?;
        Ok(self.plan_handoff(product, id)?.0)
    }

    fn plan_handoff(&self, product: Product, id: &str) -> Result<(Review, Plan)> {
        if product != Product::Ndimon {
            return Err(unsupported("handoff_same_owner"));
        }
        check_id(id)?;
        if self
            .store
            .read_existing::<Journal>("pending.json")?
            .is_some()
        {
            return Err(locked("pending_transaction"));
        }
        if self
            .store
            .read_existing::<Quiesce>("quiesce.json")?
            .is_some()
        {
            return Err(locked("handoff"));
        }
        // Existing ownership must be explicitly released first. An archived
        // external-owner marker may be replaced only by a new receive decision.
        if self
            .store
            .read_existing::<Profiles>("committed.json")?
            .is_some()
            || self.host.firewall_snapshot()?.is_some()
        {
            return Err(locked("tessera_standalone"));
        }
        if product
            .store(&self.host)
            .read_existing::<Ownership>("external.json")?
            .is_some()
        {
            return Err(locked("handoff"));
        }
        let (d, digest) = load(&self.host, product, id)?;
        let p = validate(&self.host, &d, product, id)?;
        let source_table = self
            .host
            .table_snapshot(product.table())?
            .ok_or_else(|| unsupported("handoff_guard_missing"))?;
        let mut source_files = Vec::new();
        for a in &d.artifacts {
            let file = snapshot(&self.host, &a.path, None).map_err(|_| drift(&a.path))?;
            if file
                .before
                .as_ref()
                .is_none_or(|s| sha256(s.as_bytes()) != a.sha256)
            {
                return Err(drift(&a.path));
            }
            source_files.push(file);
        }
        let paths: Vec<_> = d.artifacts.iter().map(|a| a.path.clone()).collect();
        let mut plan = self.host.plan_handoff(p, &d.adoption, &paths)?;
        if plan.candidate != d.network.profiles() {
            return Err(unsupported("unrepresentable_configuration"));
        }
        // Never overwrite another Tessera-owned artifact during receipt.
        for file in &plan.files {
            if file.before.is_some()
                && !paths.contains(&file.path)
                && artifact_paths(&plan.candidate, Product::Tessera).contains(&file.path)
            {
                return Err(locked("tessera_standalone"));
            }
        }
        if d.backend == Backend::NetworkManager {
            for n in d.network.networks.iter().filter(|n| n.vlan_id.is_some()) {
                let name = plan.candidate.ifname(n);
                let from = format!(
                    "/etc/NetworkManager/system-connections/ndimon-vlan-{name}.nmconnection"
                );
                let to = format!(
                    "/etc/NetworkManager/system-connections/tessera-vlan-{name}.nmconnection"
                );
                let text = source_files
                    .iter()
                    .find(|f| f.path == from)
                    .and_then(|f| f.before.as_deref())
                    .ok_or_else(|| unsupported("incomplete_handoff_offer"))?;
                let uuid = d
                    .adoption
                    .nm_owned
                    .get(&name)
                    .ok_or_else(|| unsupported("unrepresentable_configuration"))?;
                plan.files.push(snapshot(
                    &self.host,
                    &to,
                    Some(crate::networkmanager::handoff_keyfile(text, &name, uuid)?),
                )?);
            }
        }
        for file in source_files {
            plan.files.retain(|f| f.path != file.path);
            plan.files.push(file);
        }
        let mut services = BTreeMap::new();
        for service in [
            "tesserad.service",
            "ndimon-r.service",
            "ndimon-finder.service",
        ] {
            services.insert(service.into(), self.host.service_active(service)?);
        }
        plan.handoff = Some(Transfer {
            document: d.clone(),
            document_sha256: digest,
            source_table,
            services,
            transaction: String::new(),
        });
        let mut impact = Impact::new("commit_network_handoff",
            RestartImpact::new(RestartScope::Daemon, "Transfer network ownership to standalone Tessera without changing addresses or VLANs.")
                .with_what_drops(vec!["Network connections and running audio consumers may be interrupted; NDIMon services are stopped.".into()])
                .with_downtime_hint("Up to 90 seconds while the backend activates.")
                .with_afterwards("Keep within 60 seconds after readiness, or restore the releasing engine."));
        impact.audio_clients_detached = plan.running;
        // Include exact before-images and runtime intent, excluding mutable nft
        // packet counters and the generated transaction identity.
        let mut context = plan.handoff.clone();
        if let Some(t) = &mut context {
            t.source_table = table_intent(&t.source_table);
        }
        let impact_digest = sha256(
            encode(&(
                &plan.files,
                &plan.metadata,
                &context,
                plan.web_port,
                plan.confirm_policy,
                &impact,
            ))?
            .as_bytes(),
        );
        impact.token = Some(ImpactToken::new(impact_digest));
        let transfer = plan
            .handoff
            .as_ref()
            .ok_or_else(|| "Missing handoff plan".to_owned())?;
        let review = Review {
            document: d,
            write_files: plan
                .files
                .iter()
                .filter(|f| f.after.is_some())
                .map(|f| f.path.clone())
                .collect(),
            remove_files: paths,
            replace_table: product.table().into(),
            install_table: "tessera_network".into(),
            stop_services: transfer
                .services
                .iter()
                .filter(|(name, running)| **running && name.as_str() != "tesserad.service")
                .map(|(name, _)| name.clone())
                .collect(),
            restart_services: if plan.running {
                vec!["tesserad.service".into()]
            } else {
                Vec::new()
            },
            impact,
        };
        Ok((review, plan))
    }

    pub fn commit_handoff(
        &self,
        product: Product,
        id: &str,
        expect_impact: Option<&str>,
        confirm: bool,
    ) -> Result<Reply> {
        if product != Product::Ndimon {
            return Err(unsupported("handoff_same_owner"));
        }
        check_id(id)?;
        let _lock = self.store.lock_typed(false).map_err(|e| match e {
            crate::store::LockError::Busy(_) => locked("pending_transaction"),
            crate::store::LockError::Other(e) => e.into(),
        })?;
        // R and A use this same lock order: A first, then R. R never acquires A.
        let peer = product.store(&self.host);
        let _peer_lock = peer.lock_typed(false).map_err(|e| match e {
            crate::store::LockError::Busy(_) => locked("handoff"),
            crate::store::LockError::Other(e) => e.into(),
        })?;
        let (review, mut plan) = self.plan_handoff(product, id)?;
        if expect_impact
            .is_some_and(|s| Some(s) != review.impact.token.as_ref().map(ImpactToken::as_str))
            || (expect_impact.is_none() && !confirm)
        {
            return Err(Failure {
                error: ErrorKind::Handoff(Error::ConfirmationRequired {
                    impact: Box::new(review.impact),
                }),
                message: "Review and confirm this network ownership transfer.".into(),
                http_status: 409,
            });
        }
        // Snapshot our previous release marker for a return transfer as well.
        for name in ["external.json", "received.json"] {
            let path = format!("/var/lib/tessera-network/{name}");
            plan.files.push(snapshot(&self.host, &path, None)?);
        }
        let token = self.stage(plan)?;
        let mut reply = self.handle(tessera_state::network_profiles::Request {
            api: tessera_state::network_profiles::NETWORK_DOCUMENT_API,
            command: tessera_state::network_profiles::Command::Status {},
        });
        reply.token = Some(token);
        Ok(reply)
    }
}

pub(crate) fn received(host: &Host) -> crate::Result<bool> {
    let Some(m) = Product::Ndimon
        .store(host)
        .read_existing::<Ownership>("external.json")?
    else {
        return Ok(false);
    };
    if m.receiver != Product::Tessera
        || !valid_id(&m.id)
        || !valid_token(&m.transaction)
        || !valid_token(&m.document_sha256)
    {
        return Ok(false);
    }
    let done: Option<Ownership> = Product::Ndimon
        .exchange(host)
        .read_existing(&format!("{}.done", m.id))?;
    Ok(done.as_ref() == Some(&m)
        && load(host, Product::Ndimon, &m.id).map_err(|e| e.message)?.1 == m.document_sha256)
}

pub(crate) fn reserve(host: &Host, t: &Transfer) -> crate::Result<()> {
    t.store(host).write("external.json", &t.ownership())
}
pub(crate) fn verify_reservation(host: &Host, t: &Transfer) -> crate::Result<()> {
    let q: Option<Quiesce> = t.store(host).read_existing("quiesce.json")?;
    let m: Option<Ownership> = t.store(host).read_existing("external.json")?;
    if q.as_ref().map(|q| q.id.as_str()) != Some(t.document.id.as_str()) || m != Some(t.ownership())
    {
        return Err("Handoff reservation changed".into());
    }
    if load(host, t.document.product, &t.document.id)
        .map_err(|e| e.message)?
        .1
        != t.document_sha256
    {
        return Err("Handoff document changed".into());
    }
    Ok(())
}
pub(crate) fn finish(engine: &Engine, t: &Transfer) -> crate::Result<()> {
    let done: Option<Ownership> = t
        .document
        .product
        .exchange(&engine.host)
        .read_existing(&format!("{}.done", t.document.id))?;
    if done.as_ref() != Some(&t.ownership()) {
        verify_reservation(&engine.host, t)?;
    }
    engine.store.write("received.json", &t.ownership())?;
    t.document
        .product
        .exchange(&engine.host)
        .write(&format!("{}.done", t.document.id), &t.ownership())
}
pub(crate) fn release_reservation(host: &Host, t: &Transfer) -> crate::Result<()> {
    let marker: Option<Ownership> = t.store(host).read_existing("external.json")?;
    if marker.as_ref().is_some_and(|m| m != &t.ownership()) {
        return Err("Handoff reservation changed during recovery".into());
    }
    t.store(host).remove("external.json")?;
    let q: Option<Quiesce> = t.store(host).read_existing("quiesce.json")?;
    if q.as_ref().is_some_and(|q| q.id != t.document.id) {
        return Err("Handoff id changed during recovery".into());
    }
    t.store(host).remove("quiesce.json")
}

pub(crate) fn switch_firewall(host: &Host, plan: &Plan, restore: bool) -> crate::Result<()> {
    let t = plan.handoff.as_ref().ok_or("Missing handoff journal")?;
    let mut rules = String::new();
    for table in ["tessera_network", t.document.product.table()] {
        if host.table_snapshot(table)?.is_some() {
            rules.push_str(&format!("delete table inet {table}\n"));
        }
    }
    if restore {
        for slice in ["ndimonndi.slice", "ndimontessera.slice"] {
            if host.path(&format!("/etc/systemd/system/{slice}")).exists() {
                host.run("/usr/bin/systemctl", &["start", slice])?;
            }
        }
        rules.push_str(&t.source_table);
        if let Some(old) = &plan.old_firewall {
            rules.push_str(old);
        }
    } else {
        host.run("/usr/bin/systemctl", &["start", "tesseraaudio.slice"])?;
        rules.push_str(&firewall_rules(&plan.candidate, plan.web_port)?);
    }
    host.runner
        .run("/usr/sbin/nft", &["-f", "-"], Some(&rules))?;
    Ok(())
}

pub(crate) fn recheck_backend(host: &Host, plan: &Plan) -> crate::Result<()> {
    let t = plan.handoff.as_ref().ok_or("Missing handoff")?;
    if host
        .table_snapshot(t.document.product.table())?
        .as_deref()
        .map(table_intent)
        != Some(table_intent(&t.source_table))
    {
        return Err("Releasing owner's firewall changed".into());
    }
    for (service, active) in &t.services {
        if host.service_active(service)? != *active {
            return Err("Releasing owner's service state changed".into());
        }
    }
    match plan.candidate.backend {
        Backend::NetworkManager => crate::networkmanager::recheck_handoff(host, plan),
        Backend::Ifupdown => {
            let m = crate::ifupdown::handoff_metadata(host, &plan.previous)
                .map_err(|e| e.to_string())?;
            if m.metrics != plan.metadata.metrics {
                return Err("Releasing owner's ifupdown intent changed".into());
            }
            Ok(())
        }
        _ => {
            let m = host
                .networkd_metadata(&plan.previous, plan.candidate.backend == Backend::Netplan)
                .map_err(|e| e.to_string())?;
            if m.metrics != plan.metadata.metrics || m.selected != plan.metadata.selected {
                return Err("Releasing owner's backend intent changed".into());
            }
            let offered: Vec<_> = t.document.artifacts.iter().map(|a| &a.path).collect();
            if host
                .drift(&plan.candidate)
                .map_err(|e| e.to_string())?
                .iter()
                .any(|d| d.blocking && !offered.contains(&&d.path))
            {
                return Err("Foreign configuration appeared after handoff review".into());
            }
            Ok(())
        }
    }
}
fn table_intent(text: &str) -> String {
    let mut words = Vec::new();
    let mut counter = false;
    for word in text.split_whitespace() {
        if !counter || !word.bytes().all(|b| b.is_ascii_digit()) {
            words.push(word);
        }
        counter = matches!(word, "packets" | "bytes");
    }
    words.join(" ")
}
pub(crate) fn verify_restored(host: &Host, plan: &Plan) -> crate::Result<()> {
    for f in &plan.files {
        if host.read(&f.path)? != f.before {
            return Err(format!("Handoff restore differs: {}", f.path));
        }
    }
    match plan.previous.backend {
        Backend::Networkd | Backend::Netplan => host.verify_networkd(plan, true)?,
        Backend::NetworkManager => {
            backend::adapter(plan.previous.backend).verify(host, plan, true)?
        }
        Backend::Ifupdown => {
            crate::ifupdown::handoff_metadata(host, &plan.previous).map_err(|e| e.to_string())?;
        }
    }
    let mut previous = plan.previous.clone();
    previous.audio = previous.management.clone();
    host.verify_addresses(&previous)
}

/// Additive helper requests; the ordinary API-1/API-2 network wire is unchanged.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub api: u32,
    pub command: Command,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    HandoffPrepare {
        receiver: Product,
    },
    HandoffCancel {
        id: String,
    },
    HandoffImport {
        product: Product,
        id: String,
    },
    HandoffCommit {
        product: Product,
        id: String,
        expect_impact: Option<String>,
        #[serde(default)]
        confirm: bool,
    },
    HandoffStatus {},
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ControlRequest {
    Handoff(Request),
    Network(tessera_state::network_profiles::Request),
}
impl Engine {
    pub fn handle_control(&self, req: ControlRequest) -> serde_json::Value {
        let ControlRequest::Handoff(req) = req else {
            if let ControlRequest::Network(req) = req {
                return serde_json::json!(self.handle(req));
            }
            unreachable!();
        };
        let result: Result<serde_json::Value> = (|| {
            if req.api != 2 {
                return Err(invalid("api", "handoff_requires_api_2"));
            }
            match req.command {
                Command::HandoffPrepare { receiver } => Ok(
                    serde_json::json!({"api":2,"handoff":self.prepare_handoff(receiver.prefix())?}),
                ),
                Command::HandoffCancel { id } => {
                    self.cancel_handoff(&id)?;
                    Ok(serde_json::json!({"api":2,"ok":true}))
                }
                Command::HandoffImport { product, id } => {
                    Ok(serde_json::json!({"api":2,"review":self.import_handoff(product,&id)?}))
                }
                Command::HandoffCommit {
                    product,
                    id,
                    expect_impact,
                    confirm,
                } => Ok(serde_json::json!(self.commit_handoff(
                    product,
                    &id,
                    expect_impact.as_deref(),
                    confirm
                )?)),
                Command::HandoffStatus {} => {
                    Ok(serde_json::json!({"api":2,"handoff":self.reconcile_handoff()?}))
                }
            }
        })();
        match result {
            Ok(value) => value,
            Err(e) => serde_json::json!(e),
        }
    }
}
