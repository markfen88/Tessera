//! Ring parameters, geometry and the static header codec.
//!
//! This module is pure (no shared memory, no unsafe): it turns
//! [`RingParams`] into byte offsets and encodes/decodes the 128-byte static
//! header. The same functions validate a header received from a peer, which
//! makes them directly testable with arbitrary bytes.

use crate::error::LayoutError;

/// `"TSRARING"` as bytes at offset 0.
pub const RING_MAGIC: [u8; 8] = *b"TSRARING";
/// Layout major version. A different major is rejected.
pub const RING_VERSION_MAJOR: u32 = 1;
/// Layout minor version (additive changes only).
pub const RING_VERSION_MINOR: u32 = 0;
/// Size in bytes of the static header (immutable after creation).
pub const STATIC_HEADER_LEN: usize = 128;
/// Size in bytes of the whole header region (static + dynamic lines).
pub const RING_HEADER_LEN: u64 = 512;
/// Smallest ring capacity in frames: one bitmap word.
pub const MIN_CAPACITY_FRAMES: u64 = 64;
/// Bytes of per-channel state before the bitmap.
pub const CHANNEL_STATE_LEN: u32 = 64;

/// Byte offsets of the ring header fields.
pub(crate) mod off {
    pub(crate) const MAGIC: usize = 0x00;
    pub(crate) const VERSION_MAJOR: usize = 0x08;
    pub(crate) const VERSION_MINOR: usize = 0x0C;
    pub(crate) const HEADER_LEN: usize = 0x10;
    pub(crate) const FLAGS: usize = 0x14;
    pub(crate) const TOTAL_LEN: usize = 0x18;
    pub(crate) const DIRECTION: usize = 0x20;
    pub(crate) const FORMAT: usize = 0x24;
    pub(crate) const CHANNELS: usize = 0x28;
    pub(crate) const SAMPLE_RATE: usize = 0x2C;
    pub(crate) const CAPACITY: usize = 0x30;
    pub(crate) const FIRST_CHANNEL: usize = 0x38;
    pub(crate) const CHANNEL_STRIDE: usize = 0x40;
    pub(crate) const CHANNELS_OFFSET: usize = 0x48;
    pub(crate) const BITMAP_OFFSET: usize = 0x50;
    pub(crate) const PLANE_OFFSET: usize = 0x54;
    // Dynamic lines (each on its own 64-byte cache line).
    /// Producer line.
    pub(crate) const EPOCH: usize = 0x80;
    pub(crate) const WRITE_POS: usize = 0x88;
    pub(crate) const LATE_FRAMES: usize = 0x90;
    /// Consumer line.
    pub(crate) const READ_POS: usize = 0xC0;
    pub(crate) const UNWRITTEN_FRAMES: usize = 0xC8;
    pub(crate) const HOLE_FRAMES: usize = 0xD0;
    pub(crate) const OVERRUN_FRAMES: usize = 0xD8;
    /// Owner (daemon) line.
    pub(crate) const EFFECTIVE_LATENCY: usize = 0x100;
}

/// Reserved byte ranges `[from, to)` of the static header (must be zero in
/// minor version 0). `FIELDS` lists every defined field; the tests assert the
/// two never overlap and together cover the header.
pub(crate) const RESERVED: [(usize, usize); 2] = [(0x3C, 0x40), (0x58, 0x80)];

/// Which side produces audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Network to application: the daemon produces, the client consumes.
    Rx,
    /// Application to network: the client produces, the daemon consumes.
    Tx,
}

impl Direction {
    pub(crate) fn to_u32(self) -> u32 {
        match self {
            Direction::Rx => 1,
            Direction::Tx => 2,
        }
    }
    pub(crate) fn from_u32(v: u32) -> Option<Self> {
        match v {
            1 => Some(Direction::Rx),
            2 => Some(Direction::Tx),
            _ => None,
        }
    }
}

/// Sample format of the ring planes (both are 4 bytes per sample).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SampleFormat {
    /// Signed 32-bit integer, left-aligned.
    I32,
    /// IEEE-754 binary32 float.
    F32,
}

impl SampleFormat {
    pub(crate) fn to_u32(self) -> u32 {
        match self {
            SampleFormat::I32 => 1,
            SampleFormat::F32 => 2,
        }
    }
    pub(crate) fn from_u32(v: u32) -> Option<Self> {
        match v {
            1 => Some(SampleFormat::I32),
            2 => Some(SampleFormat::F32),
            _ => None,
        }
    }
}

/// Sanity limits applied to every parameter set received from a peer or
/// requested locally. These are configuration with defaults, not protocol
/// constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest accepted ring mapping in bytes.
    pub max_ring_bytes: u64,
    /// Largest accepted channel count.
    pub max_channels: u32,
    /// Largest accepted sample rate in Hz.
    pub max_sample_rate_hz: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_ring_bytes: 1 << 30,
            max_channels: 1 << 16,
            max_sample_rate_hz: 10_000_000,
        }
    }
}

/// The parameters that define a ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingParams {
    /// Who produces.
    pub direction: Direction,
    /// Sample format of all planes.
    pub format: SampleFormat,
    /// Number of channel planes (>= 1).
    pub channels: u32,
    /// Endpoint channel number of plane 0 (informational).
    pub first_channel: u32,
    /// Sample rate in Hz (informational for the ring, used by clients).
    pub sample_rate_hz: u32,
    /// Frames per channel plane; a power of two >= [`MIN_CAPACITY_FRAMES`].
    pub capacity_frames: u64,
}

/// Byte offsets derived from [`RingParams`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingGeometry {
    /// Size of the whole mapping.
    pub total_len: u64,
    /// Distance between channel blocks.
    pub channel_stride: u64,
    /// Offset of channel block 0.
    pub channels_offset: u64,
    /// Offset of the validity bitmap inside a channel block.
    pub bitmap_offset: u32,
    /// Offset of the sample plane inside a channel block.
    pub plane_offset: u32,
}

impl RingParams {
    /// Capacity rule of ADR 0006/0011: the next power of two at or above
    /// `max_latency + reorder_window + max_client_period` (in frames), and at
    /// least [`MIN_CAPACITY_FRAMES`]. Saturates at `2^63`.
    pub fn capacity_for(max_latency: u64, reorder_window: u64, max_client_period: u64) -> u64 {
        let need = max_latency
            .saturating_add(reorder_window)
            .saturating_add(max_client_period)
            .max(MIN_CAPACITY_FRAMES);
        need.checked_next_power_of_two().unwrap_or(1 << 63)
    }

    /// Validates the parameters against `limits` and derives the geometry.
    /// All arithmetic is checked.
    pub fn validate(&self, limits: &Limits) -> Result<RingGeometry, LayoutError> {
        if self.channels == 0 || self.channels > limits.max_channels {
            return Err(LayoutError("channel count out of range"));
        }
        if self.first_channel.checked_add(self.channels).is_none() {
            return Err(LayoutError("channel range overflows"));
        }
        if self.sample_rate_hz == 0 || self.sample_rate_hz > limits.max_sample_rate_hz {
            return Err(LayoutError("sample rate out of range"));
        }
        let cap = self.capacity_frames;
        if cap < MIN_CAPACITY_FRAMES || !cap.is_power_of_two() {
            return Err(LayoutError("capacity must be a power of two >= 64"));
        }
        // bitmap: cap/8 bytes (a multiple of 8), rounded up to 64.
        let bitmap_bytes = (cap / 8)
            .checked_add(63)
            .ok_or(LayoutError("capacity overflow"))?
            & !63;
        let plane_off = u64::from(CHANNEL_STATE_LEN)
            .checked_add(bitmap_bytes)
            .ok_or(LayoutError("capacity overflow"))?;
        let plane_bytes = cap.checked_mul(4).ok_or(LayoutError("capacity overflow"))?;
        let stride = plane_off
            .checked_add(plane_bytes)
            .ok_or(LayoutError("capacity overflow"))?;
        let total = stride
            .checked_mul(u64::from(self.channels))
            .and_then(|v| v.checked_add(RING_HEADER_LEN))
            .ok_or(LayoutError("ring size overflow"))?;
        if total > limits.max_ring_bytes {
            return Err(LayoutError("ring larger than the configured limit"));
        }
        // The offsets are stored as u32; make sure they fit.
        let plane_offset =
            u32::try_from(plane_off).map_err(|_| LayoutError("plane offset overflow"))?;
        Ok(RingGeometry {
            total_len: total,
            channel_stride: stride,
            channels_offset: RING_HEADER_LEN,
            bitmap_offset: CHANNEL_STATE_LEN,
            plane_offset,
        })
    }
}

fn put_u32(b: &mut [u8; STATIC_HEADER_LEN], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_ne_bytes());
}
fn put_u64(b: &mut [u8; STATIC_HEADER_LEN], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_ne_bytes());
}
fn get_u32(b: &[u8; STATIC_HEADER_LEN], at: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[at..at + 4]);
    u32::from_ne_bytes(a)
}
fn get_u64(b: &[u8; STATIC_HEADER_LEN], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_ne_bytes(a)
}

/// Encodes the static header for `params` (which must validate).
pub fn encode_static_header(
    params: &RingParams,
    limits: &Limits,
) -> Result<[u8; STATIC_HEADER_LEN], LayoutError> {
    let geo = params.validate(limits)?;
    let mut b = [0u8; STATIC_HEADER_LEN];
    b[off::MAGIC..off::MAGIC + 8].copy_from_slice(&RING_MAGIC);
    put_u32(&mut b, off::VERSION_MAJOR, RING_VERSION_MAJOR);
    put_u32(&mut b, off::VERSION_MINOR, RING_VERSION_MINOR);
    put_u32(&mut b, off::HEADER_LEN, RING_HEADER_LEN as u32);
    put_u32(&mut b, off::FLAGS, 0);
    put_u64(&mut b, off::TOTAL_LEN, geo.total_len);
    put_u32(&mut b, off::DIRECTION, params.direction.to_u32());
    put_u32(&mut b, off::FORMAT, params.format.to_u32());
    put_u32(&mut b, off::CHANNELS, params.channels);
    put_u32(&mut b, off::SAMPLE_RATE, params.sample_rate_hz);
    put_u64(&mut b, off::CAPACITY, params.capacity_frames);
    put_u32(&mut b, off::FIRST_CHANNEL, params.first_channel);
    put_u64(&mut b, off::CHANNEL_STRIDE, geo.channel_stride);
    put_u64(&mut b, off::CHANNELS_OFFSET, geo.channels_offset);
    put_u32(&mut b, off::BITMAP_OFFSET, geo.bitmap_offset);
    put_u32(&mut b, off::PLANE_OFFSET, geo.plane_offset);
    Ok(b)
}

/// Decodes and validates a static header received from a peer.
///
/// `actual_len` is the real size of the mapping (from `fstat`); the header's
/// `total_len` must equal it. Every derived offset is recomputed from the
/// parameters and compared with the stored value. Total: never panics.
pub fn decode_static_header(
    b: &[u8; STATIC_HEADER_LEN],
    actual_len: u64,
    limits: &Limits,
) -> Result<(RingParams, RingGeometry), LayoutError> {
    if b[off::MAGIC..off::MAGIC + 8] != RING_MAGIC {
        return Err(LayoutError("bad ring magic"));
    }
    if get_u32(b, off::VERSION_MAJOR) != RING_VERSION_MAJOR {
        return Err(LayoutError("unsupported ring layout version"));
    }
    let minor = get_u32(b, off::VERSION_MINOR);
    if u64::from(get_u32(b, off::HEADER_LEN)) != RING_HEADER_LEN {
        return Err(LayoutError("unexpected header length"));
    }
    // Reserved bytes must be zero for the minor version we implement; a
    // newer minor may use them additively.
    #[allow(clippy::absurd_extreme_comparisons)] // keep the intent when the minor is bumped
    let known_minor = minor <= RING_VERSION_MINOR;
    if known_minor {
        if get_u32(b, off::FLAGS) != 0 {
            return Err(LayoutError("unknown flags"));
        }
        if !RESERVED
            .iter()
            .all(|&(from, to)| b[from..to].iter().all(|&x| x == 0))
        {
            return Err(LayoutError("reserved bytes not zero"));
        }
    }
    let direction =
        Direction::from_u32(get_u32(b, off::DIRECTION)).ok_or(LayoutError("unknown direction"))?;
    let format = SampleFormat::from_u32(get_u32(b, off::FORMAT))
        .ok_or(LayoutError("unknown sample format"))?;
    let params = RingParams {
        direction,
        format,
        channels: get_u32(b, off::CHANNELS),
        first_channel: get_u32(b, off::FIRST_CHANNEL),
        sample_rate_hz: get_u32(b, off::SAMPLE_RATE),
        capacity_frames: get_u64(b, off::CAPACITY),
    };
    let geo = params.validate(limits)?;
    let stored = RingGeometry {
        total_len: get_u64(b, off::TOTAL_LEN),
        channel_stride: get_u64(b, off::CHANNEL_STRIDE),
        channels_offset: get_u64(b, off::CHANNELS_OFFSET),
        bitmap_offset: get_u32(b, off::BITMAP_OFFSET),
        plane_offset: get_u32(b, off::PLANE_OFFSET),
    };
    if stored != geo {
        return Err(LayoutError("stored geometry does not match parameters"));
    }
    if geo.total_len != actual_len {
        return Err(LayoutError("header length does not match mapping size"));
    }
    Ok((params, geo))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> RingParams {
        RingParams {
            direction: Direction::Rx,
            format: SampleFormat::I32,
            channels: 4,
            first_channel: 2,
            sample_rate_hz: 48_000,
            capacity_frames: 4096,
        }
    }

    #[test]
    fn roundtrip() {
        let lim = Limits::default();
        let p = params();
        let geo = p.validate(&lim).unwrap();
        let hdr = encode_static_header(&p, &lim).unwrap();
        let (p2, g2) = decode_static_header(&hdr, geo.total_len, &lim).unwrap();
        assert_eq!(p, p2);
        assert_eq!(geo, g2);
        // 512 header + 4 * (64 + 512 + 16384)
        assert_eq!(geo.total_len, 512 + 4 * (64 + 512 + 16384));
        assert_eq!(geo.channel_stride % 64, 0);
    }

    #[test]
    fn rejects_wrong_length_and_bad_params() {
        let lim = Limits::default();
        let p = params();
        let geo = p.validate(&lim).unwrap();
        let hdr = encode_static_header(&p, &lim).unwrap();
        assert!(decode_static_header(&hdr, geo.total_len - 1, &lim).is_err());
        assert!(decode_static_header(&hdr, geo.total_len + 1, &lim).is_err());
        let mut bad = p;
        bad.capacity_frames = 100;
        assert!(bad.validate(&lim).is_err());
        bad.capacity_frames = 32;
        assert!(bad.validate(&lim).is_err());
        bad = p;
        bad.channels = 0;
        assert!(bad.validate(&lim).is_err());
        bad = p;
        bad.capacity_frames = 1 << 62;
        assert!(bad.validate(&lim).is_err());
        bad = p;
        bad.first_channel = u32::MAX;
        assert!(bad.validate(&lim).is_err());
    }

    #[test]
    fn capacity_rule() {
        // ADR 0011 example: 40 ms + 10 ms + 21.3 ms at 48 kHz -> 4096.
        assert_eq!(RingParams::capacity_for(1920, 480, 1024), 4096);
        assert_eq!(RingParams::capacity_for(0, 0, 0), 64);
        assert_eq!(RingParams::capacity_for(u64::MAX, 1, 1), 1 << 63);
    }
}

#[cfg(test)]
mod layout_map_tests {
    use super::*;

    /// (offset, size) of every defined static-header field.
    const FIELDS: [(usize, usize); 16] = [
        (off::MAGIC, 8),
        (off::VERSION_MAJOR, 4),
        (off::VERSION_MINOR, 4),
        (off::HEADER_LEN, 4),
        (off::FLAGS, 4),
        (off::TOTAL_LEN, 8),
        (off::DIRECTION, 4),
        (off::FORMAT, 4),
        (off::CHANNELS, 4),
        (off::SAMPLE_RATE, 4),
        (off::CAPACITY, 8),
        (off::FIRST_CHANNEL, 4),
        (off::CHANNEL_STRIDE, 8),
        (off::CHANNELS_OFFSET, 8),
        (off::BITMAP_OFFSET, 4),
        (off::PLANE_OFFSET, 4),
    ];

    #[test]
    fn fields_and_reserved_ranges_tile_the_header_without_overlap() {
        let mut owner = [0u8; STATIC_HEADER_LEN];
        let mut mark = |from: usize, len: usize| {
            for o in &mut owner[from..from + len] {
                *o += 1;
            }
        };
        for &(o, n) in &FIELDS {
            mark(o, n);
        }
        for &(from, to) in &RESERVED {
            mark(from, to - from);
        }
        // The dynamic lines start at 0x80 and are outside the static header.
        for (i, c) in owner.iter().enumerate() {
            assert_eq!(*c, 1, "byte {i:#x} is covered {c} times (want exactly 1)");
        }
        // Fields are naturally aligned.
        for &(o, n) in &FIELDS {
            assert!(n == 0 || o % n == 0);
        }
    }

    fn common_params() -> RingParams {
        RingParams {
            direction: Direction::Tx,
            format: SampleFormat::F32,
            channels: 1,
            first_channel: 0,
            sample_rate_hz: 48_000,
            capacity_frames: 64,
        }
    }

    #[test]
    fn capacity_of_2_pow_32_frames_or_more_is_accepted() {
        let big = Limits {
            max_ring_bytes: u64::MAX,
            ..Limits::default()
        };
        for cap in [1u64 << 32, 1 << 33, 1 << 34] {
            let p = RingParams {
                direction: Direction::Tx,
                format: SampleFormat::F32,
                channels: 1,
                first_channel: 0,
                sample_rate_hz: 48_000,
                capacity_frames: cap,
            };
            let geo = p.validate(&big).unwrap();
            let h = encode_static_header(&p, &big).unwrap();
            // Upper half of capacity_frames lives at 0x34..0x38.
            assert_ne!(h[0x34..0x38], [0; 4]);
            let (p2, g2) = decode_static_header(&h, geo.total_len, &big).unwrap();
            assert_eq!((p2, g2), (p, geo));
        }
        // plane_offset is a u32, which bounds the capacity at 2^34 frames.
        let too_big = RingParams {
            capacity_frames: 1 << 35,
            ..common_params()
        };
        assert!(too_big.validate(&big).is_err());
    }
}
