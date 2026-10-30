//! Authenticated pre-exec registration and live callback association.
//! Kernel-bound process handles, installed image metadata and the system
//! manager establish the lifetime. Callback content never supplies identity
//! or source-selection ordering. All waits happen outside the state actor.
use std::sync::Arc;
use std::time::{Duration, Instant};

use tessera_control_types::{CommandError, DeviceStatus, NetworkStatus};
use tokio::io::AsyncReadExt;

use crate::platform::connector::{ConnectedProcess, launch};

mod builds;

pub(crate) fn unavailable(reason: &str) -> CommandError {
    CommandError::Unsupported {
        what: format!("runtime integration: {reason}"),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Evidence {
    process: Arc<ConnectedProcess>,
    observation: Arc<launch::Observation>,
    roles: Arc<launch::RoleTable>,
    collected: Instant,
}

impl Evidence {
    pub(crate) async fn collect(
        process: Option<Arc<ConnectedProcess>>,
    ) -> Result<Self, CommandError> {
        let process = process.ok_or_else(|| unavailable("socket process identity unavailable"))?;
        tokio::task::spawn_blocking(move || {
            let roles = Arc::new(
                launch::RoleTable::installed()
                    .map_err(|_| unavailable("installed role metadata unavailable"))?,
            );
            let observation = Arc::new(
                process
                    .role_snapshot(&roles)
                    .map_err(|_| unavailable("process role unavailable"))?,
            );
            Ok(Self {
                process,
                observation,
                roles,
                collected: Instant::now(),
            })
        })
        .await
        .map_err(|_| unavailable("process observation stopped"))?
    }

    fn fresh(&self) -> Result<(), CommandError> {
        if self.collected.elapsed() > Duration::from_secs(3) {
            return Err(unavailable("process observation expired"));
        }
        self.process
            .recheck_role(&self.observation)
            .map_err(|_| unavailable("callback sender changed or exited"))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Manager {
    pid: i32,
    cgroup: String,
    invocation: String,
}

impl Manager {
    fn parse(text: &str) -> Result<Self, CommandError> {
        let mut fields = std::collections::BTreeMap::new();
        for line in text.lines() {
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| unavailable("invalid manager reply"))?;
            if fields.insert(key, value).is_some() {
                return Err(unavailable("duplicate manager field"));
            }
        }
        let get = |key| {
            fields
                .get(key)
                .copied()
                .ok_or_else(|| unavailable("missing manager field"))
        };
        let pid: i32 = get("MainPID")?
            .parse()
            .map_err(|_| unavailable("invalid manager PID"))?;
        let cgroup = get("ControlGroup")?;
        let invocation = get("InvocationID")?;
        if pid <= 0
            || !cgroup.starts_with('/')
            || !cgroup.ends_with("/tessera-controller@main.service")
            || !matches!(get("ActiveState")?, "active" | "activating")
            || get("KillMode")? != "control-group"
            || invocation.len() != 32
            || !invocation.bytes().all(|b| b.is_ascii_hexdigit())
            || invocation.bytes().all(|b| b == b'0')
        {
            return Err(unavailable("unit lifetime or stop policy unavailable"));
        }
        Ok(Self {
            pid,
            cgroup: cgroup.into(),
            invocation: invocation.into(),
        })
    }

    async fn read() -> Result<Self, CommandError> {
        use std::process::Stdio;
        let mut command = tokio::process::Command::new("/usr/bin/systemctl");
        command
            .args([
                "show",
                "tessera-controller@main.service",
                "--no-pager",
                "-p",
                "MainPID",
                "-p",
                "ControlGroup",
                "-p",
                "InvocationID",
                "-p",
                "ActiveState",
                "-p",
                "KillMode",
            ])
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|_| unavailable("cannot query service manager"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| unavailable("manager output unavailable"))?
            .take(16 * 1024 + 1);
        let mut bytes = Vec::new();
        let status = tokio::time::timeout(Duration::from_secs(2), async {
            stdout.read_to_end(&mut bytes).await?;
            child.wait().await
        })
        .await
        .map_err(|_| unavailable("manager query deadline"))?
        .map_err(|_| unavailable("manager query failed"))?;
        if !status.success() || bytes.len() > 16 * 1024 {
            return Err(unavailable("manager reply refused"));
        }
        Self::parse(std::str::from_utf8(&bytes).map_err(|_| unavailable("invalid manager text"))?)
    }
}

#[derive(Debug)]
pub(crate) struct Start {
    pub(crate) device: DeviceStatus,
    pub(crate) network: NetworkStatus,
    pub(crate) wire_device_id: String,
    evidence: Evidence,
    manager: Manager,
}

impl Start {
    pub(crate) async fn prepare(
        device: DeviceStatus,
        network: NetworkStatus,
        wire_device_id: String,
        process: Option<Arc<ConnectedProcess>>,
    ) -> Result<Self, CommandError> {
        let evidence = Evidence::collect(process).await?;
        builds::check(&evidence.roles).await?;
        let manager = Manager::read().await?;
        // Build probes can outlive the short observation TTL. Collect the
        // singleton/launcher facts after those waits, retaining the verified
        // image set across the refresh.
        let fresh = Evidence::collect(Some(evidence.process.clone())).await?;
        if !evidence.roles.same_images(&fresh.roles) {
            return Err(unavailable(
                "runtime integration executables changed during registration",
            ));
        }
        let result = Self {
            device,
            network,
            wire_device_id,
            evidence: fresh,
            manager,
        };
        result.register_check()?;
        Ok(result)
    }

    pub(crate) fn register_check(&self) -> Result<(), CommandError> {
        self.evidence.fresh()?;
        let snapshot = &self.evidence.observation.snapshot;
        if snapshot.image_kind != launch::Image::Launcher
            || !snapshot.observed_subtree_singleton
            || self.evidence.process.pid() != Some(self.manager.pid)
            || snapshot.unified_cgroup != self.manager.cgroup
        {
            return Err(unavailable("launcher is not the singleton unit MainPID"));
        }
        self.evidence
            .process
            .recheck_launcher_singleton(&self.evidence.observation)
            .map_err(|_| unavailable("unit contains surviving or changed processes"))?;
        Ok(())
    }

    pub(crate) fn holder_live(&self) -> Result<(), CommandError> {
        self.evidence
            .process
            .recheck_runtime_holder(&self.evidence.observation, &self.evidence.roles)
            .map_err(|_| unavailable("registered runtime exited or changed binding"))
    }

    pub(crate) fn child(&self, child: &Evidence, role: launch::Image) -> Result<(), CommandError> {
        self.holder_live()?;
        child.fresh()?;
        if !self.evidence.roles.same_images(&child.roles) {
            return Err(unavailable(
                "installed runtime roles changed; restart the runtime",
            ));
        }
        let holder = &self.evidence.observation.snapshot;
        let snapshot = &child.observation.snapshot;
        if snapshot.image_kind != role
            || snapshot.cgroup != holder.cgroup
            || snapshot.unified_cgroup != holder.unified_cgroup
            || !child.observation.parents.iter().any(|p| {
                p.process.key() == holder.process
                    && p.image_kind == launch::Image::Runtime
                    && p.unified_cgroup == holder.unified_cgroup
            })
        {
            return Err(unavailable(
                "callback is not a child of this runtime incarnation",
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Active {
    start: Start,
    run: tessera_state::engine::RuntimeRun,
}

#[derive(Debug, Default)]
pub(crate) struct Authority {
    active: Option<Active>,
}

#[derive(Debug)]
pub(crate) struct ReadJob {
    ticket: tessera_state::engine::RuntimeRead,
    target: crate::routing_receiver::Target,
    channels: u16,
}

pub(crate) struct RecoveryTarget {
    pub target: crate::routing_receiver::Target,
    pub channels: u16,
    pub context: (String, u64, u64),
    pub restore_observed: bool,
}

impl Authority {
    pub(crate) fn operation_context(
        &self,
        engine: &tessera_state::StateEngine,
    ) -> Result<(String, u64), CommandError> {
        // A daemon serving the device role itself is its own receiver
        // authority; its binding incarnation replaces the runtime run.
        if let (None, Some(native)) = (&self.active, crate::native::local()) {
            return Ok((native.incarnation, 0));
        }
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| unavailable("no registered runtime"))?;
        active.start.holder_live()?;
        let generation = engine.runtime_operation_generation(&active.run)?;
        Ok((active.start.manager.invocation.clone(), generation))
    }

    pub(crate) fn recovery_target(
        &self,
        engine: &tessera_state::StateEngine,
    ) -> Result<RecoveryTarget, CommandError> {
        if let (None, Some(native)) = (&self.active, crate::native::local()) {
            return Ok(RecoveryTarget {
                target: crate::routing_receiver::Target {
                    endpoint: tessera_control_types::EndpointRef::new(
                        "native-local",
                        &native.incarnation,
                    ),
                    label: native.name,
                    interface: native.interface,
                    source: native.address,
                    peer: std::net::SocketAddrV4::new(native.address, crate::native::ARC_PORT),
                    profile: tessera_control::arc::Legacy::from_advertised_version("2.7.41")
                        .map_err(|_| unavailable("native ARC profile unavailable"))?,
                },
                channels: native.rx_channels,
                context: (native.incarnation, 0, 0),
                restore_observed: true,
            });
        }
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| unavailable("no registered runtime"))?;
        active.start.holder_live()?;
        let (generation, echo) = engine.runtime_recovery_context(&active.run)?;
        let network = &active.start.network;
        let interface = network
            .primary
            .as_ref()
            .ok_or_else(|| unavailable("no primary interface"))?;
        let source = network
            .interfaces
            .iter()
            .find(|i| &i.name == interface)
            .and_then(|i| i.ipv4.as_ref())
            .and_then(|ip| ip.parse().ok())
            .ok_or_else(|| unavailable("no primary IPv4 address"))?;
        let channels = u16::try_from(active.start.device.rx_channels.len())
            .map_err(|_| unavailable("invalid receiver geometry"))?;
        Ok(RecoveryTarget {
            target: crate::routing_receiver::Target {
                endpoint: tessera_control_types::EndpointRef::new(
                    "runtime-local",
                    &active.start.manager.invocation,
                ),
                label: active.start.device.name.clone(),
                interface: interface.clone(),
                source,
                peer: std::net::SocketAddrV4::new(source, 4440),
                profile: tessera_control::arc::Legacy::from_advertised_version("2.7.41")
                    .map_err(|_| unavailable("native ARC profile unavailable"))?,
            },
            channels,
            context: (active.start.manager.invocation.clone(), generation, echo),
            restore_observed: engine.runtime_restore_observed(&active.run)?,
        })
    }

    pub(crate) fn retire_dead(&mut self, engine: &mut tessera_state::StateEngine) {
        if self
            .active
            .as_ref()
            .is_some_and(|a| a.start.evidence.process.still_live().is_err())
        {
            if let Some(active) = self.active.take() {
                engine.end_runtime_reports(&active.run);
            }
        }
    }

    pub(crate) fn carrier_review(
        &self,
        engine: &tessera_state::StateEngine,
    ) -> Result<tessera_state::engine::RuntimeCarrierReview, CommandError> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| unavailable("no registered runtime"))?;
        active.start.holder_live()?;
        engine.runtime_carrier_review(&active.run)
    }

    pub(crate) fn release_carriers(
        &self,
        engine: &mut tessera_state::StateEngine,
        review: &tessera_state::engine::RuntimeCarrierReview,
    ) -> Result<(), CommandError> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| unavailable("no registered runtime"))?;
        active.start.holder_live()?;
        if active.start.manager.invocation != review.invocation {
            return Err(CommandError::RoutingSnapshotExpired);
        }
        engine.release_runtime_carriers(&active.run, review)
    }

    pub(crate) fn register(
        &mut self,
        engine: &mut tessera_state::StateEngine,
        start: Start,
        daemon_epoch: &str,
    ) -> Result<(), CommandError> {
        start.register_check()?;
        self.retire_dead(engine);
        if self.active.is_some() {
            return Err(unavailable("another registered runtime remains alive"));
        }
        let mut current = engine
            .status()
            .devices
            .into_iter()
            .next()
            .ok_or_else(|| unavailable("no first device"))?;
        let mut expected = start.device.clone();
        current.subscriptions.clear();
        expected.subscriptions.clear();
        if current != expected
            || engine.status().network.as_ref() != Some(&start.network)
            || engine
                .model()
                .devices
                .first()
                .is_none_or(|d| d.wire_device_id.to_hex() != start.wire_device_id)
        {
            return Err(unavailable("launch inputs changed before registration"));
        }
        let run = engine.begin_runtime_reports_for(&start.manager.invocation)?;
        if let Err(error) = engine.claim_runtime_carrier_grant(&run, daemon_epoch) {
            engine.end_runtime_reports(&run);
            return Err(error);
        }
        tracing::info!(invocation = %start.manager.invocation, "runtime: registered pre-exec lifetime");
        self.active = Some(Active { start, run });
        Ok(())
    }

    pub(crate) fn grant_next_run(
        &self,
        engine: &mut tessera_state::StateEngine,
        review: &tessera_state::engine::RuntimeCarrierReview,
        epoch: &str,
        uid: u32,
        id: &str,
    ) -> Result<(), CommandError> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| unavailable("no registered runtime"))?;
        active.start.holder_live()?;
        engine.grant_runtime_carriers_to_next_run(&active.run, review, epoch, uid, id)
    }

    pub(crate) fn report(
        &mut self,
        engine: &mut tessera_state::StateEngine,
        evidence: &Evidence,
        flows: Vec<tessera_control_types::FlowSpec>,
    ) -> Result<tessera_state::Applied, CommandError> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| unavailable("no registered runtime"))?;
        active.start.child(evidence, launch::Image::ReportAdapter)?;
        engine.apply_runtime_report(&active.run, flows)
    }

    pub(crate) fn echo(
        &mut self,
        engine: &mut tessera_state::StateEngine,
        evidence: &Evidence,
    ) -> Result<tessera_state::Applied, CommandError> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| unavailable("no registered runtime"))?;
        active.start.child(evidence, launch::Image::ForwardedCli)?;
        engine.notice_runtime_echo(&active.run)?;
        Ok(tessera_state::Applied::default())
    }

    pub(crate) fn begin_read(
        &mut self,
        engine: &mut tessera_state::StateEngine,
    ) -> Result<Option<ReadJob>, CommandError> {
        let Some(active) = self.active.as_ref() else {
            return Ok(None);
        };
        active.start.holder_live()?;
        let Some(ticket) = engine.begin_runtime_read(&active.run)? else {
            return Ok(None);
        };
        let network = &active.start.network;
        let interface = network
            .primary
            .as_ref()
            .ok_or_else(|| unavailable("no primary interface"))?;
        let source = network
            .interfaces
            .iter()
            .find(|i| &i.name == interface)
            .and_then(|i| i.ipv4.as_ref())
            .and_then(|ip| ip.parse().ok())
            .ok_or_else(|| unavailable("no primary IPv4 address"))?;
        Ok(Some(ReadJob {
            ticket,
            target: crate::routing_receiver::Target {
                endpoint: tessera_control_types::EndpointRef::new(
                    "runtime-local",
                    &active.start.manager.invocation,
                ),
                label: active.start.device.name.clone(),
                interface: interface.clone(),
                source,
                peer: std::net::SocketAddrV4::new(source, 4440),
                profile: tessera_control::arc::Legacy::from_advertised_version("2.7.41")
                    .map_err(|_| unavailable("native ARC profile unavailable"))?,
            },
            channels: u16::try_from(active.start.device.rx_channels.len())
                .map_err(|_| unavailable("invalid receiver geometry"))?,
        }))
    }

    pub(crate) fn finish_read(
        &mut self,
        engine: &mut tessera_state::StateEngine,
        job: ReadJob,
        selections: Option<
            Vec<(
                tessera_control_types::Channel,
                Option<tessera_control_types::ReceiverSelection>,
            )>,
        >,
    ) -> Result<tessera_state::Applied, CommandError> {
        self.active
            .as_ref()
            .ok_or_else(|| unavailable("runtime ended during read"))?
            .start
            .holder_live()?;
        engine.finish_runtime_read(job.ticket, selections)
    }
}

impl ReadJob {
    pub(crate) async fn read(
        &self,
    ) -> Result<
        Vec<(
            tessera_control_types::Channel,
            Option<tessera_control_types::ReceiverSelection>,
        )>,
        CommandError,
    > {
        use tessera_control_types::Reply;
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut first = 1;
        let mut selections = Vec::new();
        let mut budget = 1024 * 1024;
        loop {
            let page = crate::routing_receiver::read(&self.target, first, deadline, budget).await?;
            budget = budget
                .checked_sub(page.wire_size())
                .ok_or_else(|| unavailable("complete receiver read exceeds budget"))?;
            let Reply::RoutingReceiver(page) = page.into_reply(1024 * 1024)? else {
                unreachable!()
            };
            if selections.len().saturating_add(page.channels.len()) > usize::from(self.channels) {
                return Err(unavailable("receiver list exceeds registered geometry"));
            }
            for row in page.channels {
                let channel = tessera_control_types::Channel::new(row.id)
                    .ok_or_else(|| unavailable("invalid receiver ID"))?;
                selections.push((channel, row.source));
            }
            match page.next_first {
                Some(next) if next > first => first = next,
                Some(_) => return Err(unavailable("receiver page made no progress")),
                None => return Ok(selections),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manager_requires_one_live_invocation_and_control_group_stop() {
        let text = "MainPID=42\nControlGroup=/system.slice/tessera-controller@main.service\nInvocationID=0123456789abcdef0123456789abcdef\nActiveState=active\nKillMode=control-group\n";
        assert_eq!(Manager::parse(text).unwrap().pid, 42);
        for bad in [
            text.replace("MainPID=42", "MainPID=0"),
            text.replace("control-group", "process"),
            text.replace("ActiveState=active", "ActiveState=inactive"),
            text.replace(
                "0123456789abcdef0123456789abcdef",
                "00000000000000000000000000000000",
            ),
            format!("{text}MainPID=42\n"),
            text.replace("tessera-controller@main.service", "other.service"),
        ] {
            assert!(Manager::parse(&bad).is_err(), "{bad}");
        }
    }
}
