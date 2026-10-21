//! The packet formats a flow can use, behind `tessera_media::PacketFormat`.
//!
//! **PROVISIONAL.** Today the only format is the test-only reference format
//! of `tessera-media` (a guess at the wire layout, `flows-media.md`
//! `likely`). The real format arrives with `tessera-proto` and lab captures;
//! it becomes another variant here and nothing else in the engine changes.

use tessera_media::convert::{Dither, WireFormat};
use tessera_media::error::{ConfigError, FormatError};
use tessera_media::packet::{Decoded, PacketFormat};
use tessera_media::reference::{HEADER_LEN, ReferenceFormat};
use tessera_state::config::FlowFormat;

/// A packet format selected by configuration.
#[derive(Debug, Clone, Copy)]
pub enum AnyFormat {
    /// The provisional reference format.
    Reference(ReferenceFormat),
}

/// Wire sample encoding for a bit depth (16, 24 or 32), big-endian.
///
/// # Errors
/// Other depths.
pub fn wire_for_bits(bits: u8) -> Result<WireFormat, String> {
    match bits {
        16 => Ok(WireFormat::BE16),
        24 => Ok(WireFormat::BE24),
        32 => Ok(WireFormat::BE32),
        other => Err(format!("unsupported encoding of {other} bits")),
    }
}

impl AnyFormat {
    /// Builds the format for `channels` channels.
    ///
    /// # Errors
    /// Unsupported bit depth or invalid channel count / rate.
    pub fn build(kind: FlowFormat, channels: usize, rate: u32, bits: u8) -> Result<Self, String> {
        let wire = wire_for_bits(bits)?;
        match kind {
            FlowFormat::Reference => ReferenceFormat::new(channels, rate, wire)
                .map(Self::Reference)
                .map_err(|e: ConfigError| e.to_string()),
        }
    }

    /// Header bytes before the samples.
    #[must_use]
    pub fn header_len(&self) -> usize {
        match self {
            Self::Reference(_) => HEADER_LEN,
        }
    }

    /// Bytes per sample on the wire.
    #[must_use]
    pub fn bytes_per_sample(&self) -> usize {
        match self {
            Self::Reference(f) => f.wire().bytes_per_sample(),
        }
    }
}

impl PacketFormat for AnyFormat {
    fn channels(&self) -> usize {
        match self {
            Self::Reference(f) => f.channels(),
        }
    }

    fn encoded_len(&self, frames: usize) -> usize {
        match self {
            Self::Reference(f) => f.encoded_len(frames),
        }
    }

    fn encode(
        &self,
        timestamp: u64,
        samples: &[i32],
        out: &mut [u8],
        dither: Option<&mut Dither>,
    ) -> Result<usize, FormatError> {
        match self {
            Self::Reference(f) => f.encode(timestamp, samples, out, dither),
        }
    }

    fn decode(&self, datagram: &[u8], samples: &mut [i32]) -> Result<Decoded, FormatError> {
        match self {
            Self::Reference(f) => f.decode(datagram, samples),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_depths_map_to_the_native_wire_formats() {
        assert_eq!(wire_for_bits(16).unwrap(), WireFormat::BE16);
        assert_eq!(wire_for_bits(24).unwrap(), WireFormat::BE24);
        assert_eq!(wire_for_bits(32).unwrap(), WireFormat::BE32);
        assert!(wire_for_bits(20).is_err());
    }

    #[test]
    fn the_reference_format_round_trips_through_the_enum() {
        let f = AnyFormat::build(FlowFormat::Reference, 2, 48_000, 24).unwrap();
        let samples = [1 << 8, -2 << 8, 3 << 8, -4 << 8];
        let mut out = vec![0u8; f.encoded_len(2)];
        let n = f.encode(1234, &samples, &mut out, None).unwrap();
        let mut back = [0i32; 4];
        let d = f.decode(&out[..n], &mut back).unwrap();
        assert_eq!((d.timestamp, d.frames), (1234, 2));
        assert_eq!(back, samples);
        assert_eq!(f.header_len(), 9);
        assert_eq!(f.bytes_per_sample(), 3);
    }

    #[test]
    fn invalid_arguments_are_errors() {
        assert!(AnyFormat::build(FlowFormat::Reference, 0, 48_000, 24).is_err());
        assert!(AnyFormat::build(FlowFormat::Reference, 2, 0, 24).is_err());
        assert!(AnyFormat::build(FlowFormat::Reference, 2, 48_000, 8).is_err());
    }
}
