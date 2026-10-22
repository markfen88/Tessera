//! PROVISIONAL reference packet format, for tests and bring-up only.
//!
//! **This is not the wire format.** It follows the *provisional* description
//! in `spec/protocol/flows-media.md` section 1 (`likely`, from a single
//! implementation, never confirmed by a capture): a 9-byte header, then
//! interleaved big-endian samples.
//!
//! ```text
//! byte 0      type byte (sent as 0x02, ignored on receive)      guess
//! bytes 1..5  timestamp, whole seconds, u32 BE                  likely
//! bytes 5..9  timestamp, sub-second in SAMPLES, u32 BE          likely
//! bytes 9..   frames: channels x bytes-per-sample, frame-major  likely
//! ```
//!
//! Combined timestamp = `seconds * sample_rate + subsecond`. The frame count
//! is derived from the datagram length (no fpp field); a trailing partial
//! frame is ignored, matching the spec's `(len - 9) / (N x B)`. Epoch of the
//! seconds field is UNKNOWN.
//!
//! Everything wire-facing lives in the constants below so that the real
//! format (from `tessera-proto` and lab captures) replaces this module
//! without touching the engines.

#![deny(clippy::indexing_slicing)]

use crate::convert::{self, Dither, WireFormat};
use crate::error::{ConfigError, FormatError};
use crate::packet::{Decoded, PacketFormat};

/// Header length in bytes. PROVISIONAL (`likely`).
pub const HEADER_LEN: usize = 9;
/// Value written to header byte 0. PROVISIONAL (`guess`: meaning unknown).
pub const TYPE_BYTE: u8 = 0x02;

/// The provisional reference format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceFormat {
    channels: usize,
    sample_rate: u32,
    wire: WireFormat,
}

impl ReferenceFormat {
    /// A format for `channels` channels at `sample_rate`, with the given
    /// sample encoding (the spec's native choice is [`WireFormat::BE24`]).
    ///
    /// # Errors
    /// [`ConfigError::ZeroChannels`] or [`ConfigError::ZeroSampleRate`].
    pub fn new(channels: usize, sample_rate: u32, wire: WireFormat) -> Result<Self, ConfigError> {
        if channels == 0 {
            return Err(ConfigError::ZeroChannels);
        }
        if sample_rate == 0 {
            return Err(ConfigError::ZeroSampleRate);
        }
        Ok(Self {
            channels,
            sample_rate,
            wire,
        })
    }

    /// The sample encoding.
    #[must_use]
    pub fn wire(&self) -> WireFormat {
        self.wire
    }

    fn frame_bytes(&self) -> usize {
        self.channels.saturating_mul(self.wire.bytes_per_sample())
    }
}

impl PacketFormat for ReferenceFormat {
    fn channels(&self) -> usize {
        self.channels
    }

    fn encoded_len(&self, frames: usize) -> usize {
        frames
            .saturating_mul(self.frame_bytes())
            .saturating_add(HEADER_LEN)
    }

    fn encode(
        &self,
        timestamp: u64,
        samples: &[i32],
        out: &mut [u8],
        dither: Option<&mut Dither>,
    ) -> Result<usize, FormatError> {
        if !samples.len().is_multiple_of(self.channels) {
            return Err(FormatError::BadSamples);
        }
        let frames = samples.len() / self.channels;
        let total = self.encoded_len(frames);
        let rate = u64::from(self.sample_rate);
        let secs = u32::try_from(timestamp / rate).map_err(|_| FormatError::TimestampOverflow)?;
        let sub = u32::try_from(timestamp % rate).map_err(|_| FormatError::TimestampOverflow)?;
        let out = out
            .get_mut(..total)
            .ok_or(FormatError::BufferTooSmall { needed: total })?;
        let (head, body) = out.split_at_mut(HEADER_LEN);
        let header = core::iter::once(TYPE_BYTE)
            .chain(secs.to_be_bytes())
            .chain(sub.to_be_bytes());
        for (dst, src) in head.iter_mut().zip(header) {
            *dst = src;
        }
        let converted = match dither {
            Some(d) => convert::encode_dithered(self.wire, samples, body, d),
            None => convert::encode(self.wire, samples, body),
        };
        converted.map_err(|_| FormatError::BufferTooSmall { needed: total })?;
        Ok(total)
    }

    fn decode(&self, datagram: &[u8], samples: &mut [i32]) -> Result<Decoded, FormatError> {
        let (head, body) = datagram
            .split_at_checked(HEADER_LEN)
            .ok_or(FormatError::Truncated)?;
        let &[_type, s0, s1, s2, s3, u0, u1, u2, u3] = head else {
            return Err(FormatError::Truncated);
        };
        let secs = u64::from(u32::from_be_bytes([s0, s1, s2, s3]));
        let sub = u64::from(u32::from_be_bytes([u0, u1, u2, u3]));
        // Not validated: sub < sample_rate (spec: the reference receiver does not).
        let timestamp = secs * u64::from(self.sample_rate) + sub;
        let fb = self.frame_bytes();
        let frames = body.len() / fb;
        if frames == 0 {
            return Err(FormatError::BadLength);
        }
        let used = frames * fb;
        let n = frames * self.channels;
        let out = samples
            .get_mut(..n)
            .ok_or(FormatError::BufferTooSmall { needed: n })?;
        let payload = body.get(..used).ok_or(FormatError::BadLength)?;
        convert::decode(self.wire, payload, out).map_err(|_| FormatError::BadLength)?;
        Ok(Decoded { timestamp, frames })
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn fmt(ch: usize) -> ReferenceFormat {
        ReferenceFormat::new(ch, 48_000, WireFormat::BE24).unwrap()
    }

    #[test]
    fn header_layout() {
        let f = fmt(1);
        let mut out = [0u8; 16];
        // 3 seconds + 100 samples.
        let n = f
            .encode(3 * 48_000 + 100, &[0x0102_0300], &mut out, None)
            .unwrap();
        assert_eq!(n, 12);
        assert_eq!(&out[..12], &[2, 0, 0, 0, 3, 0, 0, 0, 100, 1, 2, 3]);
    }

    #[test]
    fn decode_rejects_short_and_empty() {
        let f = fmt(2);
        let mut s = [0i32; 8];
        assert_eq!(f.decode(&[], &mut s), Err(FormatError::Truncated));
        assert_eq!(f.decode(&[0; 8], &mut s), Err(FormatError::Truncated));
        assert_eq!(f.decode(&[0; 9], &mut s), Err(FormatError::BadLength));
        // 5 payload bytes: less than one 6-byte frame.
        assert_eq!(f.decode(&[0; 14], &mut s), Err(FormatError::BadLength));
        // 13 payload bytes: two frames and a ragged tail ignored.
        let d = f.decode(&[0; 22], &mut s).unwrap();
        assert_eq!(d.frames, 2);
        // Too many frames for the scratch.
        let mut tiny = [0i32; 3];
        assert_eq!(
            f.decode(&[0; 21], &mut tiny),
            Err(FormatError::BufferTooSmall { needed: 4 })
        );
    }

    #[test]
    fn timestamp_overflow_and_buffer_errors() {
        let f = fmt(1);
        let mut out = [0u8; 12];
        assert_eq!(
            f.encode(u64::MAX, &[0], &mut out, None),
            Err(FormatError::TimestampOverflow)
        );
        assert_eq!(
            f.encode(0, &[0], &mut out[..11], None),
            Err(FormatError::BufferTooSmall { needed: 12 })
        );
        let f2 = fmt(2);
        assert_eq!(
            f2.encode(0, &[0, 0, 0], &mut out, None),
            Err(FormatError::BadSamples)
        );
    }

    proptest! {
        #[test]
        fn roundtrip(ts in 0u64..(u64::from(u32::MAX) * 48_000), frames in 1usize..40, ch in 1usize..9, seed in any::<u64>()) {
            let f = fmt(ch);
            let mut x = seed | 1;
            let samples: Vec<i32> = (0..frames * ch)
                .map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; (x as i32) & !0xFF })
                .collect();
            let mut out = vec![0u8; f.encoded_len(frames)];
            prop_assert_eq!(f.encode(ts, &samples, &mut out, None).unwrap(), out.len());
            let mut back = vec![0i32; frames * ch];
            let d = f.decode(&out, &mut back).unwrap();
            prop_assert_eq!(d.timestamp, ts);
            prop_assert_eq!(d.frames, frames);
            prop_assert_eq!(back, samples);
        }

        #[test]
        fn decode_is_total(bytes in proptest::collection::vec(any::<u8>(), 0..200), ch in 1usize..5) {
            let f = fmt(ch);
            let mut s = vec![0i32; 64];
            let _ = f.decode(&bytes, &mut s);
        }
    }
}
