//! Sample-format conversion between the internal representation and wire
//! bytes (ADR 0006).
//!
//! Internal samples are `i32`, **left-aligned**: a 16-bit sample `s` is
//! stored as `s << 16`, a 24-bit sample as `s << 8`. The wire formats are
//! signed two's-complement integers of 16, 24 or 32 bits, big-endian
//! (`spec/protocol/flows-media.md` section 1, `likely`); little-endian
//! variants and 32-bit IEEE float exist as edge conversions only (a float
//! wire format is UNKNOWN in the spec).
//!
//! Properties (all covered by property tests):
//! * integer round trips are exact: `decode(encode(x)) == x` whenever the
//!   low bits dropped by the target width of `x` are zero;
//! * narrowing without dither **truncates** (drops the low bytes, as the
//!   spec describes); dither is opt-in via [`Dither`];
//! * float: `x -> f32 -> x` is exact when `x` has at most 24 significant
//!   bits; `-1.0` maps to `i32::MIN`, `+1.0` and anything larger clips to
//!   `i32::MAX`, NaN decodes to silence.
//!
//! Everything here is total, allocation-free and `core`-only.

#![deny(clippy::indexing_slicing)]

use crate::error::ConvertError;

/// Sample representation on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SampleFormat {
    /// Signed 16-bit integer.
    I16,
    /// Signed 24-bit integer (3 bytes, no padding).
    I24,
    /// Signed 32-bit integer.
    I32,
    /// IEEE-754 binary32 in `[-1.0, 1.0)`.
    F32,
}

impl SampleFormat {
    /// Bytes one sample occupies on the wire.
    #[must_use]
    pub const fn bytes(self) -> usize {
        match self {
            Self::I16 => 2,
            Self::I24 => 3,
            Self::I32 | Self::F32 => 4,
        }
    }

    /// Low bits of a left-aligned `i32` that this format discards.
    const fn dropped_bits(self) -> u32 {
        match self {
            Self::I16 => 16,
            Self::I24 => 8,
            Self::I32 | Self::F32 => 0,
        }
    }
}

/// Byte order of multi-byte samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Endian {
    /// Network order, most significant byte first (the native wire format).
    Big,
    /// Least significant byte first.
    Little,
}

/// A sample format with its byte order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WireFormat {
    /// Sample representation.
    pub sample: SampleFormat,
    /// Byte order.
    pub endian: Endian,
}

impl WireFormat {
    /// 16-bit big-endian integers (native wire format).
    pub const BE16: Self = Self::new(SampleFormat::I16, Endian::Big);
    /// 24-bit big-endian integers (native wire format).
    pub const BE24: Self = Self::new(SampleFormat::I24, Endian::Big);
    /// 32-bit big-endian integers (native wire format).
    pub const BE32: Self = Self::new(SampleFormat::I32, Endian::Big);

    /// Builds a wire format.
    #[must_use]
    pub const fn new(sample: SampleFormat, endian: Endian) -> Self {
        Self { sample, endian }
    }

    /// Bytes per sample.
    #[must_use]
    pub const fn bytes_per_sample(self) -> usize {
        self.sample.bytes()
    }
}

/// Optional triangular (TPDF) dither for narrowing conversions.
///
/// Off by default everywhere (ADR 0006: bit-exact). The generator is a
/// seeded xorshift64*, so output is reproducible. Only 16- and 24-bit targets
/// use it; 32-bit integer and float output never lose bits to dither.
#[derive(Debug, Clone)]
pub struct Dither {
    state: u64,
}

impl Dither {
    /// A generator with the given seed (zero is replaced by a fixed
    /// non-zero constant).
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Adds TPDF noise plus a rounding offset, returning a left-aligned value
    /// that is then truncated by `drop` bits by the caller.
    fn apply(&mut self, x: i32, drop: u32) -> i32 {
        if drop == 0 {
            return x;
        }
        let lsb = 1u64 << drop;
        let r1 = (self.next() >> 11) & (lsb - 1);
        let r2 = (self.next() >> 11) & (lsb - 1);
        let noise = r1 as i64 - r2 as i64;
        let t = i64::from(x) + noise + (lsb / 2) as i64;
        t.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    }
}

const F32_SCALE: f32 = 2_147_483_648.0; // 2^31

/// Converts a left-aligned `i32` to a float in `[-1.0, 1.0)`.
#[must_use]
pub fn i32_to_f32(x: i32) -> f32 {
    x as f32 * (1.0 / F32_SCALE)
}

/// Converts a float to a left-aligned `i32`: rounds to nearest (ties away
/// from zero), saturates out-of-range values, maps NaN to 0.
#[must_use]
pub fn f32_to_i32(f: f32) -> i32 {
    let v = f * F32_SCALE;
    if v.is_nan() {
        0
    } else if v >= F32_SCALE {
        i32::MAX
    } else if v <= -F32_SCALE {
        i32::MIN
    } else if (-8_388_608.0..8_388_608.0).contains(&v) {
        // Below 2^23 floats have a fractional part; at or above they are
        // already integers (adding 0.5 there would round the wrong way).
        if v >= 0.0 {
            (v + 0.5) as i32
        } else {
            (v - 0.5) as i32
        }
    } else {
        v as i32
    }
}

#[inline]
fn put(fmt: WireFormat, x: i32, dither: Option<&mut Dither>, out: &mut [u8]) {
    let drop = fmt.sample.dropped_bits();
    let x = match dither {
        Some(d) => d.apply(x, drop),
        None => x,
    };
    let be = fmt.endian == Endian::Big;
    match fmt.sample {
        SampleFormat::I16 => {
            let v = (x >> 16) as i16;
            let b = if be { v.to_be_bytes() } else { v.to_le_bytes() };
            out.copy_from_slice(&b);
        }
        SampleFormat::I24 => {
            let v = x >> 8;
            let b = if be { v.to_be_bytes() } else { v.to_le_bytes() };
            // Big-endian: the low three of four bytes; little-endian: the
            // first three.
            let b3 = if be {
                [b[1], b[2], b[3]]
            } else {
                [b[0], b[1], b[2]]
            };
            out.copy_from_slice(&b3);
        }
        SampleFormat::I32 => {
            let b = if be { x.to_be_bytes() } else { x.to_le_bytes() };
            out.copy_from_slice(&b);
        }
        SampleFormat::F32 => {
            let f = i32_to_f32(x);
            let b = if be { f.to_be_bytes() } else { f.to_le_bytes() };
            out.copy_from_slice(&b);
        }
    }
}

#[inline]
fn get(fmt: WireFormat, b: &[u8]) -> i32 {
    let be = fmt.endian == Endian::Big;
    match (fmt.sample, b) {
        (SampleFormat::I16, &[a, c]) => {
            let v = if be {
                i16::from_be_bytes([a, c])
            } else {
                i16::from_le_bytes([a, c])
            };
            i32::from(v) << 16
        }
        (SampleFormat::I24, &[a, c, d]) => {
            let (hi, mid, lo) = if be { (a, c, d) } else { (d, c, a) };
            i32::from_be_bytes([hi, mid, lo, 0])
        }
        (SampleFormat::I32, &[a, c, d, e]) => {
            if be {
                i32::from_be_bytes([a, c, d, e])
            } else {
                i32::from_le_bytes([a, c, d, e])
            }
        }
        (SampleFormat::F32, &[a, c, d, e]) => {
            let f = if be {
                f32::from_be_bytes([a, c, d, e])
            } else {
                f32::from_le_bytes([a, c, d, e])
            };
            f32_to_i32(f)
        }
        _ => 0,
    }
}

/// Encodes `src` into `dst` (truncating, no dither). Returns the number of
/// bytes written (`src.len() * bytes_per_sample`).
///
/// # Errors
/// [`ConvertError::OutputTooSmall`] if `dst` cannot hold the result.
pub fn encode(fmt: WireFormat, src: &[i32], dst: &mut [u8]) -> Result<usize, ConvertError> {
    encode_impl(fmt, src, dst, None)
}

/// Like [`encode`], adding TPDF dither when the target is narrower than 32
/// bits.
///
/// # Errors
/// [`ConvertError::OutputTooSmall`] if `dst` cannot hold the result.
pub fn encode_dithered(
    fmt: WireFormat,
    src: &[i32],
    dst: &mut [u8],
    dither: &mut Dither,
) -> Result<usize, ConvertError> {
    encode_impl(fmt, src, dst, Some(dither))
}

fn encode_impl(
    fmt: WireFormat,
    src: &[i32],
    dst: &mut [u8],
    mut dither: Option<&mut Dither>,
) -> Result<usize, ConvertError> {
    let b = fmt.bytes_per_sample();
    let needed = src
        .len()
        .checked_mul(b)
        .ok_or(ConvertError::OutputTooSmall { needed: usize::MAX })?;
    let dst = dst
        .get_mut(..needed)
        .ok_or(ConvertError::OutputTooSmall { needed })?;
    for (x, out) in src.iter().zip(dst.chunks_exact_mut(b)) {
        put(fmt, *x, dither.as_deref_mut(), out);
    }
    Ok(needed)
}

/// Decodes wire bytes into left-aligned `i32` samples. Returns the number of
/// samples written.
///
/// # Errors
/// [`ConvertError::Misaligned`] if `src` is not a whole number of samples,
/// [`ConvertError::OutputTooSmall`] if `dst` is too short.
pub fn decode(fmt: WireFormat, src: &[u8], dst: &mut [i32]) -> Result<usize, ConvertError> {
    let b = fmt.bytes_per_sample();
    if src.len() % b != 0 {
        return Err(ConvertError::Misaligned);
    }
    let n = src.len() / b;
    let dst = dst
        .get_mut(..n)
        .ok_or(ConvertError::OutputTooSmall { needed: n })?;
    for (chunk, out) in src.chunks_exact(b).zip(dst.iter_mut()) {
        *out = get(fmt, chunk);
    }
    Ok(n)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const ALL_INT: [WireFormat; 6] = [
        WireFormat::new(SampleFormat::I16, Endian::Big),
        WireFormat::new(SampleFormat::I16, Endian::Little),
        WireFormat::new(SampleFormat::I24, Endian::Big),
        WireFormat::new(SampleFormat::I24, Endian::Little),
        WireFormat::new(SampleFormat::I32, Endian::Big),
        WireFormat::new(SampleFormat::I32, Endian::Little),
    ];

    fn roundtrip(fmt: WireFormat, x: i32) -> i32 {
        let mut b = [0u8; 4];
        let n = encode(fmt, &[x], &mut b).unwrap();
        assert_eq!(n, fmt.bytes_per_sample());
        let mut out = [0i32; 1];
        decode(fmt, &b[..n], &mut out).unwrap();
        out[0]
    }

    #[test]
    fn known_big_endian_bytes() {
        let mut b = [0u8; 6];
        encode(WireFormat::BE24, &[0x1234_5600, i32::MIN], &mut b).unwrap();
        assert_eq!(b, [0x12, 0x34, 0x56, 0x80, 0x00, 0x00]);
        let mut b = [0u8; 4];
        encode(WireFormat::BE16, &[0x7FFF_0000, -65536], &mut b).unwrap();
        assert_eq!(b, [0x7F, 0xFF, 0xFF, 0xFF]);
        let mut b = [0u8; 3];
        encode(
            WireFormat::new(SampleFormat::I24, Endian::Little),
            &[0x1234_5600],
            &mut b,
        )
        .unwrap();
        assert_eq!(b, [0x56, 0x34, 0x12]);
    }

    #[test]
    fn float_edges() {
        assert_eq!(f32_to_i32(-1.0), i32::MIN);
        assert_eq!(f32_to_i32(1.0), i32::MAX);
        assert_eq!(f32_to_i32(7.5), i32::MAX);
        assert_eq!(f32_to_i32(-7.5), i32::MIN);
        assert_eq!(f32_to_i32(f32::NAN), 0);
        assert_eq!(f32_to_i32(f32::INFINITY), i32::MAX);
        assert_eq!(f32_to_i32(0.0), 0);
        assert_eq!(i32_to_f32(i32::MIN), -1.0);
        assert_eq!(f32_to_i32(0.5), 1 << 30);
    }

    #[test]
    fn size_errors() {
        let mut small = [0u8; 3];
        assert_eq!(
            encode(WireFormat::BE16, &[1, 2], &mut small),
            Err(ConvertError::OutputTooSmall { needed: 4 })
        );
        let mut out = [0i32; 1];
        assert_eq!(
            decode(WireFormat::BE24, &[0; 4], &mut out),
            Err(ConvertError::Misaligned)
        );
        assert_eq!(
            decode(WireFormat::BE16, &[0; 4], &mut out),
            Err(ConvertError::OutputTooSmall { needed: 2 })
        );
    }

    #[test]
    fn dither_is_reproducible_and_bounded() {
        let src = [0x1234_0000i32; 64];
        let mut a = [0u8; 128];
        let mut b = [0u8; 128];
        encode_dithered(WireFormat::BE16, &src, &mut a, &mut Dither::new(7)).unwrap();
        encode_dithered(WireFormat::BE16, &src, &mut b, &mut Dither::new(7)).unwrap();
        assert_eq!(a, b);
        let mut out = [0i32; 64];
        decode(WireFormat::BE16, &a, &mut out).unwrap();
        // Within one LSB of the target width of the exact value ...
        assert!(
            out.iter()
                .all(|v| (v.wrapping_sub(0x1234_0000)).abs() <= 1 << 16)
        );
        // ... and actually dithered (not all identical).
        assert!(out.iter().any(|v| *v != 0x1234_0000));
        // 32-bit output is never touched by dither.
        let mut w = [0u8; 8];
        encode_dithered(WireFormat::BE32, &[5, -5], &mut w, &mut Dither::new(1)).unwrap();
        assert_eq!(w, [0, 0, 0, 5, 255, 255, 255, 251]);
    }

    proptest! {
        // Every n-bit value survives wire -> internal -> wire.
        #[test]
        fn nbit_values_roundtrip_exactly(v16 in any::<i16>(), v24 in -(1i32 << 23)..(1i32 << 23), v32 in any::<i32>()) {
            for endian in [Endian::Big, Endian::Little] {
                let f = WireFormat::new(SampleFormat::I16, endian);
                let internal = i32::from(v16) << 16;
                let mut b = [0u8; 2];
                encode(f, &[internal], &mut b).unwrap();
                let mut o = [0i32; 1];
                decode(f, &b, &mut o).unwrap();
                prop_assert_eq!(o[0], internal);
                let w = if endian == Endian::Big { v16.to_be_bytes() } else { v16.to_le_bytes() };
                prop_assert_eq!(b, w);

                let f = WireFormat::new(SampleFormat::I24, endian);
                let internal = v24 << 8;
                prop_assert_eq!(roundtrip(f, internal), internal);

                let f = WireFormat::new(SampleFormat::I32, endian);
                prop_assert_eq!(roundtrip(f, v32), v32);
            }
        }

        // Left-aligned values with zero low bits are exact in every width.
        #[test]
        fn aligned_values_exact(x in any::<i32>()) {
            let a16 = x & !0xFFFF;
            let a24 = x & !0xFF;
            for endian in [Endian::Big, Endian::Little] {
                prop_assert_eq!(roundtrip(WireFormat::new(SampleFormat::I16, endian), a16), a16);
                prop_assert_eq!(roundtrip(WireFormat::new(SampleFormat::I24, endian), a24), a24);
                prop_assert_eq!(roundtrip(WireFormat::new(SampleFormat::I32, endian), x), x);
            }
        }

        // Narrowing without dither is truncation toward minus infinity.
        #[test]
        fn narrowing_truncates(x in any::<i32>()) {
            prop_assert_eq!(roundtrip(WireFormat::BE16, x), x & !0xFFFF);
            prop_assert_eq!(roundtrip(WireFormat::BE24, x), x & !0xFF);
        }

        // 24-bit-aligned values pass exactly through f32.
        #[test]
        fn f32_exact_for_24_significant_bits(v in -(1i32 << 23)..(1i32 << 23)) {
            let x = v << 8;
            for endian in [Endian::Big, Endian::Little] {
                prop_assert_eq!(roundtrip(WireFormat::new(SampleFormat::F32, endian), x), x);
            }
            prop_assert_eq!(f32_to_i32(i32_to_f32(x)), x);
        }

        // For arbitrary i32 the f32 round trip is within f32 resolution.
        #[test]
        fn f32_roundtrip_error_is_bounded(x in any::<i32>()) {
            let y = f32_to_i32(i32_to_f32(x));
            let err = (i64::from(x) - i64::from(y)).abs();
            // half an ulp of a 24-bit mantissa at 2^31 is 2^7, plus saturation at the top end
            prop_assert!(err <= 128, "x={x} y={y}");
        }

        // Float decode is total and monotone-ish: never panics, always in range.
        #[test]
        fn f32_decode_total(bits in any::<u32>()) {
            let f = f32::from_bits(bits);
            let _ = f32_to_i32(f);
        }

        // Slice conversions agree with per-sample conversion and are total.
        #[test]
        fn slice_encode_decode(xs in proptest::collection::vec(any::<i32>(), 0..64), idx in 0usize..6) {
            let fmt = *ALL_INT.get(idx).unwrap();
            let mut bytes = vec![0u8; xs.len() * fmt.bytes_per_sample()];
            prop_assert_eq!(encode(fmt, &xs, &mut bytes).unwrap(), bytes.len());
            let mut back = vec![0i32; xs.len()];
            prop_assert_eq!(decode(fmt, &bytes, &mut back).unwrap(), xs.len());
            for (x, y) in xs.iter().zip(&back) {
                prop_assert_eq!(roundtrip(fmt, *x), *y);
            }
        }

        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64), idx in 0usize..6) {
            let fmt = *ALL_INT.get(idx).unwrap();
            let mut out = vec![0i32; 64];
            let _ = decode(fmt, &bytes, &mut out);
            let ffmt = WireFormat::new(SampleFormat::F32, Endian::Big);
            let _ = decode(ffmt, &bytes, &mut out);
        }
    }
}
