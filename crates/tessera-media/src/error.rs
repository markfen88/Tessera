//! Per-crate error enums (ADR 0009): hand-written `Display`, no `thiserror`,
//! usable without `std`.

use core::fmt;

macro_rules! error_impls {
    ($t:ty) => {
        impl core::error::Error for $t {}
    };
}

/// A configuration was rejected by `validate`/`new`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigError {
    /// The sample rate is zero.
    ZeroSampleRate,
    /// The channel count is zero.
    ZeroChannels,
    /// Frames per packet is zero.
    ZeroFramesPerPacket,
    /// The packet format and the flow disagree on the channel count.
    ChannelMismatch {
        /// Channels in the flow configuration.
        config: usize,
        /// Channels the packet format was built for.
        format: usize,
    },
    /// A computed size does not fit in memory addressing.
    SizeOverflow,
    /// A buffer capacity is not a power of two of at least
    /// `MIN_CAPACITY`.
    BadCapacity,
    /// One packet would exceed the configured maximum payload size.
    PacketTooLarge {
        /// Bytes one packet needs.
        needed: usize,
        /// The configured limit.
        max: usize,
    },
    /// A numeric parameter is outside its valid range.
    OutOfRange(&'static str),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSampleRate => f.write_str("sample rate must be non-zero"),
            Self::ZeroChannels => f.write_str("channel count must be non-zero"),
            Self::ZeroFramesPerPacket => f.write_str("frames per packet must be non-zero"),
            Self::ChannelMismatch { config, format } => write!(
                f,
                "flow has {config} channels but the packet format has {format}"
            ),
            Self::SizeOverflow => f.write_str("a computed size overflows"),
            Self::BadCapacity => f.write_str("capacity must be a power of two >= the minimum"),
            Self::PacketTooLarge { needed, max } => {
                write!(f, "a packet needs {needed} bytes, the limit is {max}")
            }
            Self::OutOfRange(what) => write!(f, "{what} is out of range"),
        }
    }
}
error_impls!(ConfigError);

/// A [`PacketFormat`](crate::PacketFormat) could not encode or decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FormatError {
    /// The datagram is shorter than the fixed header.
    Truncated,
    /// The payload length is not a whole number of frames, or holds no frame.
    BadLength,
    /// The destination buffer is too small.
    BufferTooSmall {
        /// What the operation needs (bytes for `encode`, samples for `decode`).
        needed: usize,
    },
    /// The timestamp cannot be represented on the wire.
    TimestampOverflow,
    /// The sample slice does not hold a whole number of frames.
    BadSamples,
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("datagram shorter than the header"),
            Self::BadLength => f.write_str("payload is not a whole number of frames"),
            Self::BufferTooSmall { needed } => write!(f, "buffer too small, {needed} needed"),
            Self::TimestampOverflow => f.write_str("timestamp not representable on the wire"),
            Self::BadSamples => f.write_str("sample count is not a multiple of the channels"),
        }
    }
}
error_impls!(FormatError);

/// Sample-format conversion failed (sizes only; conversion itself is total).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConvertError {
    /// The output slice is too small.
    OutputTooSmall {
        /// Required output length (bytes or samples).
        needed: usize,
    },
    /// The input byte length is not a multiple of the sample width.
    Misaligned,
}

impl fmt::Display for ConvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputTooSmall { needed } => write!(f, "output too small, {needed} needed"),
            Self::Misaligned => f.write_str("input length is not a multiple of the sample width"),
        }
    }
}
error_impls!(ConvertError);

/// A TX poll failed. The flow state is unchanged unless stated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TxError {
    /// The datagram buffer passed to `poll` is smaller than one packet.
    OutputTooSmall {
        /// Bytes one packet needs.
        needed: usize,
    },
    /// The packet format refused to encode.
    Format(FormatError),
}

impl fmt::Display for TxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputTooSmall { needed } => {
                write!(f, "datagram buffer too small, {needed} needed")
            }
            Self::Format(e) => write!(f, "packet format: {e}"),
        }
    }
}
error_impls!(TxError);

/// A drift-control configuration was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DriftConfigError {
    /// The named parameter is NaN or infinite.
    NotFinite(&'static str),
    /// The named parameter must not be negative.
    Negative(&'static str),
    /// `max_correction` must lie in `(0, 1)`.
    BadMaxCorrection,
    /// The loop may command ratios (`1 +- max_correction`) outside the range
    /// the resampler supports, so the integrator would wind up while the
    /// resampler rejected the ratio.
    CorrectionExceedsRatioRange,
}

impl fmt::Display for DriftConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFinite(p) => write!(f, "{p} is not finite"),
            Self::Negative(p) => write!(f, "{p} must not be negative"),
            Self::BadMaxCorrection => f.write_str("max_correction must be in (0, 1)"),
            Self::CorrectionExceedsRatioRange => {
                f.write_str("max_correction exceeds the resampler's ratio range")
            }
        }
    }
}
error_impls!(DriftConfigError);

/// A resampler operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResampleError {
    /// The requested ratio is not finite, not positive, or outside the range
    /// the resampler was built for.
    InvalidRatio,
    /// The input slice holds fewer frames than `input_frames_next`.
    InputTooShort {
        /// Frames required.
        needed: usize,
    },
    /// The output slice is smaller than `output_frames_max`.
    OutputTooSmall {
        /// Frames required.
        needed: usize,
    },
    /// The configuration is invalid (zero channels, zero chunk, ...).
    BadConfig,
    /// The requested quality level is not available in this build.
    Unsupported,
    /// The backing resampler library reported an error.
    Backend,
}

impl fmt::Display for ResampleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRatio => f.write_str("invalid resampling ratio"),
            Self::InputTooShort { needed } => write!(f, "input too short, {needed} frames needed"),
            Self::OutputTooSmall { needed } => {
                write!(f, "output too small, {needed} frames needed")
            }
            Self::BadConfig => f.write_str("invalid resampler configuration"),
            Self::Unsupported => f.write_str("quality level not available in this build"),
            Self::Backend => f.write_str("resampler backend error"),
        }
    }
}
error_impls!(ResampleError);
