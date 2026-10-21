//! Builders for the TXT records and instance names of the four service kinds.
//!
//! Layout and key order follow `spec/protocol/discovery-mdns.md` section 3
//! (all `likely`/`guess`). Every key and default comes from
//! [`crate::provisional`]; this module only assembles them. All fields are
//! public plain data; [`Default`] gives the reference values of the spec.

use std::net::Ipv4Addr;

use crate::model::{ServiceKind, ServiceSpec};
use crate::provisional as p;
use crate::txt::{TxtError, TxtRecord};

/// Joins `<name>@<hostname>`, the instance form of chan and bund records.
#[must_use]
pub fn scoped_instance(name: &str, hostname: &str) -> String {
    format!("{name}{}{hostname}", p::INSTANCE_SEPARATOR)
}

/// TXT content of the `arc` record (spec 3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArcAdvert {
    /// `arcp_vers`.
    pub arcp_vers: String,
    /// `arcp_min`.
    pub arcp_min: String,
    /// `router_vers`.
    pub router_vers: String,
    /// `router_info` (omitted if `None`).
    pub router_info: Option<String>,
    /// `mf`, the manufacturer.
    pub manufacturer: String,
    /// `model`.
    pub model: String,
}

impl Default for ArcAdvert {
    fn default() -> Self {
        Self {
            arcp_vers: p::ARC_DEFAULT_ARCP_VERS.into(),
            arcp_min: p::ARC_DEFAULT_ARCP_MIN.into(),
            router_vers: p::ARC_DEFAULT_ROUTER_VERS.into(),
            router_info: None,
            manufacturer: String::new(),
            model: p::DEFAULT_MODEL.into(),
        }
    }
}

impl ArcAdvert {
    /// Builds the TXT record.
    ///
    /// # Errors
    /// [`TxtError`] if a key is invalid (cannot happen with the provisional
    /// constants) or duplicated.
    pub fn txt(&self) -> Result<TxtRecord, TxtError> {
        let mut t = TxtRecord::new();
        t.push(p::ARC_KEY_ARCP_VERS, &self.arcp_vers)?;
        t.push(p::ARC_KEY_ARCP_MIN, &self.arcp_min)?;
        t.push(p::ARC_KEY_ROUTER_VERS, &self.router_vers)?;
        if let Some(i) = &self.router_info {
            t.push(p::ARC_KEY_ROUTER_INFO, i)?;
        }
        t.push(p::KEY_MANUFACTURER, &self.manufacturer)?;
        t.push(p::KEY_MODEL, &self.model)?;
        Ok(t)
    }

    /// The service: instance name is the hostname (`guess`, spec section 2).
    ///
    /// # Errors
    /// As for [`ArcAdvert::txt`].
    pub fn service(&self, hostname: &str, port: u16) -> Result<ServiceSpec, TxtError> {
        Ok(ServiceSpec {
            kind: ServiceKind::Arc,
            instance: hostname.to_owned(),
            port,
            txt: self.txt()?,
        })
    }
}

/// TXT content of the `cmc` record (spec 3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmcAdvert {
    /// `id`: the 8-byte device id, rendered as 16 lowercase hex digits.
    pub id: [u8; 8],
    /// `process`.
    pub process: u16,
    /// `cmcp_vers`.
    pub cmcp_vers: String,
    /// `cmcp_min`.
    pub cmcp_min: String,
    /// `server_vers`.
    pub server_vers: String,
    /// `channels` (meaning UNKNOWN, `guess`).
    pub channels: String,
    /// `mf`.
    pub manufacturer: String,
    /// `model`.
    pub model: String,
}

impl Default for CmcAdvert {
    fn default() -> Self {
        Self {
            id: [0; 8],
            process: 0,
            cmcp_vers: p::CMC_DEFAULT_CMCP_VERS.into(),
            cmcp_min: p::CMC_DEFAULT_CMCP_MIN.into(),
            server_vers: p::CMC_DEFAULT_SERVER_VERS.into(),
            channels: p::CMC_DEFAULT_CHANNELS.into(),
            manufacturer: String::new(),
            model: p::DEFAULT_MODEL.into(),
        }
    }
}

impl CmcAdvert {
    /// Builds the TXT record. The two empty strings of the reference are not
    /// emitted (UNKNOWN whether real devices carry them, spec 3.2).
    ///
    /// # Errors
    /// As for [`ArcAdvert::txt`].
    pub fn txt(&self) -> Result<TxtRecord, TxtError> {
        let mut t = TxtRecord::new();
        t.push(p::CMC_KEY_ID, p::device_id_value(&self.id))?;
        t.push(p::CMC_KEY_PROCESS, self.process.to_string())?;
        t.push(p::CMC_KEY_CMCP_VERS, &self.cmcp_vers)?;
        t.push(p::CMC_KEY_CMCP_MIN, &self.cmcp_min)?;
        t.push(p::CMC_KEY_SERVER_VERS, &self.server_vers)?;
        t.push(p::CMC_KEY_CHANNELS, &self.channels)?;
        t.push(p::KEY_MANUFACTURER, &self.manufacturer)?;
        t.push(p::KEY_MODEL, &self.model)?;
        Ok(t)
    }

    /// The service: instance name is the hostname (`guess`).
    ///
    /// # Errors
    /// As for [`ArcAdvert::txt`].
    pub fn service(&self, hostname: &str, port: u16) -> Result<ServiceSpec, TxtError> {
        Ok(ServiceSpec {
            kind: ServiceKind::Cmc,
            instance: hostname.to_owned(),
            port,
            txt: self.txt()?,
        })
    }
}

/// TXT content of one `chan` record (spec 3.3). A renamed channel needs two
/// records (factory name with `default`, friendly name without).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelAdvert {
    /// The channel name used in the instance (`<name>@<hostname>`).
    pub name: String,
    /// `id`: 1-based TX channel index.
    pub id: u32,
    /// `rate` in Hz.
    pub rate: u32,
    /// First number of `pcm`: bytes per sample.
    pub bytes_per_sample: u8,
    /// Second part of `pcm`: encoding type code in hex (`guess` semantics).
    pub pcm_type: String,
    /// `enc` and `en`: bits per sample.
    pub bits: u8,
    /// `latency_ns`.
    pub latency_ns: u64,
    /// `fpp` maximum (listed first).
    pub fpp_max: u16,
    /// `fpp` minimum.
    pub fpp_min: u16,
    /// `nchan`.
    pub nchan: u16,
    /// True on the record carrying the factory name (adds the `default` flag).
    pub default: bool,
    /// `b.<bundle>=<position>` entries (1-based position) for channels that
    /// are also carried in multicast bundles.
    pub bundles: Vec<(u32, u16)>,
    /// `dbcp1`.
    pub dbcp1: String,
    /// `dbcp` (meaning UNKNOWN).
    pub dbcp: String,
}

impl Default for ChannelAdvert {
    fn default() -> Self {
        Self {
            name: String::new(),
            id: 1,
            rate: p::DEFAULT_RATE_HZ,
            bytes_per_sample: p::DEFAULT_BYTES_PER_SAMPLE,
            pcm_type: p::CHAN_DEFAULT_PCM_TYPE.into(),
            bits: p::DEFAULT_BITS,
            latency_ns: p::INVENTED_DEFAULT_LATENCY_NS,
            fpp_max: p::CHAN_DEFAULT_FPP_MAX,
            fpp_min: p::CHAN_DEFAULT_FPP_MIN,
            nchan: p::CHAN_DEFAULT_NCHAN,
            default: true,
            bundles: Vec::new(),
            dbcp1: p::CHAN_DEFAULT_DBCP1.into(),
            dbcp: p::CHAN_DEFAULT_DBCP.into(),
        }
    }
}

impl ChannelAdvert {
    /// Builds the TXT record.
    ///
    /// # Errors
    /// As for [`ArcAdvert::txt`]; also [`TxtError::DuplicateKey`] if
    /// `bundles` lists a bundle twice.
    pub fn txt(&self) -> Result<TxtRecord, TxtError> {
        let mut t = TxtRecord::new();
        t.push(p::CHAN_KEY_TXTVERS, p::CHAN_DEFAULT_TXTVERS)?;
        t.push(p::CHAN_KEY_DBCP1, &self.dbcp1)?;
        t.push(p::CHAN_KEY_DBCP, &self.dbcp)?;
        t.push(p::CHAN_KEY_ID, self.id.to_string())?;
        t.push(p::CHAN_KEY_RATE, self.rate.to_string())?;
        t.push(
            p::CHAN_KEY_PCM,
            p::pcm_value(self.bytes_per_sample, &self.pcm_type),
        )?;
        t.push(p::CHAN_KEY_ENC, self.bits.to_string())?;
        t.push(p::CHAN_KEY_EN, self.bits.to_string())?;
        t.push(p::CHAN_KEY_LATENCY_NS, self.latency_ns.to_string())?;
        t.push(
            p::CHAN_KEY_FPP,
            p::fpp_pair_value(self.fpp_max, self.fpp_min),
        )?;
        t.push(p::CHAN_KEY_NCHAN, self.nchan.to_string())?;
        if self.default {
            t.push_flag(p::CHAN_KEY_DEFAULT)?;
        }
        for (bundle, pos) in &self.bundles {
            t.push(p::bundle_membership_key(*bundle), pos.to_string())?;
        }
        Ok(t)
    }

    /// The instance name `<name>@<hostname>`.
    #[must_use]
    pub fn instance(&self, hostname: &str) -> String {
        scoped_instance(&self.name, hostname)
    }

    /// The service (SRV port is the flow-control port).
    ///
    /// # Errors
    /// As for [`ChannelAdvert::txt`].
    pub fn service(&self, hostname: &str, flow_port: u16) -> Result<ServiceSpec, TxtError> {
        Ok(ServiceSpec {
            kind: ServiceKind::Chan,
            instance: self.instance(hostname),
            port: flow_port,
            txt: self.txt()?,
        })
    }
}

/// TXT content of one `bund` record (spec 3.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleAdvert {
    /// `id`: the bundle (multicast flow) id; also the instance prefix.
    pub id: u32,
    /// `nchan`.
    pub nchan: u16,
    /// `latency_ns`.
    pub latency_ns: u64,
    /// `fpp`: a single integer here.
    pub fpp: u16,
    /// `rate` in Hz.
    pub rate: u32,
    /// `enc`: bits per sample.
    pub bits: u8,
    /// `a.0`: multicast group.
    pub addr: Ipv4Addr,
    /// `p.0`: destination port.
    pub port: u16,
}

impl Default for BundleAdvert {
    fn default() -> Self {
        Self {
            id: 1,
            nchan: p::INVENTED_DEFAULT_BUNDLE_NCHAN,
            latency_ns: p::INVENTED_DEFAULT_LATENCY_NS,
            fpp: p::BUND_DEFAULT_FPP,
            rate: p::DEFAULT_RATE_HZ,
            bits: p::DEFAULT_BITS,
            addr: Ipv4Addr::from(p::INVENTED_DEFAULT_BUNDLE_ADDR),
            port: p::DEFAULT_BUNDLE_PORT,
        }
    }
}

impl BundleAdvert {
    /// Builds the TXT record.
    ///
    /// # Errors
    /// As for [`ArcAdvert::txt`].
    pub fn txt(&self) -> Result<TxtRecord, TxtError> {
        let mut t = TxtRecord::new();
        t.push(p::BUND_KEY_TXTVERS, p::BUND_DEFAULT_TXTVERS)?;
        t.push(p::BUND_KEY_ID, self.id.to_string())?;
        t.push(p::BUND_KEY_NCHAN, self.nchan.to_string())?;
        t.push(p::BUND_KEY_LATENCY_NS, self.latency_ns.to_string())?;
        t.push(p::BUND_KEY_FPP, self.fpp.to_string())?;
        t.push(p::BUND_KEY_RATE, self.rate.to_string())?;
        t.push(p::BUND_KEY_ENC, self.bits.to_string())?;
        t.push(p::BUND_KEY_ADDR, self.addr.to_string())?;
        t.push(p::BUND_KEY_PORT, self.port.to_string())?;
        Ok(t)
    }

    /// The instance name `<id>@<hostname>`.
    #[must_use]
    pub fn instance(&self, hostname: &str) -> String {
        scoped_instance(&self.id.to_string(), hostname)
    }

    /// The service (SRV port is the flow-control port).
    ///
    /// # Errors
    /// As for [`BundleAdvert::txt`].
    pub fn service(&self, hostname: &str, flow_port: u16) -> Result<ServiceSpec, TxtError> {
        Ok(ServiceSpec {
            kind: ServiceKind::Bund,
            instance: self.instance(hostname),
            port: flow_port,
            txt: self.txt()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::txt::TxtLimits;
    use proptest::prelude::*;

    fn strings(t: &TxtRecord) -> Vec<String> {
        t.to_strings()
            .into_iter()
            .map(|s| String::from_utf8(s).unwrap())
            .collect()
    }

    #[test]
    fn arc_record_matches_spec_table() {
        let a = ArcAdvert {
            router_info: Some("board".into()),
            manufacturer: "Acme".into(),
            ..ArcAdvert::default()
        };
        assert_eq!(
            strings(&a.txt().unwrap()),
            [
                "arcp_vers=2.7.41",
                "arcp_min=0.2.4",
                "router_vers=4.0.2",
                "router_info=board",
                "mf=Acme",
                "model=_000000000000000b"
            ]
        );
        let s = a.service("studio-a", 4440).unwrap();
        assert_eq!(
            (s.kind, s.instance.as_str(), s.port),
            (ServiceKind::Arc, "studio-a", 4440)
        );
    }

    #[test]
    fn cmc_id_is_sixteen_lowercase_hex_digits() {
        let c = CmcAdvert {
            id: [0, 0, 0, 0, 10, 0xAB, 0, 0x2a],
            process: 7,
            ..CmcAdvert::default()
        };
        let t = c.txt().unwrap();
        assert_eq!(t.get_str("id"), Some("000000000aab002a"));
        assert_eq!(t.get_str("process"), Some("7"));
        assert_eq!(t.get_str("cmcp_vers"), Some("1.2.0"));
        assert_eq!(t.get_str("channels"), Some("0x6000004d"));
    }

    #[test]
    fn chan_record_matches_spec_table() {
        let c = ChannelAdvert {
            name: "Mic 1".into(),
            bundles: vec![(3, 2)],
            ..ChannelAdvert::default()
        };
        assert_eq!(
            strings(&c.txt().unwrap()),
            [
                "txtvers=2",
                "dbcp1=0x1102",
                "dbcp=0x1004",
                "id=1",
                "rate=48000",
                "pcm=3 e",
                "enc=24",
                "en=24",
                "latency_ns=1000000",
                "fpp=32,2",
                "nchan=8",
                "default",
                "b.3=2"
            ]
        );
        assert_eq!(c.instance("studio-a"), "Mic 1@studio-a");
        let renamed = ChannelAdvert {
            default: false,
            ..c
        };
        assert!(!renamed.txt().unwrap().contains("default"));
    }

    #[test]
    fn chan_duplicate_bundle_is_an_error() {
        let c = ChannelAdvert {
            bundles: vec![(3, 2), (3, 4)],
            ..ChannelAdvert::default()
        };
        assert!(matches!(c.txt(), Err(TxtError::DuplicateKey(_))));
    }

    #[test]
    fn bund_record_matches_spec_table() {
        let b = BundleAdvert::default();
        assert_eq!(
            strings(&b.txt().unwrap()),
            [
                "txtvers=1",
                "id=1",
                "nchan=2",
                "latency_ns=1000000",
                "fpp=32",
                "rate=48000",
                "enc=24",
                "a.0=239.255.0.1",
                "p.0=4321"
            ]
        );
        assert_eq!(b.instance("studio-a"), "1@studio-a");
    }

    proptest! {
        #[test]
        fn chan_records_stay_within_limits(
            name in "[ -~]{0,40}", id in any::<u32>(), rate in any::<u32>(), lat in any::<u64>(),
            bundles in proptest::collection::vec((any::<u32>(), any::<u16>()), 0..30),
        ) {
            let c = ChannelAdvert { name, id, rate, latency_ns: lat, bundles, ..ChannelAdvert::default() };
            if let Ok(t) = c.txt() {
                // 30 bundles of at most "b.4294967295=65535" fit the defaults.
                prop_assert!(t.validate(&TxtLimits::default()).is_ok());
                let wire = t.encode(&TxtLimits::default()).unwrap();
                let back = TxtRecord::parse(&wire, &TxtLimits::default()).unwrap();
                prop_assert_eq!(back.record, t);
            }
        }
    }
}
