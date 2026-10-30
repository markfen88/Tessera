//! Private runtime-report admission model and default-off diagnostic observer.
//!
//! Identity, ordering and causality remain explicit test premises. The feature
//! observer supplies none of them. No wire type, production admission call site,
//! candidate persistence or runtime proof exists.

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;

use tessera_control_types::{Direction, FlowSpec};

use crate::config::{Config, ControlLock};
use crate::{Environment, Issue};

mod guards;
mod launch;
mod report_domain;
mod transition;
pub(crate) use transition::AcceptanceProbe;
#[cfg(feature = "routing-shadow-test-hooks")]
pub use transition::test_hooks;
mod tx_network;

const MAX_NODES: usize = 1024;
const MAX_CHANNEL_VALUES: usize = 65_536;
// Finite diagnostic/model retention, not a product routing limit.
const MAX_TEXT_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ModelError {
    Capacity,
    GenerationExhausted,
    Invalid(Vec<String>),
}

/// Logical handle, not a PID, credential, wire token or authentication proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Lifetime {
    instance: launch::DaemonInstance,
    serial: u64,
}
const TEST_INSTANCE: launch::DaemonInstance = launch::DaemonInstance(1);
const fn test_lifetime(serial: u64) -> Lifetime {
    Lifetime {
        instance: TEST_INSTANCE,
        serial,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Writer {
    #[default]
    Unknown,
    Foreign,
    Host,
    Runtime(Lifetime),
    // Proposal admission supplies exact recognition, never generic RX rights.
    Portal(Lifetime, u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryStamp {
    flow: FlowSpec,
    writer: Writer,
    generation: u64,
    epoch: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Ledger {
    generation: u64,
    entries: BTreeMap<String, EntryStamp>,
    // Closed by default, including devices introduced by reload. Observation
    // cannot open acquisition; this prototype supplies no unscoped permission.
    acquirable: BTreeMap<String, (u64, u64)>,
    // Explicit lifetime-scoped TX policy. A new verified registration may open
    // this right; observations and receiver actions never do.
    tx_role: Option<TxRole>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TxRole {
    lifetime: Lifetime,
    binding: guards::Binding,
    generation: u64,
    epoch: u64,
    channels: u16,
    network: tx_network::NetworkFacts,
    policy: tx_network::TxPolicy,
    device_multicast: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteDisposition {
    Preview,
    Refused,
    Accepted,
}

impl Ledger {
    fn restored(stored: &[FlowSpec], _config: &Config) -> Self {
        Self {
            entries: stored
                .iter()
                .map(|flow| {
                    (
                        flow.id.clone(),
                        EntryStamp {
                            flow: flow.clone(),
                            writer: Writer::Unknown,
                            generation: 0,
                            epoch: 0,
                        },
                    )
                })
                .collect(),
            ..Self::default()
        }
    }

    /// Record the authority's result, never independently revalidate it. An
    /// accepted byte-identical write fences just as a changed write does.
    fn foreign_replacement(
        &self,
        next: &[FlowSpec],
        disposition: WriteDisposition,
    ) -> Result<Self, ModelError> {
        if disposition != WriteDisposition::Accepted {
            return Ok(self.clone());
        }
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(ModelError::GenerationExhausted)?;
        Ok(Self {
            generation,
            entries: next
                .iter()
                .map(|flow| {
                    (
                        flow.id.clone(),
                        EntryStamp {
                            flow: flow.clone(),
                            writer: Writer::Foreign,
                            generation,
                            epoch: 0,
                        },
                    )
                })
                .collect(),
            acquirable: BTreeMap::new(),
            tx_role: None,
        })
    }

    fn observe(&self, _report: &[FlowSpec]) -> Self {
        self.clone()
    }

    fn owned_by(&self, id: &str, lifetime: Lifetime) -> bool {
        self.entries.get(id).is_some_and(|stamp| {
            stamp.writer == Writer::Runtime(lifetime) && stamp.generation == self.generation
        })
    }

    fn owns_exact(&self, flow: &FlowSpec, lifetime: Lifetime, epoch: u64) -> bool {
        self.owned_by(&flow.id, lifetime)
            && self
                .entries
                .get(&flow.id)
                .is_some_and(|stamp| stamp.flow == *flow && stamp.epoch == epoch)
    }

    fn acquisition_is_fenced(&self, flow: &FlowSpec, epoch: u64) -> bool {
        flow.direction == Direction::Tx
            || self.acquirable.get(&flow.device.0) != Some(&(self.generation, epoch))
    }

    fn tx_allows(&self, flow: &FlowSpec, lifetime: Lifetime, epoch: u64) -> bool {
        self.tx_role.as_ref().is_some_and(|role| {
            flow.direction == Direction::Tx
                && role.lifetime == lifetime
                && role.generation == self.generation
                && role.epoch == epoch
                && role.binding.contains(flow)
                && role.channels > 0
                && flow.channels.iter().all(|c| *c > 0 && *c <= role.channels)
                && address(flow.peer.as_deref()).is_some_and(|peer| {
                    role.network
                        .allows(peer, &role.policy, role.device_multicast)
                })
        })
    }

    fn protected(&self, flow: &FlowSpec) -> bool {
        self.entries
            .get(&flow.id)
            .is_some_and(|stamp| stamp.writer == Writer::Host)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Baseline,
    Observed,
    Stored,
    Configured,
}

/// Canonicalised once. Invalid or unknown geometry cannot prove independence.
#[derive(Debug)]
enum Channels {
    All,
    Selected(Vec<u16>),
}

impl Channels {
    fn of(flow: &FlowSpec, config: &Config) -> Self {
        let Some(device) = config.device(&flow.device.0) else {
            return Self::All;
        };
        if flow.channels.is_empty()
            || flow
                .channels
                .iter()
                .any(|c| *c == 0 || *c > device.rx_channels)
        {
            return Self::All;
        }
        let mut channels = flow.channels.clone();
        channels.sort_unstable();
        channels.dedup();
        Self::Selected(channels)
    }

    fn overlaps(&self, other: &Self) -> bool {
        let (Self::Selected(a), Self::Selected(b)) = (self, other) else {
            return true;
        };
        let (mut i, mut j) = (0, 0);
        while i < a.len() && j < b.len() {
            match a[i].cmp(&b[j]) {
                std::cmp::Ordering::Equal => return true,
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
            }
        }
        false
    }
}

#[derive(Debug)]
struct Node<'a> {
    source: Source,
    index: usize,
    flow: &'a FlowSpec,
    channels: Channels,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Component {
    baseline: Vec<usize>,
    observed: Vec<usize>,
    stored: Vec<usize>,
    configured: Vec<usize>,
    scopes: Vec<usize>,
}

#[derive(Debug, Clone, Copy)]
struct Inputs<'a> {
    baseline: &'a [FlowSpec],
    observed: &'a [FlowSpec],
    stored: &'a [FlowSpec],
}

/// Every retained FlowSpec text field participates in the same checked budget.
fn flow_text_bytes(flow: &FlowSpec) -> Option<usize> {
    [
        flow.id.as_str(),
        flow.device.0.as_str(),
        flow.format.as_str(),
        flow.peer.as_deref().unwrap_or_default(),
        flow.interface.as_deref().unwrap_or_default(),
        flow.local_address.as_deref().unwrap_or_default(),
        flow.rx_keepalive_payload.as_deref().unwrap_or_default(),
    ]
    .into_iter()
    .try_fold(0_usize, |bytes, value| bytes.checked_add(value.len()))
}

fn check_capacity<'a>(sets: impl IntoIterator<Item = &'a [FlowSpec]>) -> Result<(), ModelError> {
    let (mut nodes, mut channels, mut text) = (0_usize, 0_usize, 0_usize);
    for flows in sets {
        nodes = nodes.checked_add(flows.len()).ok_or(ModelError::Capacity)?;
        for flow in flows {
            text = text
                .checked_add(flow_text_bytes(flow).ok_or(ModelError::Capacity)?)
                .ok_or(ModelError::Capacity)?;
            if text > MAX_TEXT_BYTES {
                return Err(ModelError::Capacity);
            }
            channels = channels
                .checked_add(flow.channels.len())
                .ok_or(ModelError::Capacity)?;
        }
        if nodes > MAX_NODES || channels > MAX_CHANNEL_VALUES {
            return Err(ModelError::Capacity);
        }
    }
    Ok(())
}

fn address(value: Option<&str>) -> Option<Ipv4Addr> {
    value.and_then(|v| v.parse().ok())
}

fn possible_listen_overlap(a: &FlowSpec, b: &FlowSpec) -> bool {
    let same_interface =
        a.interface == b.interface || a.interface.is_none() || b.interface.is_none();
    let (aa, ba) = (
        address(a.local_address.as_deref()),
        address(b.local_address.as_deref()),
    );
    a.port == b.port && same_interface && (aa == ba || aa.is_none() || ba.is_none())
}

fn overlaps(a: &Node<'_>, b: &Node<'_>) -> bool {
    let (af, bf) = (a.flow, b.flow);
    af.id == bf.id
        || (af.direction == bf.direction
            && af.port == bf.port
            && address(af.peer.as_deref()) == address(bf.peer.as_deref()))
        || (af.direction == Direction::Rx
            && bf.direction == Direction::Rx
            && (possible_listen_overlap(af, bf)
                || (af.device == bf.device && a.channels.overlaps(&b.channels))))
}

fn configured_specs(config: &Config) -> Result<Vec<FlowSpec>, ModelError> {
    config
        .flows
        .iter()
        .map(crate::flows::to_spec)
        .collect::<Result<_, _>>()
        .map_err(|e| ModelError::Invalid(vec![e]))
}

fn components(input: Inputs<'_>, config: &Config) -> Result<Vec<Component>, ModelError> {
    scoped_components(input, config, &[])
}

/// Scope-only vertices close one complete action even when it targets empty
/// receivers or receivers omitted from both report baselines.
fn scoped_components(
    input: Inputs<'_>,
    config: &Config,
    scopes: &[guards::RxScope],
) -> Result<Vec<Component>, ModelError> {
    if scopes.len() > guards::MAX_GRANTS {
        return Err(ModelError::Capacity);
    }
    let vertices: Vec<_> = scopes
        .iter()
        .map(|s| (s.device.clone(), s.channels.clone()))
        .collect();
    resource_components(input, config, &vertices)
}

/// Resource vertices need no current geometry: continuing portal fences also
/// survive channel-count reductions and removal of their configured device.
fn resource_components(
    input: Inputs<'_>,
    config: &Config,
    vertices: &[(String, BTreeSet<u16>)],
) -> Result<Vec<Component>, ModelError> {
    let configured = configured_specs(config)?;
    check_capacity([input.baseline, input.observed, input.stored, &configured])?;
    let mut nodes = Vec::new();
    for (source, flows) in [
        (Source::Baseline, input.baseline),
        (Source::Observed, input.observed),
        (Source::Stored, input.stored),
        (Source::Configured, configured.as_slice()),
    ] {
        for (index, flow) in flows.iter().enumerate() {
            nodes.push(Node {
                source,
                index,
                flow,
                channels: Channels::of(flow, config),
            });
        }
    }
    if nodes.len() + vertices.len() > MAX_NODES {
        return Err(ModelError::Capacity);
    }
    let touches = |vertex: &(String, BTreeSet<u16>), flow: &FlowSpec| {
        flow.direction == Direction::Rx
            && flow.device.0 == vertex.0
            && (flow.channels.is_empty() || flow.channels.iter().any(|c| vertex.1.contains(c)))
    };
    let connected = |a: usize, b: usize| match (nodes.get(a), nodes.get(b)) {
        (Some(a), Some(b)) => overlaps(a, b),
        (Some(a), None) => touches(&vertices[b - nodes.len()], a.flow),
        (None, Some(b)) => touches(&vertices[a - nodes.len()], b.flow),
        (None, None) => {
            let (a, b) = (&vertices[a - nodes.len()], &vertices[b - nodes.len()]);
            a.0 == b.0 && !a.1.is_disjoint(&b.1)
        }
    };
    let mut remaining: BTreeSet<usize> = (0..nodes.len() + vertices.len()).collect();
    let mut result = Vec::new();
    while let Some(first) = remaining.pop_first() {
        let mut stack = vec![first];
        let mut component = Component::default();
        while let Some(index) = stack.pop() {
            if let Some(node) = nodes.get(index) {
                match node.source {
                    Source::Baseline => component.baseline.push(node.index),
                    Source::Observed => component.observed.push(node.index),
                    Source::Stored => component.stored.push(node.index),
                    Source::Configured => component.configured.push(node.index),
                }
            } else {
                component.scopes.push(index - nodes.len());
            }
            let adjacent: Vec<_> = remaining
                .iter()
                .copied()
                .filter(|b| connected(index, *b))
                .collect();
            for other in adjacent {
                remaining.remove(&other);
                stack.push(other);
            }
        }
        component.baseline.sort_unstable();
        component.observed.sort_unstable();
        component.stored.sort_unstable();
        component.configured.sort_unstable();
        component.scopes.sort_unstable();
        result.push(component);
    }
    Ok(result)
}

fn validation_errors(candidate: &[FlowSpec], config: &Config, env: &Environment) -> Vec<Issue> {
    let converted = match crate::flows::from_specs(candidate, env) {
        Ok(flows) => flows,
        Err(issues) => return issues.into_iter().filter(Issue::is_error).collect(),
    };
    let mut combined = config.clone();
    combined.flows.extend(converted);
    crate::validate::validate(&combined, env)
        .into_iter()
        .filter(Issue::is_error)
        .collect()
}

/// Unauthorized validation preview; admission uses the joined transition.
#[derive(Debug)]
struct Proposal {
    component: Component,
    candidate: Vec<FlowSpec>,
    errors: Vec<Issue>,
}

fn proposals(
    input: Inputs<'_>,
    config: &Config,
    env: &Environment,
) -> Result<Vec<Proposal>, ModelError> {
    components(input, config)?
        .into_iter()
        .map(|component| {
            let mut candidate: Vec<_> = input
                .stored
                .iter()
                .enumerate()
                .filter(|(i, _)| !component.stored.contains(i))
                .map(|(_, f)| f.clone())
                .collect();
            candidate.extend(
                component
                    .observed
                    .iter()
                    .map(|i| input.observed[*i].clone()),
            );
            let errors = validation_errors(&candidate, config, env);
            Ok(Proposal {
                component,
                candidate,
                errors,
            })
        })
        .collect()
}
