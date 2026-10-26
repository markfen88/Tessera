//! Journal before side effects; a committed revision is the durable commit
//! point. Independent system-manager jobs recover after browser/API failure.

use std::fs::{self, File};
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use nix::fcntl::Flock;
use serde::{Deserialize, Serialize};
use tessera_state::network_profiles::{
    APPLY_DELAY_SECONDS, CONFIRM_SECONDS, Command, ConfirmPolicy, ConfirmationError, Document,
    GUARD_SECONDS, Ipv4, LEGACY_MAX_REVISION, NETWORK_API, NETWORK_DOCUMENT_API, Pending, Profiles,
    Reply, Request,
};

use crate::Result;
use crate::host::{Host, Plan};
use crate::store::{LockError, Store, token, valid_token};

const JOURNAL: &str = "pending.json";
const COMMITTED: &str = "committed.json";
/// How long after a recovery that did not verify the helper tries again (NE-6.13, U12).
const RECOVERY_RETRY_SECONDS: u64 = 10;

/// How long a Revert waits for the apply worker's lock (see
/// [`Engine::lock_for_revert`]). Each request has its own thread, so waiting
/// holds up no other request.
pub const REVERT_LOCK_WAIT: Duration = Duration::from_secs(5);

/// The retryable refusal (HTTP 409) while the apply worker holds the lock or
/// the phase is not yet `applied`.
fn still_applying() -> ConfirmationFailure {
    ConfirmationFailure::new(
        ConfirmationError::StillApplying,
        "The network change is still being applied or recovered. Try again in a moment.",
    )
}

/// A recovery that was attempted and did not verify (HW-3, D15): typed, with the detail the
/// helper has, and not an internal failure. The journal stays and the helper retries it.
fn recovery_failure(detail: String) -> ConfirmationFailure {
    ConfirmationFailure::new(
        ConfirmationError::ApplyFailed {
            domain: "network".into(),
            step: "recover".into(),
            reason: detail.clone(),
        },
        format!(
            "The previous network settings could not be verified yet; the helper tries again every {RECOVERY_RETRY_SECONDS} seconds ({detail})."
        ),
    )
}

/// Persisted before any file/firewall/backend change, never web-writable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Journal {
    /// Public identity for systemd jobs; never use the browser secret in argv.
    pub job_id: String,
    pub token: String,
    pub plan: Plan,
    pub phase: String,
    pub deadline: f64,
    pub boot_id: String,
    pub error: Option<String>,
    #[serde(default)]
    pub recovery_error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LastResult {
    token: String,
    message: String,
    #[serde(default)]
    error_kind: Option<ConfirmationError>,
}

/// A typed confirmation refusal, or an unclassified helper failure.
#[derive(Debug)]
pub struct ConfirmationFailure {
    /// Stable kind for clients; older/unclassified failures have none.
    pub kind: Option<ConfirmationError>,
    /// Human-readable explanation, also retained in the legacy reply field.
    pub message: String,
}

impl ConfirmationFailure {
    fn new(kind: ConfirmationError, message: impl Into<String>) -> Self {
        Self {
            kind: Some(kind),
            message: message.into(),
        }
    }
}

impl From<crate::backend::Failure> for ConfirmationFailure {
    fn from(failure: crate::backend::Failure) -> Self {
        Self {
            kind: failure.kind,
            message: failure.message,
        }
    }
}

impl From<String> for ConfirmationFailure {
    fn from(message: String) -> Self {
        Self {
            kind: None,
            message,
        }
    }
}

impl From<&str> for ConfirmationFailure {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}

/// The single writer for standalone host-networking state.
#[derive(Debug, Clone)]
pub struct Engine {
    pub store: Store,
    pub host: Host,
}

impl Default for Engine {
    fn default() -> Self {
        Self {
            store: Store("/var/lib/tessera-network".into()),
            host: Host::default(),
        }
    }
}

impl Engine {
    fn current(&self) -> std::result::Result<Profiles, ConfirmationFailure> {
        if let Some(p) = self.store.read(COMMITTED)? {
            return Ok(p);
        }
        if let Some(j) = self.store.read::<Journal>(JOURNAL)? {
            return Ok(j.plan.previous);
        }
        self.host.adopt().map_err(Into::into)
    }

    /// Queries cannot leak a pending token. The initiator receives it once.
    pub fn handle(&self, req: Request) -> Reply {
        let mut reply = Reply {
            api: if req.api == NETWORK_DOCUMENT_API {
                NETWORK_DOCUMENT_API
            } else {
                NETWORK_API
            },
            externally_managed: self.host.external(),
            profiles: None,
            document: None,
            pending: None,
            token: None,
            error: None,
            error_kind: None,
            confirm_policy: ConfirmPolicy::default(),
            drift: Vec::new(),
        };
        let result: std::result::Result<(), ConfirmationFailure> = (|| {
            if req.api != NETWORK_API && req.api != NETWORK_DOCUMENT_API {
                return Err("Unsupported network API version; install matching packages.".into());
            }
            let release = self.reconcile_handoff().map_err(|e| e.message)?;
            reply.externally_managed |= release.externally_managed;
            if !release.stale_done.is_empty() {
                eprintln!(
                    "tessera-network: ignored stale handoff completion(s): {:?}",
                    release.stale_done
                );
            }
            if release.quiesced && !matches!(req.command, Command::Status {}) {
                return Err(ConfirmationFailure::new(
                    ConfirmationError::Locked {
                        field: "network".into(),
                        by: "handoff".into(),
                    },
                    "Network ownership handoff is prepared.",
                ));
            }
            let receiving = self
                .store
                .read::<Journal>(JOURNAL)?
                .is_some_and(|j| j.plan.handoff.is_some());
            if reply.externally_managed && !receiving {
                return match req.command {
                    Command::Status {} => Ok(()),
                    _ => Err(ConfirmationFailure::new(
                        ConfirmationError::Locked {
                            field: "network".into(),
                            by: "ndimon".into(),
                        },
                        "NDIMon manages this appliance's networking.",
                    )),
                };
            }
            reply.confirm_policy = self.confirm_policy();
            let mut document_policy = None;
            let command = match req.command {
                Command::PreviewDocument { .. } | Command::ApplyDocument { .. }
                    if req.api != NETWORK_DOCUMENT_API =>
                {
                    return Err("Semantic network documents require API 2".into());
                }
                Command::PreviewDocument { document, session } => {
                    document_policy = Some(document.policies.confirm);
                    match session {
                        Some(session) => Command::PreviewFrom {
                            profiles: document.into_profiles(),
                            session,
                        },
                        None => Command::Preview {
                            profiles: document.into_profiles(),
                        },
                    }
                }
                Command::ApplyDocument { document, session } => {
                    document_policy = Some(document.policies.confirm);
                    match session {
                        Some(session) => Command::ApplyFrom {
                            profiles: document.into_profiles(),
                            session,
                        },
                        None => Command::Apply {
                            profiles: document.into_profiles(),
                        },
                    }
                }
                Command::Apply { .. }
                | Command::Preview { .. }
                | Command::ApplyFrom { .. }
                | Command::PreviewFrom { .. }
                    if req.api == NETWORK_DOCUMENT_API =>
                {
                    return Err("API 2 requires a semantic network document".into());
                }
                command => command,
            };
            let session = match &command {
                Command::ApplyFrom { session, .. } | Command::PreviewFrom { session, .. } => {
                    Some(session.clone())
                }
                _ => None,
            };
            match command {
                Command::Status {} => (),
                Command::Pending { token } => {
                    self.matching(&token)?;
                }
                Command::Preview { profiles } | Command::PreviewFrom { profiles, .. } => {
                    let _lock = self.store.lock(false)?;
                    self.ensure_idle()?;
                    self.ensure_not_quiesced()?;
                    let previous = self.current()?;
                    Self::legacy_revision(req.api, &previous, &profiles)?;
                    if let Some(session) = &session {
                        self.host
                            .guard_session(&previous, &profiles, session, true)?;
                    }
                    let plan = self.host.plan(previous, profiles)?;
                    Self::check_document_policy(document_policy, plan.confirm_policy)?;
                }
                Command::Apply { profiles } | Command::ApplyFrom { profiles, .. } => {
                    let _lock = self.store.lock(false)?;
                    self.ensure_idle()?;
                    self.ensure_not_quiesced()?;
                    let previous = self.current()?;
                    Self::legacy_revision(req.api, &previous, &profiles)?;
                    if let Some(session) = &session {
                        self.host
                            .guard_session(&previous, &profiles, session, true)?;
                    }
                    let mut plan = self.host.plan(previous, profiles)?;
                    Self::check_document_policy(document_policy, plan.confirm_policy)?;
                    plan.session = session;
                    if plan.previous == plan.candidate {
                        reply.profiles = Some(plan.previous);
                        return Ok(());
                    }
                    reply.token = Some(self.stage(plan)?);
                }
                Command::Confirm {
                    token,
                    local_address,
                } => {
                    // Apply holds the mutation lock. Classify its public phase
                    // first, so early Keep presses never become lock failures.
                    let journal = self.matching(&token)?;
                    self.confirmable(&journal, local_address)?;
                    Self::legacy_revision(
                        req.api,
                        &journal.plan.previous,
                        &journal.plan.candidate,
                    )?;
                    let _lock = self.store.lock_typed(false).map_err(|error| match error {
                        LockError::Busy(_) => still_applying(),
                        LockError::Other(message) => ConfirmationFailure::from(message),
                    })?;
                    let journal = self.matching(&token)?;
                    self.confirmable(&journal, local_address)?;
                    let mut committed = journal.plan.candidate.clone();
                    committed.revision = committed.revision.checked_add(1).ok_or_else(|| {
                        ConfirmationFailure::new(
                            ConfirmationError::Invalid {
                                field: "revision".into(),
                                reason: "revision_overflow".into(),
                            },
                            "Network revision exhausted.",
                        )
                    })?;
                    // Recovery must never undo this durable commit, even if the
                    // process stops before deleting its journal/timers.
                    self.store.write(COMMITTED, &committed)?;
                    self.record_ownership(&journal.plan, &committed)?;
                    if let Some(t) = &journal.plan.handoff {
                        crate::handoff::finish(self, t)?;
                    }
                    crate::backend::adapter(journal.plan.candidate.backend)
                        .commit(&self.host, &journal.plan)
                        .map_err(|message| {
                            ConfirmationFailure::new(
                                ConfirmationError::ApplyFailed {
                                    domain: "network".into(),
                                    step: "commit_cleanup".into(),
                                    reason: "backend_cleanup_failed".into(),
                                },
                                message,
                            )
                        })?;
                    self.store.remove(JOURNAL)?;
                    self.stop_timers(&journal.job_id);
                }
                Command::Revert { token } => {
                    self.matching(&token)?;
                    let _lock = self.lock_for_revert(REVERT_LOCK_WAIT)?;
                    let journal = self.matching(&token)?;
                    self.recover_locked(Some(&journal.job_id))
                        .map_err(recovery_failure)?;
                }
                Command::PreviewDocument { .. } | Command::ApplyDocument { .. } => {
                    unreachable!("normalized semantic command")
                }
            }
            if receiving && self.store.read::<Journal>(JOURNAL)?.is_none() && self.host.external() {
                reply.externally_managed = true;
                return Ok(());
            }
            reply.profiles = Some(self.current()?);
            if let Some(profiles) = &reply.profiles {
                if receiving {
                    reply.drift = self
                        .store
                        .read::<Journal>(JOURNAL)?
                        .map_or_else(Vec::new, |j| j.plan.drift);
                } else {
                    reply.drift = self.host.drift(profiles)?;
                }
            }
            if let Some(journal) = self.store.read::<Journal>(JOURNAL)? {
                reply.confirm_policy = journal.plan.confirm_policy;
                reply.drift = journal.plan.drift.clone();
                reply.pending = Some(Pending {
                    phase: public_phase(&journal.phase).into(),
                    seconds: (journal.deadline - uptime()?).max(0.0).ceil() as u64,
                    management_address: match &journal
                        .plan
                        .candidate
                        .assigned(&journal.plan.candidate.management)?
                        .ipv4
                    {
                        Ipv4::Static { address, .. } => Some(*address),
                        Ipv4::Dhcp if public_phase(&journal.phase) == "applied" => self
                            .host
                            .address(&journal.plan.candidate, &journal.plan.candidate.management)
                            .ok()
                            .flatten(),
                        Ipv4::Dhcp => None,
                    },
                    management_port: journal.plan.web_port,
                    error: journal.error,
                    recovery_pending: public_phase(&journal.phase) == "failed",
                    recovery_error: journal.recovery_error,
                });
            }
            Ok(())
        })();
        if req.api == NETWORK_DOCUMENT_API {
            reply.document = reply
                .profiles
                .take()
                .map(|p| Document::from_profiles(p, reply.confirm_policy));
        } else if reply
            .profiles
            .as_ref()
            .is_some_and(|p| p.revision > LEGACY_MAX_REVISION)
        {
            reply.profiles = None;
            if result.is_ok() {
                reply.error = Some("This revision requires networking API 2.".into());
                reply.error_kind = Some(ConfirmationError::Invalid {
                    field: "revision".into(),
                    reason: "revision_requires_api_2".into(),
                });
            }
        }
        if let Err(e) = result {
            reply.error = Some(e.message);
            reply.error_kind = e.kind;
        }
        reply
    }

    pub(crate) fn stage(&self, mut plan: Plan) -> std::result::Result<String, ConfirmationFailure> {
        let job_id = token()?;
        let token = token()?;
        if let Some(transfer) = &mut plan.handoff {
            transfer.transaction = job_id.clone();
        }
        let journal = Journal {
            job_id: job_id.clone(),
            token: token.clone(),
            plan,
            phase: "staged".into(),
            deadline: uptime()? + GUARD_SECONDS as f64,
            boot_id: boot_id()?,
            error: None,
            recovery_error: None,
        };
        self.store.write(JOURNAL, &journal)?;
        // Arming failure happens before any configuration mutation.
        if let Err(e) = self.arm(&job_id, "guard", GUARD_SECONDS) {
            if let Some(transfer) = &journal.plan.handoff {
                crate::handoff::release_reservation(&self.host, transfer)?;
            }
            self.store.remove(JOURNAL)?;
            self.stop_timers(&job_id);
            return Err(e.into());
        }
        if let Some(transfer) = &journal.plan.handoff {
            if let Err(error) = crate::handoff::reserve(&self.host, transfer) {
                // Keep the armed journal so independent recovery can retry.
                return Err(error.into());
            }
        }
        let unit = format!("tessera-network-apply-{job_id}");
        let delay = format!("--on-active={APPLY_DELAY_SECONDS}s");
        if let Err(e) = self.host.run(
            "/usr/bin/systemd-run",
            &[
                "--quiet",
                "--collect",
                "--unit",
                &unit,
                &delay,
                "--timer-property=AccuracySec=100ms",
                "--timer-property=RandomizedDelaySec=0",
                "--property=RuntimeMaxSec=90s",
                "--property=TimeoutStopSec=2s",
                "--property=CapabilityBoundingSet=~CAP_SYS_TIME",
                "--property=NoNewPrivileges=yes",
                "--property=ProtectClock=yes",
                "--",
                "/usr/bin/tessera-network",
                "apply",
                &job_id,
            ],
        ) {
            if let Some(transfer) = &journal.plan.handoff {
                crate::handoff::release_reservation(&self.host, transfer)?;
            }
            self.store.remove(JOURNAL)?;
            self.stop_timers(&job_id);
            return Err(e.into());
        }
        Ok(token)
    }

    fn check_document_policy(
        requested: Option<ConfirmPolicy>,
        effective: ConfirmPolicy,
    ) -> std::result::Result<(), ConfirmationFailure> {
        if requested.is_some_and(|policy| policy != effective) {
            return Err(ConfirmationFailure::new(
                ConfirmationError::Invalid {
                    field: "policies.confirm".into(),
                    reason: "administrator_policy_changed".into(),
                },
                "Reload the effective administrator confirmation policy.",
            ));
        }
        Ok(())
    }

    fn legacy_revision(
        api: u32,
        previous: &Profiles,
        candidate: &Profiles,
    ) -> std::result::Result<(), ConfirmationFailure> {
        if api == NETWORK_API
            && (candidate.revision > LEGACY_MAX_REVISION
                || previous.revision > LEGACY_MAX_REVISION
                || (candidate != previous && previous.revision == LEGACY_MAX_REVISION))
        {
            return Err(ConfirmationFailure::new(
                ConfirmationError::Invalid {
                    field: "revision".into(),
                    reason: "revision_requires_api_2".into(),
                },
                "This revision requires networking API 2.",
            ));
        }
        Ok(())
    }

    fn record_ownership(&self, plan: &Plan, committed: &Profiles) -> Result<()> {
        if plan.candidate.backend == tessera_state::network_profiles::Backend::NetworkManager {
            self.store.write(
                "backend.json",
                &crate::networkmanager::committed_ownership(plan, committed.clone()),
            )?;
        }
        Ok(())
    }

    pub(crate) fn ensure_idle(&self) -> std::result::Result<(), ConfirmationFailure> {
        if self.store.read::<Journal>(JOURNAL)?.is_some() {
            return Err(ConfirmationFailure::new(
                ConfirmationError::Locked {
                    field: "network".into(),
                    by: "pending_transaction".into(),
                },
                "A network change still needs confirmation or recovery.",
            ));
        }
        Ok(())
    }

    fn ensure_not_quiesced(&self) -> std::result::Result<(), ConfirmationFailure> {
        if self
            .store
            .read_existing::<crate::handoff::Quiesce>("quiesce.json")?
            .is_some()
        {
            return Err(ConfirmationFailure::new(
                ConfirmationError::Locked {
                    field: "network".into(),
                    by: "handoff".into(),
                },
                "Network ownership handoff is prepared.",
            ));
        }
        Ok(())
    }

    fn matching(&self, token: &str) -> std::result::Result<Journal, ConfirmationFailure> {
        if !valid_token(token) {
            return Err(ConfirmationFailure::new(
                ConfirmationError::Invalid {
                    field: "token".into(),
                    reason: "invalid_token".into(),
                },
                "Invalid transaction token.",
            ));
        }
        let j = match self.store.read::<Journal>(JOURNAL)? {
            Some(j) => j,
            None => {
                if let Some(last) = self.store.read::<LastResult>("last.json")? {
                    if crate::store::token_equal(&last.token, token) {
                        return Err(ConfirmationFailure::new(
                            last.error_kind.unwrap_or(ConfirmationError::NotPending),
                            last.message,
                        ));
                    }
                }
                return Err(ConfirmationFailure::new(
                    ConfirmationError::NotPending,
                    "This network transaction has ended.",
                ));
            }
        };
        if !crate::store::token_equal(&j.token, token) {
            return Err(ConfirmationFailure::new(
                ConfirmationError::TokenMismatch,
                "The confirmation token does not match the pending network change.",
            ));
        }
        Ok(j)
    }

    fn job(&self, id: &str) -> Result<Journal> {
        if !valid_token(id) {
            return Err("Invalid network job identity.".into());
        }
        let j = self
            .store
            .read::<Journal>(JOURNAL)?
            .ok_or("This network job has ended.")?;
        if j.job_id != id {
            return Err("This network job has ended.".into());
        }
        Ok(j)
    }

    /// The host's confirmation policy. An unreadable, invalid or unsafe
    /// web.toml falls back to the strict default: a portal configuration error
    /// must never block status, apply or revert, nor widen who may keep a change.
    fn confirm_policy(&self) -> ConfirmPolicy {
        self.host.confirm_policy().unwrap_or_else(|e| {
            eprintln!("tessera-network: ignoring [network] confirm in web.toml: {e}");
            ConfirmPolicy::RequireManagementAddress
        })
    }

    /// Apply the host policy to a token-authorized transaction. Address checks
    /// use the actual HTTP destination, never a forwarded header.
    pub fn confirmable(
        &self,
        j: &Journal,
        local: Ipv4Addr,
    ) -> std::result::Result<(), ConfirmationFailure> {
        if public_phase(&j.phase) == "failed" {
            return Err(ConfirmationFailure::new(
                ConfirmationError::ApplyFailed {
                    domain: "network".into(),
                    step: "recover".into(),
                    reason: j
                        .recovery_error
                        .clone()
                        .unwrap_or_else(|| "recovery_pending".into()),
                },
                "Network recovery is pending; Keep is unavailable.",
            ));
        }
        if j.boot_id != boot_id()? || uptime()? >= j.deadline {
            return Err(ConfirmationFailure::new(
                ConfirmationError::Expired,
                "The network confirmation window has expired; previous settings are being restored.",
            ));
        }
        if public_phase(&j.phase) != "applied" {
            return Err(still_applying());
        }
        if j.plan.confirm_policy == ConfirmPolicy::TokenOnly {
            return Ok(());
        }
        let expected = self
            .host
            .address(&j.plan.candidate, &j.plan.candidate.management)?;
        if local.is_loopback() || expected != Some(local) {
            return Err(ConfirmationFailure::new(
                ConfirmationError::WrongAddress,
                "Open the new management address directly to keep these settings.",
            ));
        }
        Ok(())
    }

    /// The mutation lock for a Revert. The apply worker holds it for the whole
    /// activation, and a Revert pressed in that window, or just after the
    /// phase turns `applied`, waits up to `wait` for it. If the worker still
    /// holds it, the answer is the retryable "still being applied" refusal
    /// (409), not an internal error.
    pub fn lock_for_revert(
        &self,
        wait: Duration,
    ) -> std::result::Result<Flock<File>, ConfirmationFailure> {
        let give_up = Instant::now() + wait;
        loop {
            match self.store.lock_typed(false) {
                Ok(lock) => return Ok(lock),
                Err(LockError::Busy(_)) if Instant::now() < give_up => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(LockError::Busy(_)) => return Err(still_applying()),
                Err(LockError::Other(message)) => return Err(message.into()),
            }
        }
    }

    fn arm(&self, token: &str, kind: &str, seconds: u64) -> Result<()> {
        let unit = format!("tessera-network-{kind}-{token}");
        let delay = format!("--on-active={seconds}s");
        self.host.run(
            "/usr/bin/systemd-run",
            &[
                "--quiet",
                "--collect",
                "--unit",
                &unit,
                &delay,
                "--timer-property=AccuracySec=100ms",
                "--timer-property=RandomizedDelaySec=0",
                "--property=CapabilityBoundingSet=~CAP_SYS_TIME",
                "--property=NoNewPrivileges=yes",
                "--property=ProtectClock=yes",
                "--",
                "/usr/bin/tessera-network",
                "recover",
                token,
            ],
        )?;
        Ok(())
    }

    fn stop_timers(&self, token: &str) {
        for kind in ["guard", "revert", "apply", "retry"] {
            let unit = format!("tessera-network-{kind}-{token}.timer");
            let _ = self
                .host
                .run("/usr/bin/systemctl", &["stop", "--no-block", &unit]);
        }
    }

    /// Runs in a separate, time-limited systemd job.
    pub fn apply(&self, token: &str) -> Result<()> {
        let result = Self {
            store: self.store.clone(),
            host: self.host.bounded(90),
        }
        .apply_bounded(token);
        if result.is_err() {
            if let Some(j) = self.store.read::<Journal>(JOURNAL)? {
                if j.job_id == token && public_phase(&j.phase) == "failed" {
                    self.arm(token, "retry", RECOVERY_RETRY_SECONDS)?;
                }
            }
        }
        result
    }
    fn apply_bounded(&self, token: &str) -> Result<()> {
        let _lock = self.store.lock(true)?;
        let Some(existing) = self.store.read::<Journal>(JOURNAL)? else {
            return Ok(());
        };
        if existing.job_id != token || !valid_token(token) {
            return Ok(());
        }
        let mut j = self.job(token)?;
        if j.plan.handoff.is_none() {
            self.ensure_not_quiesced().map_err(|e| e.message)?;
            if self.host.external() {
                return Err("NDIMon owns networking; standalone apply refused.".into());
            }
        }
        if j.phase == "applying" {
            // A replay after an interrupted worker must restore its journal.
            // Rechecking partially changed files as a fresh preflight would
            // discard the only before-images and release the source too early.
            return self.recover_locked(Some(token));
        }
        if j.phase != "staged" || j.boot_id != boot_id()? || uptime()? >= j.deadline {
            return Ok(());
        }
        if let Err(error) = self.host.recheck(&j.plan) {
            // No configuration mutation has begun. Do not restore stale
            // snapshots over an administrator's intervening changes.
            self.store.write(
                "last.json",
                &LastResult {
                    token: j.token.clone(),
                    message: format!("Network apply refused before activation: {error}"),
                    error_kind: Some(ConfirmationError::ApplyFailed {
                        domain: "network".into(),
                        step: "preflight".into(),
                        reason: "configuration_changed".into(),
                    }),
                },
            )?;
            if let Some(t) = &j.plan.handoff {
                crate::handoff::release_reservation(&self.host, t)?;
            }
            self.store.remove(JOURNAL)?;
            self.stop_timers(&j.job_id);
            return Err(error);
        }
        let result: Result<()> = (|| {
            if self
                .store
                .read::<Profiles>(COMMITTED)?
                .is_some_and(|p| p != j.plan.previous)
            {
                return Err("Committed network revision changed.".into());
            }
            j.phase = "applying".into();
            self.store.write(JOURNAL, &j)?;
            if j.plan.running {
                self.host
                    .run("/usr/bin/systemctl", &["stop", "tesserad.service"])?;
            }
            if j.plan
                .candidate
                .networks
                .iter()
                .any(|n| n.vlan_id.is_some())
            {
                self.host.run("/usr/sbin/modprobe", &["8021q"])?;
            }
            if let Some(t) = &j.plan.handoff {
                for (service, running) in &t.services {
                    if *running && service != "tesserad.service" {
                        self.host.run("/usr/bin/systemctl", &["stop", service])?;
                    }
                }
                crate::handoff::switch_firewall(&self.host, &j.plan, false)?;
            } else {
                self.host
                    .firewall(&j.plan.candidate, j.plan.web_port, None, false)?;
            }
            crate::backend::adapter(j.plan.candidate.backend).prepare(&self.host, &j.plan)?;
            self.host.write_files(&j.plan, false)?;
            if j.plan.handoff.is_some() {
                self.host.run("/usr/bin/systemctl", &["daemon-reload"])?;
                if j.plan.candidate.backend
                    == tessera_state::network_profiles::Backend::NetworkManager
                {
                    self.host.run("/usr/bin/nmcli", &["connection", "reload"])?;
                }
            }
            self.host.activate(&j.plan, false)?;
            self.host.wait_addresses(&j.plan.candidate, 45)?;
            self.host.verify(&j.plan, false)?;
            if j.plan.running {
                self.host
                    .run("/usr/bin/systemctl", &["restart", "tesserad.service"])?;
            } else {
                self.host
                    .run("/usr/bin/systemctl", &["stop", "tesserad.service"])?;
            }
            j.phase = "applied".into();
            j.deadline = uptime()? + CONFIRM_SECONDS as f64;
            self.store.write(JOURNAL, &j)?;
            self.arm(token, "revert", CONFIRM_SECONDS)
        })();
        if let Err(error) = result {
            j.error = Some(error.clone());
            self.store.write(JOURNAL, &j)?;
            self.recover_locked(Some(token))
                .map_err(|recovery| format!("{error}; recovery: {recovery}"))?;
            return Err(error);
        }
        Ok(())
    }

    /// Boot or timer recovery. Old jobs cannot roll back a newer transaction.
    pub fn recover(&self, token: Option<&str>, expired_only: bool) -> Result<()> {
        let _lock = self.store.lock(true)?;
        if expired_only || token.is_some() {
            let Some(j) = self.store.read::<Journal>(JOURNAL)? else {
                return Ok(());
            };
            if token.is_some_and(|id| id != j.job_id) {
                return Ok(());
            }
            if j.boot_id == boot_id()? && uptime()? < j.deadline {
                if token.is_some() {
                    self.arm(
                        &j.job_id,
                        "revert",
                        (j.deadline - uptime()?).ceil().max(1.0) as u64,
                    )?;
                }
                return Ok(());
            }
        }
        self.recover_locked(token)
    }

    fn recover_locked(&self, token: Option<&str>) -> Result<()> {
        let Some(mut j) = self.store.read::<Journal>(JOURNAL)? else {
            return Ok(());
        };
        if token.is_some_and(|t| !valid_token(t) || t != j.job_id) {
            return Ok(());
        }
        let committed = self.store.read::<Profiles>(COMMITTED)?;
        let expected = j.plan.candidate.revision.checked_add(1).map(|revision| {
            let mut expected = j.plan.candidate.clone();
            expected.revision = revision;
            expected
        });
        if let Some(expected) = expected.filter(|expected| committed.as_ref() == Some(expected)) {
            self.record_ownership(&j.plan, &expected)?;
            if let Some(t) = &j.plan.handoff {
                crate::handoff::finish(self, t)?;
            }
            crate::backend::adapter(j.plan.candidate.backend).commit(&self.host, &j.plan)?;
            self.store.remove(JOURNAL)?;
            self.stop_timers(&j.job_id);
            return Ok(());
        }
        // A retry follows an attempt that did not verify and then started the audio daemon
        // again (see below): it works on the network and no longer takes the audio down.
        let retry = public_phase(&j.phase) == "failed";
        let result: Result<()> = (|| {
            if j.phase == "staged" && j.plan.handoff.is_some() {
                crate::handoff::release_reservation(
                    &self.host,
                    j.plan.handoff.as_ref().ok_or("Missing transfer")?,
                )?;
                self.store.remove(JOURNAL)?;
                self.stop_timers(&j.job_id);
                return Ok(());
            }
            if !retry {
                self.host
                    .run("/usr/bin/systemctl", &["stop", "tesserad.service"])?;
            }
            self.host.restore(&j.plan)?;
            // Before the first commit there may be no public audio lock. Restore
            // the original firewall byte-for-byte, not a guessed default.
            if j.plan.handoff.is_some() {
                self.host.run("/usr/bin/systemctl", &["daemon-reload"])?;
                crate::handoff::switch_firewall(&self.host, &j.plan, true)?;
            } else {
                self.host.firewall(
                    &j.plan.previous,
                    j.plan.web_port,
                    j.plan.old_firewall.as_deref(),
                    true,
                )?;
            }
            let mut management_only = j.plan.previous.clone();
            management_only.audio = management_only.management.clone();
            self.host.wait_addresses(&management_only, 45)?;
            let mut restored = j.plan.clone();
            restored.previous = management_only;
            if j.plan.handoff.is_some() {
                crate::handoff::verify_restored(&self.host, &j.plan)?;
            } else {
                self.host.verify(&restored, true)?;
            }
            if j.plan.running {
                self.host.run(
                    "/usr/bin/systemctl",
                    &["restart", "--no-block", "tesserad.service"],
                )?;
            }
            if let Some(t) = &j.plan.handoff {
                for (service, running) in &t.services {
                    if service == "tesserad.service" {
                        continue;
                    }
                    if *running {
                        self.host
                            .run("/usr/bin/systemctl", &["restart", "--no-block", service])?;
                    } else if self.host.service_active(service)? {
                        self.host.run("/usr/bin/systemctl", &["stop", service])?;
                    }
                }
                crate::handoff::release_reservation(&self.host, t)?;
            }
            let message = j.error.as_ref().map_or_else(
                || "Previous network settings restored.".into(),
                |error| format!("Network change failed; previous settings restored: {error}"),
            );
            self.store.write(
                "last.json",
                &LastResult {
                    token: j.token.clone(),
                    message,
                    error_kind: Some(if j.boot_id != boot_id()? || uptime()? >= j.deadline {
                        ConfirmationError::Expired
                    } else {
                        ConfirmationError::NotPending
                    }),
                },
            )?;
            self.store.remove(JOURNAL)?;
            self.stop_timers(&j.job_id);
            Ok(())
        })();
        if let Err(e) = &result {
            eprintln!("tessera-network: recovery {} did not verify: {e}", j.job_id);
            // The speaker is worth more on whatever network is there than silent: nothing
            // else would start the daemon, the engine and the controller again (HW-3, D14).
            self.resume_services(&j.plan);
            j.phase = "failed".into();
            j.deadline = 0.0;
            j.recovery_error = Some(e.clone());
            self.store.write(JOURNAL, &j)?;
            // A transaction-specific retry is independent of the portal.
            self.arm(&j.job_id, "retry", RECOVERY_RETRY_SECONDS)?;
        }
        result
    }

    /// Start the services a recovery stopped, after it failed. They follow the daemon
    /// (`Wants`, the engine's and the controller's links), so the daemon is enough; best
    /// effort, because the recovery's own error is what is reported.
    fn resume_services(&self, plan: &Plan) {
        let mut units = Vec::new();
        if plan.running {
            units.push("tesserad.service");
        }
        if let Some(transfer) = &plan.handoff {
            units.extend(
                transfer
                    .services
                    .iter()
                    .filter(|(service, running)| **running && *service != "tesserad.service")
                    .map(|(service, _)| service.as_str()),
            );
        }
        for unit in units {
            if let Err(e) = self
                .host
                .run("/usr/bin/systemctl", &["start", "--no-block", unit])
            {
                eprintln!("tessera-network: could not start {unit} after the failed recovery: {e}");
            }
        }
    }

    /// The operator's way out of a recovery that cannot finish (`tessera-network
    /// accept-current`). It keeps the network as it is now: the journal and its timers are
    /// dropped, the firewall is put back as it was before the change, and the audio services
    /// are started. Without `confirm` it only reports what would be accepted.
    pub fn accept_current(&self, confirm: bool) -> Result<String> {
        let _lock = self.store.lock(false)?;
        let Some(j) = self.store.read::<Journal>(JOURNAL)? else {
            return Ok("No network change is pending; nothing to accept.".into());
        };
        if public_phase(&j.phase) != "failed" {
            return Err(format!(
                "The network change is {}, not waiting on a failed recovery; let it finish or revert it from the Network page.",
                public_phase(&j.phase)
            ));
        }
        if j.plan.handoff.is_some() {
            return Err("A network ownership handoff cannot be accepted this way.".into());
        }
        let mut report = format!(
            "Recovery of the network change {} did not verify: {}\n",
            j.job_id,
            j.recovery_error.as_deref().unwrap_or("no reason recorded")
        );
        let differing: Vec<&str> = j
            .plan
            .files
            .iter()
            .filter(|file| {
                !crate::backend::adapter(j.plan.candidate.backend).same_file(
                    &file.path,
                    file.before.as_deref(),
                    self.host.read(&file.path).ok().flatten().as_deref(),
                )
            })
            .map(|file| file.path.as_str())
            .collect();
        if differing.is_empty() {
            report.push_str("All configuration files are as they were before the change.\n");
        } else {
            report.push_str("Configuration files that differ from their saved copy:\n");
            for path in differing {
                report.push_str(&format!("  {path}\n"));
            }
        }
        if !confirm {
            report.push_str(
                "Nothing was changed. To keep the network as it is now, run `tessera-network accept-current --yes`.\n",
            );
            return Ok(report);
        }
        self.host.firewall(
            &j.plan.previous,
            j.plan.web_port,
            j.plan.old_firewall.as_deref(),
            true,
        )?;
        self.store.write(
            "last.json",
            &LastResult {
                token: j.token.clone(),
                message: "The administrator kept the network as it was after a recovery that did not verify.".into(),
                error_kind: Some(ConfirmationError::NotPending),
            },
        )?;
        self.store.remove(JOURNAL)?;
        self.stop_timers(&j.job_id);
        self.resume_services(&j.plan);
        report.push_str("The current network settings are kept; the pending recovery is closed and the audio services are started.\n");
        Ok(report)
    }

    /// On boot reconstruct service isolation before audio/web services start.
    pub fn boot(&self) -> Result<()> {
        self.store.migrate_legacy_lock()?;
        // A return transfer can still have our previous completed release
        // marker before its apply job removes it. Recover our receive journal
        // before that historical marker can fence boot recovery itself.
        if self
            .store
            .read_existing::<Journal>(JOURNAL)?
            .is_some_and(|j| j.plan.handoff.is_some())
        {
            self.recover(None, false)?;
        }
        let release = self.reconcile_handoff().map_err(|e| e.message)?;
        if release.quiesced || release.externally_managed {
            return Ok(());
        }
        self.recover(None, false)?;
        if self.host.external() {
            return Ok(());
        }
        if let Some(p) = self.store.read::<Profiles>(COMMITTED)? {
            let web = self.host.web_config()?;
            let listen: std::net::SocketAddr = web
                .get("listen")
                .and_then(toml::Value::as_str)
                .unwrap_or("127.0.0.1:8090")
                .parse()
                .map_err(|_| "Invalid listen address")?;
            self.host.firewall(&p, listen.port(), None, false)?;
        }
        Ok(())
    }
}

fn uptime() -> Result<f64> {
    fs::read_to_string("/proc/uptime")
        .map_err(|e| e.to_string())?
        .split_whitespace()
        .next()
        .ok_or("Missing monotonic uptime")?
        .parse()
        .map_err(|e: std::num::ParseFloatError| e.to_string())
}

fn boot_id() -> Result<String> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().into())
        .map_err(|e| e.to_string())
}

fn public_phase(phase: &str) -> &str {
    match phase {
        "ready" => "applied",
        "recovery_failed" => "failed",
        phase => phase,
    }
}
