//! PROVISIONAL wire-facing values.
//!
//! Every service type name, TXT key, default value and default port used on
//! the wire lives in this one module. The source is
//! `spec/protocol/discovery-mdns.md` (draft 1), which was derived from a single
//! implementation and is tagged `likely` or `guess`, never `confirmed`. When lab
//! captures confirm or correct a value, change it here; no logic elsewhere
//! depends on the literal values.
//!
//! Confidence for each group is given in the doc comment of the group; unless
//! stated otherwise it is `likely`. Items marked `guess` are called out.
//!
//! Open items (UNKNOWN until captures): further service types, the meaning of
//! `dbcp`, `channels`, `router_vers`, `server_vers`, `model`; the two empty TXT
//! strings in the CMC record; TTL values (the reference uses 4500 s for the ARC
//! record; neither backend lets us set a TTL, see the crate docs).

// --- Service types (spec §2) -------------------------------------------------

/// DNS-SD service type of the routing-control endpoint. `likely`.
pub const TYPE_ARC: &str = "_netaudio-arc._udp";
/// DNS-SD service type of the control-and-monitoring endpoint. `likely`.
pub const TYPE_CMC: &str = "_netaudio-cmc._udp";
/// DNS-SD service type describing a transmit channel. `likely`.
pub const TYPE_CHAN: &str = "_netaudio-chan._udp";
/// DNS-SD service type describing a multicast bundle. `likely`.
pub const TYPE_BUND: &str = "_netaudio-bund._udp";
/// DNS domain of all services.
pub const DOMAIN: &str = "local.";

/// Separator between channel/bundle name and device hostname in instance
/// names (`<channel>@<hostname>`). `likely`.
pub const INSTANCE_SEPARATOR: &str = "@";

// --- Default ports (spec §2) --------------------------------------------------

/// Default ARC port. `likely`.
pub const DEFAULT_ARC_PORT: u16 = 4440;
/// Default CMC port. `likely`.
pub const DEFAULT_CMC_PORT: u16 = 8800;
/// Default flow-control port, used in the SRV of chan and bund records. `likely`.
pub const DEFAULT_FLOW_PORT: u16 = 4455;
/// Default UDP port of a multicast bundle (`p.0`). `likely`.
pub const DEFAULT_BUNDLE_PORT: u16 = 4321;

// --- Limits (spec §2) ---------------------------------------------------------

/// A friendly hostname longer than this makes the vendor controller ignore the
/// device. `likely`. Configurable through [`crate::Limits`].
pub const DEFAULT_MAX_HOSTNAME_LEN: usize = 31;

// --- TXT keys: arc (spec §3.1) --------------------------------------------------

/// ARC protocol version. `likely`.
pub const ARC_KEY_ARCP_VERS: &str = "arcp_vers";
/// Minimum ARC protocol version. `likely`.
pub const ARC_KEY_ARCP_MIN: &str = "arcp_min";
/// Routing component version. `guess`.
pub const ARC_KEY_ROUTER_VERS: &str = "router_vers";
/// Free-text router info. `guess`.
pub const ARC_KEY_ROUTER_INFO: &str = "router_info";
/// Manufacturer. `likely`.
pub const KEY_MANUFACTURER: &str = "mf";
/// Model identifier. `guess` for the form of the value.
pub const KEY_MODEL: &str = "model";

/// Default `arcp_vers` value (reference value; real devices differ). `likely`.
pub const ARC_DEFAULT_ARCP_VERS: &str = "2.7.41";
/// Default `arcp_min` value. `likely`.
pub const ARC_DEFAULT_ARCP_MIN: &str = "0.2.4";
/// Default `router_vers` value. `guess`.
pub const ARC_DEFAULT_ROUTER_VERS: &str = "4.0.2";
/// Default `model` value (reference value). `guess`.
pub const DEFAULT_MODEL: &str = "_000000000000000b";

// --- TXT keys: cmc (spec §3.2) --------------------------------------------------

/// 16 lowercase hex digits, the 8-byte device id. `likely`.
pub const CMC_KEY_ID: &str = "id";
/// Decimal u16 process id. `likely`.
pub const CMC_KEY_PROCESS: &str = "process";
/// CMC protocol version. `likely`.
pub const CMC_KEY_CMCP_VERS: &str = "cmcp_vers";
/// Minimum CMC protocol version. `likely`.
pub const CMC_KEY_CMCP_MIN: &str = "cmcp_min";
/// Server component version. `guess`.
pub const CMC_KEY_SERVER_VERS: &str = "server_vers";
/// UNKNOWN capability-like value. `guess`.
pub const CMC_KEY_CHANNELS: &str = "channels";

/// Default `cmcp_vers`. `likely`.
pub const CMC_DEFAULT_CMCP_VERS: &str = "1.2.0";
/// Default `cmcp_min`. `likely`.
pub const CMC_DEFAULT_CMCP_MIN: &str = "1.0.0";
/// Default `server_vers`. `guess`.
pub const CMC_DEFAULT_SERVER_VERS: &str = "4.0.2";
/// Default `channels` value (meaning UNKNOWN). `guess`.
pub const CMC_DEFAULT_CHANNELS: &str = "0x6000004d";

// --- TXT keys: chan (spec §3.3) -------------------------------------------------

/// Record layout version key. `likely`.
pub const CHAN_KEY_TXTVERS: &str = "txtvers";
/// Flow-control request header version/start code. `likely`.
pub const CHAN_KEY_DBCP1: &str = "dbcp1";
/// UNKNOWN second flow-control value. `guess`.
pub const CHAN_KEY_DBCP: &str = "dbcp";
/// 1-based TX channel id. `likely`.
pub const CHAN_KEY_ID: &str = "id";
/// Sample rate in Hz. `likely`.
pub const CHAN_KEY_RATE: &str = "rate";
/// `<bytes per sample> <pcm_type hex>`. `likely` form, `guess` semantics.
pub const CHAN_KEY_PCM: &str = "pcm";
/// Bits per sample. `likely`.
pub const CHAN_KEY_ENC: &str = "enc";
/// Legacy duplicate of `enc`. `likely`.
pub const CHAN_KEY_EN: &str = "en";
/// Minimum receive latency in ns. `likely`.
pub const CHAN_KEY_LATENCY_NS: &str = "latency_ns";
/// `<max>,<min>` frames per packet. `likely`.
pub const CHAN_KEY_FPP: &str = "fpp";
/// Channels per flow the transmitter can pack. `likely`.
pub const CHAN_KEY_NCHAN: &str = "nchan";
/// Bare flag on the factory-name record. `likely`.
pub const CHAN_KEY_DEFAULT: &str = "default";
/// Prefix of the multicast-membership keys: `b.<N>=<position>`. `likely`.
pub const CHAN_KEY_BUNDLE_PREFIX: &str = "b.";

/// Default chan `txtvers`. `likely`.
pub const CHAN_DEFAULT_TXTVERS: &str = "2";
/// Default `dbcp1`. `likely`.
pub const CHAN_DEFAULT_DBCP1: &str = "0x1102";
/// Default `dbcp` (meaning UNKNOWN). `guess`.
pub const CHAN_DEFAULT_DBCP: &str = "0x1004";
/// Default pcm type code (modern devices). `likely` form, `guess` semantics.
pub const CHAN_DEFAULT_PCM_TYPE: &str = "e";

// --- TXT keys: bund (spec §3.4) -------------------------------------------------

/// Record layout version key. `likely`.
pub const BUND_KEY_TXTVERS: &str = "txtvers";
/// Bundle id. `likely`.
pub const BUND_KEY_ID: &str = "id";
/// Channels in the flow. `likely`.
pub const BUND_KEY_NCHAN: &str = "nchan";
/// Minimum receive latency in ns. `likely`.
pub const BUND_KEY_LATENCY_NS: &str = "latency_ns";
/// Frames per packet, a single integer here. `likely`.
pub const BUND_KEY_FPP: &str = "fpp";
/// Sample rate in Hz. `likely`.
pub const BUND_KEY_RATE: &str = "rate";
/// Bits per sample. `likely`.
pub const BUND_KEY_ENC: &str = "enc";
/// Multicast group address (the `.0` suffix semantics are `guess`).
pub const BUND_KEY_ADDR: &str = "a.0";
/// UDP destination port (the `.0` suffix semantics are `guess`).
pub const BUND_KEY_PORT: &str = "p.0";

/// Default bund `txtvers`. `likely`.
pub const BUND_DEFAULT_TXTVERS: &str = "1";

// --- Value shapes (spec section 3) ----------------------------------------------
//
// How numbers and compound values are written. All `likely` unless noted.
// Numbers are plain decimal (the reference parser also accepts `0x` hex).

/// Value of `pcm`: `<bytes per sample> <pcm type>`, e.g. `3 e`. `likely` for the
/// form, `guess` for the meaning of the type code.
#[must_use]
pub fn pcm_value(bytes_per_sample: u8, pcm_type: &str) -> String {
    format!("{bytes_per_sample} {pcm_type}")
}

/// Value of the chan `fpp`: `<max>,<min>`, maximum first. `likely`. (The bund
/// `fpp` is a single integer instead.)
#[must_use]
pub fn fpp_pair_value(max: u16, min: u16) -> String {
    format!("{max},{min}")
}

/// Value of the cmc `id`: the 8-byte device id as 16 lowercase hex digits.
/// `likely`.
#[must_use]
pub fn device_id_value(id: &[u8; 8]) -> String {
    use core::fmt::Write;
    let mut s = String::with_capacity(16);
    for b in id {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Key of the multicast-membership entry of bundle `n`: `b.<n>`. `likely`.
#[must_use]
pub fn bundle_membership_key(n: u32) -> String {
    format!("{CHAN_KEY_BUNDLE_PREFIX}{n}")
}

// --- Reference defaults (spec section 3) ------------------------------------------

/// Reference chan `fpp` maximum. `likely`.
pub const CHAN_DEFAULT_FPP_MAX: u16 = 32;
/// Reference chan `fpp` minimum. `likely`.
pub const CHAN_DEFAULT_FPP_MIN: u16 = 2;
/// Reference chan `nchan` (it uses min(8, channel count)). `likely`.
pub const CHAN_DEFAULT_NCHAN: u16 = 8;
/// Reference bund `fpp`. `likely`.
pub const BUND_DEFAULT_FPP: u16 = 32;
/// Sample rate used in the spec's examples. `likely`.
pub const DEFAULT_RATE_HZ: u32 = 48_000;
/// Bits per sample in the spec's examples (`enc=24`). `likely`.
pub const DEFAULT_BITS: u8 = 24;
/// Bytes per sample in the spec's example (`pcm=3 e`). `likely`.
pub const DEFAULT_BYTES_PER_SAMPLE: u8 = 3;

// --- Invented placeholders (NOT in the spec) --------------------------------------

/// INVENTED: a default `latency_ns`. The spec gives no number (it says the
/// reference reports its receive latency). Callers should always set it.
pub const INVENTED_DEFAULT_LATENCY_NS: u64 = 1_000_000;
/// INVENTED: a default bundle channel count for `Default::default()`.
pub const INVENTED_DEFAULT_BUNDLE_NCHAN: u16 = 2;
/// INVENTED: a default multicast group (administratively scoped range) for
/// `Default::default()`. Callers must set the real group.
pub const INVENTED_DEFAULT_BUNDLE_ADDR: [u8; 4] = [239, 255, 0, 1];
