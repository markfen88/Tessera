//! Network-free legacy ARC encoding and receiver-selection decoding.
//!
//! Source: `spec/protocol/control-arc.md` §§1, 2.2, 4.4, 5.1, 6, 9.2, 9.4.
//! Only the evidenced 2.7.41 profile is admitted. A decoded name is a receiver
//! observation, not proof of media, exclusive ownership or atomic revision.
//! Callers own transport, deadlines, correlation with the original request and
//! all admission decisions. This module performs no I/O or allocation.

use core::fmt;

/// Datagram header length (§1).
pub const HEADER_LEN: usize = 10;
/// Start code for the reviewed native legacy profile (§9.2).
pub const LEGACY_START_CODE: u16 = 0x2729;
const RX_LIST: u16 = 0x3000;
const RX_RECORD_LEN: usize = 20;
const SOURCE_OFFSET: usize = 0x34;

/// A bounded wire failure. Unknown result values remain raw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The advertised profile has no reviewed codec here.
    UnsupportedVersion,
    /// A datagram or fixed record is truncated, or has an invalid total length.
    Length,
    /// The caller's destination buffer cannot hold the complete request.
    BufferTooSmall,
    /// Start code, sequence or opcode does not match the original request.
    Correlation,
    /// More pages are required; this is not a complete successful reply.
    MorePages,
    /// A non-success result, without an invented meaning for its numeric value.
    ResultCode(u16),
    /// A string points into records, outside the packet, or has no terminator.
    NameOffset,
    /// A name is empty, contains NUL, or is not UTF-8.
    Name,
    /// An RX id is zero, duplicated, or outside the observed request geometry.
    Channel,
    /// Valid observed geometry has no reviewed subscription-byte encoding.
    UnsupportedGeometry,
    /// A list request must start at a nonzero one-based item index.
    PageIndex,
    /// One source-name offset exists without its other half.
    PartialSource,
    /// This decoder was given another kind of matched reply.
    Opcode,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResultCode(code) => write!(f, "ARC result code {code:#06x}; no retry"),
            other => write!(f, "ARC wire error: {other:?}"),
        }
    }
}

impl core::error::Error for Error {}

/// A source as transmitted by the protocol; no endpoint identity is implied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Source<'a> {
    /// TX channel name.
    pub channel: &'a str,
    /// TX device name.
    pub device: &'a str,
}

/// Explicitly selected reviewed legacy profile, with no dialect fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Legacy(());

impl Legacy {
    /// Select a codec from the target's advertised `arcp_vers` (§9.2).
    pub fn from_advertised_version(version: &str) -> Result<Self, Error> {
        if version == "2.7.41" {
            Ok(Self(()))
        } else {
            Err(Error::UnsupportedVersion)
        }
    }

    /// Start code for this explicitly selected profile, not dialect negotiation.
    pub const fn start_code(self) -> u16 {
        LEGACY_START_CODE
    }

    /// Encode an RX list query with a one-based start and requested item count.
    /// Zero items wanted uses the reviewed receiver-selected page size (§9.2).
    pub fn rx_list_request(
        self,
        sequence: u16,
        first: u16,
        wanted: u16,
        output: &mut [u8],
    ) -> Result<&[u8], Error> {
        if first == 0 {
            return Err(Error::PageIndex);
        }
        let first = first.to_be_bytes();
        let wanted = wanted.to_be_bytes();
        self.request(
            sequence,
            RX_LIST,
            &[0, 1, first[0], first[1], wanted[0], wanted[1]],
            output,
        )
    }

    /// Encode exactly one subscription mutation (§§5.1, 9.4).
    /// Validation finishes before changing the destination buffer.
    pub fn subscription_request<'a>(
        self,
        sequence: u16,
        rx_count: u16,
        rx: u16,
        source: Option<Source<'_>>,
        output: &'a mut [u8],
    ) -> Result<&'a [u8], Error> {
        let length = HEADER_LEN + subscription_body_len(rx_count, rx, source)?;
        let output = output.get_mut(..length).ok_or(Error::BufferTooSmall)?;
        subscription_body(rx_count, rx, source, &mut output[HEADER_LEN..])?;
        header(sequence, 0x3010, output);
        Ok(output)
    }

    /// Write a request header and body into a caller-owned buffer (§1).
    fn request<'a>(
        self,
        sequence: u16,
        opcode: u16,
        body: &[u8],
        output: &'a mut [u8],
    ) -> Result<&'a [u8], Error> {
        let length = HEADER_LEN.checked_add(body.len()).ok_or(Error::Length)?;
        u16::try_from(length).map_err(|_| Error::Length)?;
        let output = output.get_mut(..length).ok_or(Error::BufferTooSmall)?;
        header(sequence, opcode, output);
        output[HEADER_LEN..].copy_from_slice(body);
        Ok(output)
    }

    /// Match a complete successful reply against the original request (§1).
    /// The connected peer/address must also be checked by the transport owner.
    /// Paging and every unknown/failed result are explicit errors (§2.2).
    pub fn reply(self, sequence: u16, opcode: u16, bytes: &[u8]) -> Result<Reply<'_>, Error> {
        let reply = self.reply_page(sequence, opcode, bytes)?;
        if reply.more {
            return Err(Error::MorePages);
        }
        Ok(reply)
    }

    /// Match one successful reply, retaining RX-list continuation (§§2.2, 6).
    /// Callers assemble bounded pages, reject no progress/duplicate identities
    /// across pages and advance the first index by the received record count.
    /// Multiple pages do not establish an atomic or complete-device snapshot.
    pub fn reply_page(self, sequence: u16, opcode: u16, bytes: &[u8]) -> Result<Reply<'_>, Error> {
        if bytes.len() < HEADER_LEN || usize::from(word(bytes, 2)?) != bytes.len() {
            return Err(Error::Length);
        }
        if word(bytes, 0)? != LEGACY_START_CODE
            || word(bytes, 4)? != sequence
            || word(bytes, 6)? != opcode
        {
            return Err(Error::Correlation);
        }
        match word(bytes, 8)? {
            1 => Ok(Reply {
                bytes,
                opcode,
                more: false,
            }),
            0x8112 if opcode == RX_LIST => Ok(Reply {
                bytes,
                opcode,
                more: true,
            }),
            0x8112 => Err(Error::MorePages),
            code => Err(Error::ResultCode(code)),
        }
    }
}

fn header(sequence: u16, opcode: u16, output: &mut [u8]) {
    // Both callers validate the full length and destination before writing.
    for (index, value) in [LEGACY_START_CODE, output.len() as u16, sequence, opcode, 0]
        .into_iter()
        .enumerate()
    {
        output[index * 2..index * 2 + 2].copy_from_slice(&value.to_be_bytes());
    }
}

/// Encode one RX subscription item with captured padding (§§5.1, 9.4).
/// `None` disconnects, setting both source offsets to zero. The RX count is
/// an observed count, not a configured product channel-count limit. Counts
/// above 255 are explicitly unsupported until their wire byte is evidenced.
pub fn subscription_body<'a>(
    rx_count: u16,
    rx: u16,
    source: Option<Source<'_>>,
    output: &'a mut [u8],
) -> Result<&'a [u8], Error> {
    let length = subscription_body_len(rx_count, rx, source)?;
    let output = output.get_mut(..length).ok_or(Error::BufferTooSmall)?;
    output.fill(0);
    output[0] = rx_count as u8;
    output[1] = 1;
    output[2..4].copy_from_slice(&rx.to_be_bytes());
    if let Some(source) = source {
        let device_offset = SOURCE_OFFSET + source.channel.len() + 1;
        output[4..6].copy_from_slice(&(SOURCE_OFFSET as u16).to_be_bytes());
        output[6..8].copy_from_slice(&(device_offset as u16).to_be_bytes());
        let channel_at = SOURCE_OFFSET - HEADER_LEN;
        output[channel_at..channel_at + source.channel.len()]
            .copy_from_slice(source.channel.as_bytes());
        let device_at = device_offset - HEADER_LEN;
        output[device_at..device_at + source.device.len()]
            .copy_from_slice(source.device.as_bytes());
    }
    Ok(output)
}

/// Size the single-item body, validating geometry, names and wire-size bounds.
/// This lets allocating adapters reserve exactly once; the codec needs no heap.
pub fn subscription_body_len(
    rx_count: u16,
    rx: u16,
    source: Option<Source<'_>>,
) -> Result<usize, Error> {
    if rx == 0 || rx > rx_count {
        return Err(Error::Channel);
    }
    if rx_count > u16::from(u8::MAX) {
        return Err(Error::UnsupportedGeometry);
    }
    let mut length = SOURCE_OFFSET - HEADER_LEN;
    if let Some(source) = source {
        for name in [source.channel, source.device] {
            if name.is_empty() || name.as_bytes().contains(&0) {
                return Err(Error::Name);
            }
            length = length
                .checked_add(name.len())
                .and_then(|n| n.checked_add(1))
                .ok_or(Error::Length)?;
        }
    }
    // Packet offsets and the total length both have 16-bit wire fields.
    u16::try_from(HEADER_LEN.checked_add(length).ok_or(Error::Length)?)
        .map_err(|_| Error::Length)?;
    Ok(length)
}

/// A reply whose complete envelope and success/continuation result are checked.
#[derive(Debug)]
pub struct Reply<'a> {
    bytes: &'a [u8],
    opcode: u16,
    more: bool,
}

impl<'a> Reply<'a> {
    /// Validate all legacy RX records before exposing any of them (§4.4).
    /// Unknown status values and the raw slot byte retain their wire meaning.
    pub fn rx_channels(&self) -> Result<RxPage<'a>, Error> {
        if self.opcode != RX_LIST {
            return Err(Error::Opcode);
        }
        let count = usize::from(*self.bytes.get(11).ok_or(Error::Length)?);
        let minimum = 12 + count * RX_RECORD_LEN;
        if self.bytes.len() < minimum {
            return Err(Error::Length);
        }
        let page = RxPage {
            bytes: self.bytes,
            count,
            minimum,
            more: self.more,
        };
        for index in 0..count {
            let record = page.record(index)?;
            if record.id == 0
                || (0..index).any(|before| {
                    word(self.bytes, 12 + before * RX_RECORD_LEN).ok() == Some(record.id)
                })
            {
                return Err(Error::Channel);
            }
        }
        Ok(page)
    }
}

/// An RX page, preserving record order without inventing complete-device scope.
#[derive(Debug)]
pub struct RxPage<'a> {
    bytes: &'a [u8],
    count: usize,
    minimum: usize,
    more: bool,
}

impl<'a> RxPage<'a> {
    /// Whether the receiver says further pages follow this page.
    pub fn has_more(&self) -> bool {
        self.more
    }
    /// Raw slot byte (§6); it may describe reserved space rather than count.
    pub fn slots(&self) -> u8 {
        self.bytes[10]
    }
    /// Number of validated records on this page, not a device-wide count.
    pub fn len(&self) -> usize {
        self.count
    }
    /// Whether this page has no records.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    /// Validated records in wire order, borrowing names from the datagram.
    pub fn records(&self) -> impl ExactSizeIterator<Item = RxChannel<'a>> + '_ {
        (0..self.count).map(|index| {
            self.record(index)
                .expect("all records validated at page construction")
        })
    }
    fn record(&self, index: usize) -> Result<RxChannel<'a>, Error> {
        let at = 12 + index * RX_RECORD_LEN;
        let channel = name(self.bytes, word(self.bytes, at + 6)?, self.minimum)?;
        let device = name(self.bytes, word(self.bytes, at + 8)?, self.minimum)?;
        let source = match (channel, device) {
            (None, None) => None,
            (Some(channel), Some(device)) => Some(Source { channel, device }),
            _ => return Err(Error::PartialSource),
        };
        Ok(RxChannel {
            id: word(self.bytes, at)?,
            source,
            label: name(self.bytes, word(self.bytes, at + 10)?, self.minimum)?,
            raw_status: u32::from_be_bytes(
                self.bytes[at + 12..at + 16]
                    .try_into()
                    .map_err(|_| Error::Length)?,
            ),
        })
    }
}

/// One legacy receiver record (§4.4). Raw status is deliberately not media proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxChannel<'a> {
    /// One-based RX identity, unique within this page.
    pub id: u16,
    /// Reported source-name pair, or no subscription.
    pub source: Option<Source<'a>>,
    /// Receiver channel's reported label, if present.
    pub label: Option<&'a str>,
    /// Uninterpreted four-byte subscription status.
    pub raw_status: u32,
}

fn word(bytes: &[u8], offset: usize) -> Result<u16, Error> {
    let value = bytes.get(offset..offset + 2).ok_or(Error::Length)?;
    Ok(u16::from_be_bytes([value[0], value[1]]))
}

fn name(bytes: &[u8], offset: u16, minimum: usize) -> Result<Option<&str>, Error> {
    if offset == 0 {
        return Ok(None);
    }
    let offset = usize::from(offset);
    if offset < minimum {
        return Err(Error::NameOffset);
    }
    let tail = bytes.get(offset..).ok_or(Error::NameOffset)?;
    let end = tail.iter().position(|b| *b == 0).ok_or(Error::NameOffset)?;
    if end == 0 {
        return Err(Error::Name);
    }
    core::str::from_utf8(&tail[..end])
        .map(Some)
        .map_err(|_| Error::Name)
}
