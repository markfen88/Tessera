//! Lossless bounded projections of the supported Linux UAPI record headers.
//! Unknown attributes remain present. In particular, policy selectors must not
//! disappear into a misleading unconditional (priority, table) rule.
use super::wire::{Attributes, Record, attributes, word};
use super::*;
use std::collections::BTreeSet;
use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Link {
    pub index: u32,
    pub hardware_type: u16,
    pub flags: u32,
    pub change_mask: u32,
    pub name: String,
    pub attributes: Attributes,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Address {
    pub index: u32,
    pub prefix_length: u8,
    pub flags: u32,
    pub scope: u8,
    pub local: Ipv4Addr,
    // On point-to-point links IFA_ADDRESS is the remote address, not IFA_LOCAL.
    pub address: Option<Ipv4Addr>,
    pub broadcast: Option<Ipv4Addr>,
    pub attributes: Attributes,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Route {
    pub destination_length: u8,
    pub source_length: u8,
    pub tos: u8,
    pub table: u32,
    pub protocol: u8,
    pub scope: u8,
    pub kind: u8,
    pub flags: u32,
    pub destination: Ipv4Addr,
    pub source: Option<Ipv4Addr>,
    pub output_index: Option<u32>,
    pub preferred_source: Option<Ipv4Addr>,
    pub gateway: Option<Ipv4Addr>,
    pub attributes: Attributes,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rule {
    pub destination_length: u8,
    pub source_length: u8,
    pub tos: u8,
    pub table: u32,
    pub action: u8,
    pub flags: u32,
    pub reserved: [u8; 2],
    pub priority: u32,
    pub attributes: Attributes,
}
impl Rule {
    /// Merely describes a decoded rule. Even three canonical rules do not
    /// establish current routing, namespace association or TX permission.
    pub(crate) fn unconditional_lookup(&self) -> bool {
        self.destination_length == 0
            && self.source_length == 0
            && self.tos == 0
            && self.action == 1
            && self.flags & !1 == 0
            && self.reserved == [0; 2]
            && self.attributes.iter().all(|(kind, value)| match kind {
                6 | 15 => value.len() == 4, // priority and full table identifier
                21 => value.len() == 1,     // rule origin, not a packet selector
                13 | 14 => word(value).is_ok_and(|n| n == u32::MAX), // disabled suppression
                _ => false,                 // marks, UID/ports, goto, iif/oif, unknown/nested attrs
            })
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NetworkView {
    pub links: Vec<Link>,
    pub addresses: Vec<Address>,
    pub routes: Vec<Route>,
    pub rules: Vec<Rule>,
}
fn ipv4(bytes: &[u8]) -> io::Result<Ipv4Addr> {
    Ok(Ipv4Addr::from(
        <[u8; 4]>::try_from(bytes).map_err(|_| invalid("invalid IPv4 attribute length"))?,
    ))
}
fn optional_ip(attrs: &Attributes, kind: u16) -> io::Result<Option<Ipv4Addr>> {
    attrs.get(&kind).map(|b| ipv4(b)).transpose()
}
fn optional_word(attrs: &Attributes, kind: u16) -> io::Result<Option<u32>> {
    attrs.get(&kind).map(|b| word(b)).transpose()
}
fn family(bytes: &[u8], length: usize) -> io::Result<()> {
    if bytes.len() < length || bytes[0] != 2 {
        return Err(invalid("short or non-IPv4 network record"));
    }
    Ok(())
}
fn prefix(address: Ipv4Addr, length: u8) -> bool {
    length <= 32
        && (u32::from(address) & u32::MAX.checked_shl(32 - u32::from(length)).unwrap_or(0))
            == u32::from(address)
}
impl NetworkView {
    pub(super) fn decode(records: &[Record]) -> io::Result<Self> {
        let mut view = Self::default();
        for record in records {
            let bytes = record.payload.as_slice();
            match record.kind {
                16 => {
                    if bytes.len() < 16 {
                        return Err(invalid("short interface record"));
                    }
                    let index = word(&bytes[4..8])?;
                    if index == 0 || index > i32::MAX as u32 {
                        return Err(invalid("invalid interface index"));
                    }
                    let attrs = attributes(&bytes[16..])?;
                    let name = attrs
                        .get(&3)
                        .ok_or_else(|| invalid("missing interface name"))?;
                    if name.len() < 2
                        || name.len() > 16
                        || name.last() != Some(&0)
                        || name[..name.len() - 1].contains(&0)
                    {
                        return Err(invalid("invalid interface name extent"));
                    }
                    let name = std::str::from_utf8(&name[..name.len() - 1])
                        .map_err(|_| invalid("unsupported non-UTF8 interface name"))?
                        .to_owned();
                    view.links.push(Link {
                        index,
                        hardware_type: u16::from_ne_bytes(
                            bytes[2..4]
                                .try_into()
                                .map_err(|_| invalid("invalid link type"))?,
                        ),
                        flags: word(&bytes[8..12])?,
                        change_mask: word(&bytes[12..16])?,
                        name,
                        attributes: attrs,
                    });
                }
                20 => {
                    family(bytes, 8)?;
                    if bytes[1] > 32 {
                        return Err(invalid("invalid address prefix length"));
                    }
                    let attrs = attributes(&bytes[8..])?;
                    view.addresses.push(Address {
                        index: word(&bytes[4..8])?,
                        prefix_length: bytes[1],
                        flags: optional_word(&attrs, 8)?.unwrap_or(u32::from(bytes[2])),
                        scope: bytes[3],
                        local: optional_ip(&attrs, 2)?
                            .ok_or_else(|| invalid("missing IPv4 local address"))?,
                        address: optional_ip(&attrs, 1)?,
                        broadcast: optional_ip(&attrs, 4)?,
                        attributes: attrs,
                    });
                }
                24 => {
                    family(bytes, 12)?;
                    let attrs = attributes(&bytes[12..])?;
                    let destination = optional_ip(&attrs, 1)?.unwrap_or(Ipv4Addr::UNSPECIFIED);
                    let source = optional_ip(&attrs, 2)?;
                    if (bytes[1] != 0 && !attrs.contains_key(&1))
                        || !prefix(destination, bytes[1])
                        || bytes[2] > 32
                        || (bytes[2] != 0 && source.is_none())
                        || source.is_some_and(|s| !prefix(s, bytes[2]))
                    {
                        return Err(invalid("invalid route prefix"));
                    }
                    view.routes.push(Route {
                        destination_length: bytes[1],
                        source_length: bytes[2],
                        tos: bytes[3],
                        table: optional_word(&attrs, 15)?.unwrap_or(u32::from(bytes[4])),
                        protocol: bytes[5],
                        scope: bytes[6],
                        kind: bytes[7],
                        flags: word(&bytes[8..12])?,
                        destination,
                        source,
                        output_index: optional_word(&attrs, 4)?,
                        preferred_source: optional_ip(&attrs, 7)?,
                        gateway: optional_ip(&attrs, 5)?,
                        attributes: attrs,
                    });
                }
                32 => {
                    family(bytes, 12)?;
                    if bytes[1] > 32 || bytes[2] > 32 {
                        return Err(invalid("invalid rule prefix length"));
                    }
                    let attrs = attributes(&bytes[12..])?;
                    view.rules.push(Rule {
                        destination_length: bytes[1],
                        source_length: bytes[2],
                        tos: bytes[3],
                        table: optional_word(&attrs, 15)?.unwrap_or(u32::from(bytes[4])),
                        action: bytes[7],
                        flags: word(&bytes[8..12])?,
                        reserved: [bytes[5], bytes[6]],
                        priority: optional_word(&attrs, 6)?.unwrap_or(0),
                        attributes: attrs,
                    });
                }
                _ => return Err(invalid("unrequested network record type")),
            }
        }
        let indices: BTreeSet<_> = view.links.iter().map(|link| link.index).collect();
        let names: BTreeSet<_> = view.links.iter().map(|link| &link.name).collect();
        if indices.len() != view.links.len()
            || names.len() != view.links.len()
            || view
                .addresses
                .iter()
                .any(|address| !indices.contains(&address.index))
        {
            return Err(invalid(
                "duplicate interface or address without an observed interface",
            ));
        }
        // Preserve order, duplicates and all table provenance for route/rule
        // ties. No BTreeSet projection may erase an ambiguity before policy.
        Ok(view)
    }
}
