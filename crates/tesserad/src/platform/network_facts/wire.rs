//! Linux UAPI nlmsghdr/rtattr framing. Native-endian integers belong to this
//! local-kernel transport; IPv4 octets inside attributes are network order.
use super::*;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    pub kind: u16,
    pub payload: Vec<u8>,
}

pub(super) fn request(seq: u32, port: u32, kind: u16, body_size: usize) -> Vec<u8> {
    let mut message = vec![0; 16 + body_size];
    let length = message.len() as u32;
    message[..4].copy_from_slice(&length.to_ne_bytes());
    message[4..6].copy_from_slice(&kind.to_ne_bytes());
    message[6..8].copy_from_slice(&0x301_u16.to_ne_bytes()); // REQUEST|ROOT|MATCH
    message[8..12].copy_from_slice(&seq.to_ne_bytes());
    message[12..16].copy_from_slice(&port.to_ne_bytes());
    if kind != 18 {
        message[16] = 2;
    } // AF_INET; GETLINK uses AF_UNSPEC
    message
}
pub(super) fn word(data: &[u8]) -> io::Result<u32> {
    Ok(u32::from_ne_bytes(
        data.try_into()
            .map_err(|_| invalid("invalid u32 attribute length"))?,
    ))
}
fn short(data: &[u8]) -> io::Result<u16> {
    Ok(u16::from_ne_bytes(
        data.try_into().map_err(|_| invalid("invalid u16 field"))?,
    ))
}
fn aligned(length: usize) -> io::Result<usize> {
    length
        .checked_add(3)
        .map(|n| n & !3)
        .ok_or_else(|| invalid("netlink extent overflow"))
}

#[derive(Debug)]
pub(super) struct Dump {
    seq: u32,
    port: u32,
    kind: u16,
    done: bool,
}
impl Dump {
    pub(super) fn new(seq: u32, port: u32, kind: u16) -> Self {
        Self {
            seq,
            port,
            kind,
            done: false,
        }
    }
    pub(super) fn done(&self) -> bool {
        self.done
    }
    pub(super) fn accept(&mut self, mut bytes: &[u8]) -> io::Result<Vec<Record>> {
        let mut records = Vec::new();
        if bytes.is_empty() {
            return Err(invalid("empty dump frame"));
        }
        while !bytes.is_empty() {
            if self.done || bytes.len() < 16 {
                return Err(invalid("message after dump end or short header"));
            }
            let length = word(&bytes[..4])? as usize;
            if length < 16 || length > bytes.len() {
                return Err(invalid("invalid netlink message length"));
            }
            let extent = aligned(length)?;
            if extent > bytes.len() {
                return Err(invalid("incomplete netlink message alignment"));
            }
            let kind = short(&bytes[4..6])?;
            let flags = short(&bytes[6..8])?;
            if flags & !2 != 0 || flags & 2 == 0 {
                return Err(invalid("interrupted, filtered or unsupported dump flags"));
            }
            if word(&bytes[8..12])? != self.seq || word(&bytes[12..16])? != self.port {
                return Err(invalid("unassociated netlink sequence/port"));
            }
            let payload = &bytes[16..length];
            if kind == 3 {
                // NLMSG_DONE
                if !payload.is_empty() && (payload.len() != 4 || word(payload)? != 0) {
                    return Err(invalid("netlink dump terminal error"));
                }
                self.done = true;
            } else if kind == self.kind {
                records.push(Record {
                    kind,
                    payload: payload.to_vec(),
                });
            } else {
                // ERROR (including unexpected ACK), OVERRUN, NOOP and any
                // unrequested family are never silently treated as completion.
                return Err(invalid("unsupported/error netlink message"));
            }
            bytes = &bytes[extent..];
        }
        Ok(records)
    }
}

pub(super) type Attributes = BTreeMap<u16, Vec<u8>>;
pub(super) fn attributes(mut bytes: &[u8]) -> io::Result<Attributes> {
    let mut result = BTreeMap::new();
    while !bytes.is_empty() {
        if bytes.len() < 4 {
            return Err(invalid("short netlink attribute"));
        }
        let length = usize::from(short(&bytes[..2])?);
        if length < 4 || length > bytes.len() {
            return Err(invalid("invalid attribute extent"));
        }
        let extent = aligned(length)?;
        if extent > bytes.len() {
            return Err(invalid("incomplete attribute alignment"));
        }
        let kind = short(&bytes[2..4])?;
        // Keep raw flags/type and unknown values. Duplicate attributes are not
        // projected by last-value wins, including duplicate selector fields.
        if result.insert(kind, bytes[4..length].to_vec()).is_some() {
            return Err(invalid("duplicate netlink attribute"));
        }
        bytes = &bytes[extent..];
    }
    Ok(result)
}
