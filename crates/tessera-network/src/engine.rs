//! Journal before side effects; a committed revision is the durable commit
//! point. Independent system-manager jobs recover after browser/API failure.

use std::fs;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};
use tessera_state::network_profiles::{
    APPLY_DELAY_SECONDS, CONFIRM_SECONDS, Command, GUARD_SECONDS, Ipv4, NETWORK_API, Pending,
    Profiles, Reply, Request,
};

use crate::Result;
use crate::host::{Host, Plan};
use crate::store::{Store, token, valid_token};

const JOURNAL: &str = "pending.json";
const COMMITTED: &str = "committed.json";

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
}

#[derive(Debug, Serialize, Deserialize)]
struct LastResult {
    token: String,
    message: String,
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
    fn current(&self) -> Result<Profiles> {
        if let Some(p) = self.store.read(COMMITTED)? {
            return Ok(p);
        }
        if let Some(j) = self.store.read::<Journal>(JOURNAL)? {
            return Ok(j.plan.previous);
        }
        self.host.adopt()
    }

    /// Queries cannot leak a pending token. The initiator receives it once.
    pub fn handle(&self, req: Request) -> Reply {
        let mut reply = Reply {
            api: NETWORK_API,
            externally_managed: self.host.external(),
            profiles: None,
            pending: None,
            token: None,
            error: None,
        };
        let result: Result<()> = (|| {
            if req.api != NETWORK_API {
                return Err("Unsupported network API version; install matching packages.".into());
            }
            if reply.externally_managed {
                return match req.command { Command::Status {} => Ok(()), _ => Err("NDIMon manages this appliance's networking. Make VLAN changes in NDIMon's Network settings.".into()) };
            }
            match req.command {
                Command::Status {} => (),
                Command::Pending { token } => {
                    self.matching(&token)?;
                }
                Command::Preview { profiles } => {
                    let _lock = self.store.lock(false)?;
                    self.ensure_idle()?;
                    self.host.plan(self.current()?, profiles)?;
                }
                Command::Apply { profiles } => {
                    let _lock = self.store.lock(false)?;
                    self.ensure_idle()?;
                    let previous = self.current()?;
                    let plan = self.host.plan(previous, profiles)?;
                    let job_id = token()?;
                    let token = token()?;
                    let journal = Journal {
                        job_id: job_id.clone(),
                        token: token.clone(),
                        plan,
                        phase: "applying".into(),
                        deadline: uptime()? + GUARD_SECONDS as f64,
                        boot_id: boot_id()?,
                        error: None,
                    };
                    self.store.write(JOURNAL, &journal)?;
                    // Arming failure happens before any configuration mutation.
                    if let Err(e) = self.arm(&job_id, "guard", GUARD_SECONDS) {
                        self.store.remove(JOURNAL)?;
                        return Err(e);
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
                        self.store.remove(JOURNAL)?;
                        self.stop_timers(&job_id);
                        return Err(e);
                    }
                    reply.token = Some(token);
                }
                Command::Confirm {
                    token,
                    local_address,
                } => {
                    let _lock = self.store.lock(false)?;
                    let journal = self.matching(&token)?;
                    self.confirmable(&journal, local_address)?;
                    let mut committed = journal.plan.candidate;
                    committed.revision = committed
                        .revision
                        .checked_add(1)
                        .ok_or("Network revision exhausted")?;
                    // Recovery must never undo this durable commit, even if the
                    // process stops before deleting its journal/timers.
                    self.store.write(COMMITTED, &committed)?;
                    self.store.remove(JOURNAL)?;
                    self.stop_timers(&journal.job_id);
                }
                Command::Revert { token } => {
                    let _lock = self.store.lock(false)?;
                    let journal = self.matching(&token)?;
                    self.recover_locked(Some(&journal.job_id))?;
                }
            }
            reply.profiles = Some(self.current()?);
            if let Some(journal) = self.store.read::<Journal>(JOURNAL)? {
                reply.pending = Some(Pending {
                    phase: journal.phase.clone(),
                    seconds: (journal.deadline - uptime()?).max(0.0).ceil() as u64,
                    management_address: match &journal
                        .plan
                        .candidate
                        .assigned(&journal.plan.candidate.management)?
                        .ipv4
                    {
                        Ipv4::Static { address, .. } => Some(*address),
                        Ipv4::Dhcp if journal.phase == "ready" => self
                            .host
                            .address(&journal.plan.candidate, &journal.plan.candidate.management)
                            .ok()
                            .flatten(),
                        Ipv4::Dhcp => None,
                    },
                    management_port: journal.plan.web_port,
                    error: journal.error,
                });
            }
            Ok(())
        })();
        if let Err(e) = result {
            reply.error = Some(e);
        }
        reply
    }

    fn ensure_idle(&self) -> Result<()> {
        if self.store.read::<Journal>(JOURNAL)?.is_some() {
            return Err("A network change still needs confirmation or recovery.".into());
        }
        Ok(())
    }

    fn matching(&self, token: &str) -> Result<Journal> {
        if !valid_token(token) {
            return Err("Invalid transaction token.".into());
        }
        let j = match self.store.read::<Journal>(JOURNAL)? {
            Some(j) => j,
            None => {
                if let Some(last) = self.store.read::<LastResult>("last.json")? {
                    if last.token == token {
                        return Err(last.message);
                    }
                }
                return Err("This network transaction has ended.".into());
            }
        };
        if j.token != token {
            return Err("This network transaction has ended.".into());
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

    /// Confirmation uses the new management socket IP, never a forwarded header.
    pub fn confirmable(&self, j: &Journal, local: Ipv4Addr) -> Result<()> {
        if j.phase != "ready" || j.boot_id != boot_id()? || uptime()? >= j.deadline {
            return Err("The confirmation window has expired or the change is not ready.".into());
        }
        let expected = self
            .host
            .address(&j.plan.candidate, &j.plan.candidate.management)?;
        if local.is_loopback() || expected != Some(local) {
            return Err("Open the new management address directly to keep these settings.".into());
        }
        Ok(())
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
        for kind in ["guard", "revert", "apply"] {
            let unit = format!("tessera-network-{kind}-{token}.timer");
            let _ = self
                .host
                .run("/usr/bin/systemctl", &["stop", "--no-block", &unit]);
        }
    }

    /// Runs in a separate, time-limited systemd job.
    pub fn apply(&self, token: &str) -> Result<()> {
        let _lock = self.store.lock(true)?;
        if self.host.external() {
            return Err("NDIMon owns networking; standalone apply refused.".into());
        }
        let mut j = self.job(token)?;
        let result: Result<()> = (|| {
            if j.phase != "applying" || j.boot_id != boot_id()? || uptime()? >= j.deadline {
                return Err("Stale network apply.".into());
            }
            if j.plan.running {
                self.host
                    .run("/usr/bin/systemctl", &["stop", "tesserad.service"])?;
            }
            self.host
                .firewall(&j.plan.candidate, j.plan.web_port, None, false)?;
            self.host.write_files(&j.plan, false)?;
            self.host.activate(&j.plan.candidate, &j.plan.previous)?;
            self.host.wait_addresses(&j.plan.candidate, 35)?;
            self.host.verify_addresses(&j.plan.candidate)?;
            if j.plan.running {
                self.host
                    .run("/usr/bin/systemctl", &["restart", "tesserad.service"])?;
            } else {
                self.host
                    .run("/usr/bin/systemctl", &["stop", "tesserad.service"])?;
            }
            self.arm(token, "revert", CONFIRM_SECONDS)?;
            j.phase = "ready".into();
            j.deadline = uptime()? + CONFIRM_SECONDS as f64;
            self.store.write(JOURNAL, &j)
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
        if expired_only {
            let Some(j) = self.store.read::<Journal>(JOURNAL)? else {
                return Ok(());
            };
            if j.boot_id == boot_id()? && uptime()? < j.deadline {
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
        if committed
            .as_ref()
            .is_some_and(|p| p.revision > j.plan.candidate.revision)
        {
            self.store.remove(JOURNAL)?;
            self.stop_timers(&j.job_id);
            return Ok(());
        }
        let result: Result<()> = (|| {
            self.host
                .run("/usr/bin/systemctl", &["stop", "tesserad.service"])?;
            self.host.write_files(&j.plan, true)?;
            self.host.activate(&j.plan.previous, &j.plan.candidate)?;
            // Before the first commit there may be no public audio lock. Restore
            // the original firewall byte-for-byte, not a guessed default.
            self.host.firewall(
                &j.plan.previous,
                j.plan.web_port,
                j.plan.old_firewall.as_deref(),
                true,
            )?;
            let mut management_only = j.plan.previous.clone();
            management_only.audio = management_only.management.clone();
            self.host.wait_addresses(&management_only, 35)?;
            if j.plan.running {
                self.host.run(
                    "/usr/bin/systemctl",
                    &["restart", "--no-block", "tesserad.service"],
                )?;
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
                },
            )?;
            self.store.remove(JOURNAL)?;
            self.stop_timers(&j.job_id);
            Ok(())
        })();
        if let Err(e) = &result {
            j.phase = "recovery_failed".into();
            j.deadline = 0.0;
            j.error = Some(e.clone());
            self.store.write(JOURNAL, &j)?;
        }
        result
    }

    /// On boot reconstruct service isolation before audio/web services start.
    pub fn boot(&self) -> Result<()> {
        self.recover(None, false)?;
        if self.host.external() {
            return Ok(());
        }
        if let Some(p) = self.store.read::<Profiles>(COMMITTED)? {
            let web: toml::Value =
                toml::from_str(&self.host.read("/etc/tessera/web.toml")?.unwrap_or_default())
                    .map_err(|e: toml::de::Error| e.to_string())?;
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
