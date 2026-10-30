//! Provisional 0.x routing observations and operations. Types alone grant no authority.
use crate::{Channel, DeviceKey, Direction, SubscriptionState};
use alloc::string::String;
use alloc::vec::Vec;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Current daemon-issued observation reference, not durable device identity.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct EndpointRef {
    pub id: String,
    pub generation: String,
}
impl EndpointRef {
    #[must_use]
    pub fn new(id: impl Into<String>, generation: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            generation: generation.into(),
        }
    }
}

/// Opaque continuation. The daemon validates its length, scope and revision.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize), serde(transparent))]
pub struct RoutingCursor(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct RoutingDevicesQuery {
    pub cursor: Option<RoutingCursor>,
}
impl RoutingDevicesQuery {
    #[must_use]
    pub fn new(cursor: Option<RoutingCursor>) -> Self {
        Self { cursor }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct RoutingChannelsQuery {
    pub endpoint: EndpointRef,
    pub direction: Direction,
    pub cursor: Option<RoutingCursor>,
}
impl RoutingChannelsQuery {
    #[must_use]
    pub fn new(endpoint: EndpointRef, direction: Direction, cursor: Option<RoutingCursor>) -> Self {
        Self {
            endpoint,
            direction,
            cursor,
        }
    }
}

/// Read one current receiver page from a daemon-resolved control endpoint.
/// No address or protocol profile is accepted from a client.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct RoutingReceiverQuery {
    pub endpoint: EndpointRef,
    /// One-based item index. This is a page position, not a channel ID.
    pub first: u16,
}
impl RoutingReceiverQuery {
    #[must_use]
    pub fn new(endpoint: EndpointRef, first: u16) -> Self {
        Self { endpoint, first }
    }
}

/// A receiver-reported source name pair, independent of saved intent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct ReceiverSelection {
    pub tx_device: String,
    pub tx_channel: String,
}
impl ReceiverSelection {
    #[must_use]
    pub fn new(tx_device: impl Into<String>, tx_channel: impl Into<String>) -> Self {
        Self {
            tx_device: tx_device.into(),
            tx_channel: tx_channel.into(),
        }
    }
}

/// Names reported by a receiver, independent of saved intent and media.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct RoutingReceiverChannel {
    pub id: u16,
    pub label: Option<String>,
    pub source: Option<ReceiverSelection>,
    /// Uninterpreted receiver status; not proof of selected-source audio.
    pub raw_status: u32,
    /// Short-lived daemon expectation for an explicit receiver operation.
    /// It grants no permission and is consumed at most once.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub expectation: Option<ReceiverExpectation>,
}
impl RoutingReceiverChannel {
    #[must_use]
    pub fn new(
        id: u16,
        label: Option<String>,
        source: Option<ReceiverSelection>,
        raw_status: u32,
    ) -> Self {
        Self {
            id,
            label,
            source,
            raw_status,
            expectation: None,
        }
    }
}

/// Opaque, boot-scoped receiver observation. Never an external revision or lock.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize), serde(transparent))]
pub struct ReceiverExpectation(pub String);

/// A source chosen from current daemon inventory, without caller-supplied names.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct RoutingSourceRef {
    pub endpoint: EndpointRef,
    pub channel: u32,
}
impl RoutingSourceRef {
    #[must_use]
    pub fn new(endpoint: EndpointRef, channel: u32) -> Self {
        Self { endpoint, channel }
    }
}

/// Connect requires an empty observed RX. Replace/Disconnect name the source
/// being removed so an occupied channel cannot be overwritten accidentally.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum ReceiverChange {
    Connect {
        source: RoutingSourceRef,
    },
    Replace {
        previous: ReceiverSelection,
        source: RoutingSourceRef,
    },
    Disconnect {
        previous: ReceiverSelection,
    },
}

/// Independent operation accounting; none of these states asserts audio.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum ReceiverOperationState {
    Pending,
    ReceiverConfirmed,
    /// Original outcome remains unknown; an administrator accepted fresh names.
    Reconciled {
        original_unknown: String,
    },
    Conflict {
        reason: String,
    },
    Refused {
        reason: String,
    },
    Unknown {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct ReceiverOperationResult {
    pub operation: ReceiverExpectation,
    pub receiver: EndpointRef,
    pub rx_channel: u16,
    pub state: ReceiverOperationState,
}
impl ReceiverOperationResult {
    #[must_use]
    pub fn new(
        operation: ReceiverExpectation,
        receiver: EndpointRef,
        rx_channel: u16,
        state: ReceiverOperationState,
    ) -> Self {
        Self {
            operation,
            receiver,
            rx_channel,
            state,
        }
    }
}

/// Conservative durable phase: permission was persisted, not proof that bytes
/// escaped or that the receiver applied them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum ReceiverOperationPhase {
    DispatchPermitted,
}

/// Durable context for one operation that was permitted to dispatch. This is
/// uncertainty accounting, never a second routing store or retry instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ReceiverOperationContext {
    pub operation: ReceiverExpectation,
    pub revision: u64,
    pub phase: ReceiverOperationPhase,
    pub receiver_identity: String,
    pub device: DeviceKey,
    pub interface: String,
    pub address: String,
    pub rx_channels: u16,
    pub sample_rate: u32,
    pub encoding: u8,
    pub affected_rx: u16,
    pub expected: Vec<ReceiverRecoverySelection>,
    pub desired: Option<ReceiverSelection>,
    pub runtime_incarnation: String,
    pub host_protection_revision: u64,
}

/// One receiver observation. Raw status is not a media or success assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ReceiverRecoverySelection {
    pub rx: u16,
    pub source: Option<ReceiverSelection>,
    pub raw_status: u32,
}

/// Administrator review of a complete receiver read and the existing saved
/// slot/carriers. A review grants no mutation permission. Acceptance re-reads.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ReceiverRecoveryInspection {
    pub context: Option<ReceiverOperationContext>,
    pub legacy_context_unknown: bool,
    pub receiver: EndpointRef,
    pub label: String,
    pub observed: Vec<ReceiverRecoverySelection>,
    pub saved: Vec<ReceiverRecoverySelection>,
    pub configured_carriers: Vec<crate::FlowSpec>,
    pub stored_carriers: Vec<crate::FlowSpec>,
    pub runtime_reports_fenced: bool,
    pub review: Option<ReceiverExpectation>,
    pub unavailable: Option<String>,
}

/// One stored occurrence. A missing origin is protected, never inferred from
/// names, bytes, PIDs or timestamps. Occurrences are scoped to this review.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct RuntimeCarrierRecoveryRow {
    pub occurrence: u32,
    pub carrier: crate::FlowSpec,
    pub origin_runtime: Option<String>,
    pub releasable: bool,
}

/// Explicit review of retired runtime ownership. Release changes only
/// management rights; a later complete report can replace whole carriers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct RuntimeCarrierRecoveryInspection {
    pub current_runtime: String,
    pub protection_revision: u64,
    pub carriers: Vec<RuntimeCarrierRecoveryRow>,
    pub review: Option<ReceiverExpectation>,
    pub unavailable: Option<String>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub configured_carriers: Vec<crate::FlowSpec>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub saved: Vec<ReceiverRecoverySelection>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub next_run_occurrences: Vec<u32>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub next_run_review: Option<ReceiverExpectation>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub next_run_unavailable: Option<String>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub next_run_authorized: bool,
}

/// A single correlated receiver reply. Pages are not an atomic device snapshot.
/// An empty page establishes no receiver-channel inventory or disconnection.
/// This read grants no routing permission or expected-state write token.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct RoutingReceiverPage {
    pub endpoint: EndpointRef,
    pub label: String,
    pub first: u16,
    pub raw_slots: u8,
    pub channels: Vec<RoutingReceiverChannel>,
    pub next_first: Option<u16>,
    /// Why receiver execution is unavailable on this observation.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub operation_unavailable: Option<String>,
    /// This receiver can feed one source channel to several of its inputs.
    /// False when unknown; a client then offers one input per source.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "core::ops::Not::not")
    )]
    pub fan_out: bool,
    /// Another device's access lock, from its lock-state record
    /// (`device-lock.md` §7a): `Some(true)` when it reports itself locked
    /// (no change is offered for it), `None` when not read or not answered.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub access_locked: Option<bool>,
}
impl RoutingReceiverPage {
    #[must_use]
    pub fn new(endpoint: EndpointRef, label: String, first: u16, raw_slots: u8) -> Self {
        Self {
            endpoint,
            label,
            first,
            raw_slots,
            channels: Vec::new(),
            next_first: None,
            operation_unavailable: None,
            fan_out: false,
            access_locked: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum RoutingDiscoveryState {
    Disabled,
    Starting,
    Browsing,
    Failed,
}

/// Completeness is about the available cache, never every device on the LAN.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct RoutingDiscoveryHealth {
    pub state: RoutingDiscoveryState,
    pub dropped: u64,
    pub lost_events: u64,
    /// Lifetime diagnostic count of failed resolutions and session recovery,
    /// including saturated bookkeeping or a creation reusing a retired path.
    /// It does not by itself determine current discovery completeness.
    pub resolve_failures: u64,
    pub rejected_observations: u64,
}
impl RoutingDiscoveryHealth {
    #[must_use]
    pub fn new(state: RoutingDiscoveryState) -> Self {
        Self {
            state,
            dropped: 0,
            lost_events: 0,
            resolve_failures: 0,
            rejected_observations: 0,
        }
    }
}

/// Why a visible endpoint/channel cannot be used by routing execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum RoutingUnavailableReason {
    ExecutionUnavailable,
    InterfaceUnknown,
    InterfaceNotPermitted,
    ReceiverChannelsUnknown,
    AdvertisedChannelsOnly,
    ConflictingMetadata,
    /// No longer reported (2026-10-30): receivers take channels of a device
    /// with manual multicast flows from those flows or by unicast. Kept for
    /// API compatibility.
    BundleUnsupported,
    IncompleteDiscovery,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct RoutingDevice {
    pub endpoint: EndpointRef,
    /// Observed native wire identity, only when unambiguous. Not authentication.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub wire_id: Option<String>,
    pub label: String,
    pub local_device: Option<DeviceKey>,
    pub interface: Option<String>,
    pub rx_channels: Option<u32>,
    pub tx_channels: Option<u32>,
    pub unavailable: Vec<RoutingUnavailableReason>,
}
impl RoutingDevice {
    #[must_use]
    pub fn new(endpoint: EndpointRef, label: impl Into<String>) -> Self {
        Self {
            endpoint,
            wire_id: None,
            label: label.into(),
            local_device: None,
            interface: None,
            rx_channels: None,
            tx_channels: None,
            unavailable: Vec::new(),
        }
    }
}

/// Routing-only requests forwarded by an authenticated local portal gateway.
/// The receiving daemon still owns admission, expectations and execution.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum RoutingPeerRequest {
    Devices(RoutingDevicesQuery),
    Channels(RoutingChannelsQuery),
    Receiver(RoutingReceiverQuery),
    Operation {
        operation: ReceiverExpectation,
    },
    /// Retire an unused submission. Never cancels a dispatched worker or sends ARC.
    Retire {
        operation: ReceiverExpectation,
    },
    Route {
        expectation: ReceiverExpectation,
        change: ReceiverChange,
    },
}

/// A remote u32 ID is never truncated into a local one-based Channel.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct RoutingChannel {
    pub endpoint: EndpointRef,
    pub direction: Direction,
    pub id: u32,
    pub label: String,
    /// Other names actually advertised for this channel, never inferred names.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub aliases: Vec<String>,
    pub local_channel: Option<Channel>,
    pub sample_rate: Option<u32>,
    pub encoding: Option<u8>,
    pub unavailable: Vec<RoutingUnavailableReason>,
}
impl RoutingChannel {
    #[must_use]
    pub fn new(
        endpoint: EndpointRef,
        direction: Direction,
        id: u32,
        label: impl Into<String>,
    ) -> Self {
        Self {
            endpoint,
            direction,
            id,
            label: label.into(),
            aliases: Vec::new(),
            local_channel: None,
            sample_rate: None,
            encoding: None,
            unavailable: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct RoutingDevicePage {
    pub snapshot: String,
    pub discovery: RoutingDiscoveryHealth,
    pub complete: bool,
    pub devices: Vec<RoutingDevice>,
    pub next_cursor: Option<RoutingCursor>,
}
impl RoutingDevicePage {
    #[must_use]
    pub fn new(snapshot: impl Into<String>, discovery: RoutingDiscoveryHealth) -> Self {
        Self {
            snapshot: snapshot.into(),
            discovery,
            complete: false,
            devices: Vec::new(),
            next_cursor: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct RoutingChannelPage {
    pub snapshot: String,
    pub discovery: RoutingDiscoveryHealth,
    pub complete: bool,
    pub channels: Vec<RoutingChannel>,
    pub next_cursor: Option<RoutingCursor>,
}
impl RoutingChannelPage {
    #[must_use]
    pub fn new(snapshot: impl Into<String>, discovery: RoutingDiscoveryHealth) -> Self {
        Self {
            snapshot: snapshot.into(),
            discovery,
            complete: false,
            channels: Vec::new(),
            next_cursor: None,
        }
    }
}

/// Legacy saved intent; it does not itself create a media carrier.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct ReceiverIntent {
    pub tx_device: String,
    pub tx_channel: String,
}
impl ReceiverIntent {
    #[must_use]
    pub fn new(tx_device: impl Into<String>, tx_channel: impl Into<String>) -> Self {
        Self {
            tx_device: tx_device.into(),
            tx_channel: tx_channel.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum ReceiverCarrier {
    None,
    ConfigurationFlow { id: String },
    ControllerFlow { id: String },
}

/// Independent local facts. Media presence never verifies the selected source.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct ReceiverRoute {
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub intent: Option<ReceiverIntent>,
    pub carrier: ReceiverCarrier,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub media: Option<SubscriptionState>,
}
impl ReceiverRoute {
    #[must_use]
    pub fn new(carrier: ReceiverCarrier) -> Self {
        Self {
            intent: None,
            carrier,
            media: None,
        }
    }
}
