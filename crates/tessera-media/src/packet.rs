//! The packet-format abstraction.
//!
//! The wire format of media packets is provisional (`flows-media.md` is
//! `likely`/`guess`, drawn from one implementation) and `tessera-proto` does
//! not exist yet. The flow engines therefore never touch header bytes: they
//! hand a [`PacketFormat`] interleaved internal samples (left-aligned `i32`)
//! and a media-time stamp, and get bytes back, or the reverse.

use crate::convert::Dither;
use crate::error::FormatError;

/// A successfully decoded media datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decoded {
    /// Media time (sample count on the media clock) of the first frame.
    pub timestamp: u64,
    /// Frames stored in the sample slice passed to `decode`.
    pub frames: usize,
}

/// Encodes and decodes the full datagram (header + audio payload) of one
/// media packet for a fixed channel layout and sample format.
///
/// Implementations must be **total**: `decode` returns an error on any
/// malformed input and never panics (ADR 0009); neither method allocates.
/// `tessera-proto` will provide the real implementation; this crate ships
/// only the provisional [`reference`](crate::reference) format for tests.
pub trait PacketFormat {
    /// Channels per frame carried by this format instance.
    fn channels(&self) -> usize;

    /// Total datagram length in bytes for a packet of `frames` frames.
    fn encoded_len(&self, frames: usize) -> usize;

    /// Writes one datagram into `out` and returns its length.
    ///
    /// `samples` holds `frames * channels()` interleaved samples
    /// (frame-major, channel order as in the flow descriptor).
    ///
    /// `dither` is `Some` only when the operator configured dither for the
    /// flow (ADR 0006: off by default); an implementation that narrows the
    /// sample width should pass it to
    /// [`convert::encode_dithered`](crate::convert::encode_dithered) and may
    /// ignore it otherwise.
    ///
    /// # Errors
    /// [`FormatError::BufferTooSmall`], [`FormatError::BadSamples`] or
    /// [`FormatError::TimestampOverflow`].
    fn encode(
        &self,
        timestamp: u64,
        samples: &[i32],
        out: &mut [u8],
        dither: Option<&mut Dither>,
    ) -> Result<usize, FormatError>;

    /// Parses one datagram, writing interleaved samples into `samples`.
    ///
    /// # Errors
    /// [`FormatError::Truncated`], [`FormatError::BadLength`] or
    /// [`FormatError::BufferTooSmall`] (the sample slice cannot hold the
    /// frames in the datagram).
    fn decode(&self, datagram: &[u8], samples: &mut [i32]) -> Result<Decoded, FormatError>;
}

/// Largest frames-per-packet such that `header_len + fpp * channels *
/// bytes_per_sample <= max_payload` (ADR 0006/0011), or 0 if not even one
/// frame fits.
#[must_use]
pub fn max_frames_per_packet(
    header_len: usize,
    channels: usize,
    bytes_per_sample: usize,
    max_payload: usize,
) -> usize {
    let frame = channels.saturating_mul(bytes_per_sample);
    if frame == 0 {
        return 0;
    }
    max_payload.saturating_sub(header_len) / frame
}

/// Frames in a packet of the given duration (rounded to nearest, at least 1).
#[must_use]
pub fn frames_for_period_ns(sample_rate: u32, period_ns: u64) -> usize {
    let twice = u128::from(period_ns) * u128::from(sample_rate) * 2 + 1_000_000_000;
    let n = twice / 2_000_000_000;
    usize::try_from(n).unwrap_or(usize::MAX).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mtu_bound_matches_adr_0011() {
        // 8 channels x 24-bit: fpp <= 57; 4 x 24-bit: <= 115.
        assert_eq!(max_frames_per_packet(9, 8, 3, 1400), 57);
        assert_eq!(max_frames_per_packet(9, 4, 3, 1400), 115);
        assert_eq!(max_frames_per_packet(9, 0, 3, 1400), 0);
        assert_eq!(max_frames_per_packet(2000, 2, 3, 1400), 0);
    }

    #[test]
    fn period_to_frames() {
        assert_eq!(frames_for_period_ns(48_000, 1_000_000), 48);
        assert_eq!(frames_for_period_ns(48_000, 666_667), 32);
        assert_eq!(frames_for_period_ns(48_000, 0), 1);
    }
}
