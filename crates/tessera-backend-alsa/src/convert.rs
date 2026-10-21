//! PCM sample formats and the copy between ALSA channel areas and the
//! planar ring samples (`i32` left-aligned or `f32`).
//!
//! An [`Area`] mirrors `snd_pcm_channel_area_t`: sample `i` of the channel
//! lives at byte `first + step * i` of a buffer. Interleaved and
//! non-interleaved buffers, and the buffers of MMAP and RW access, therefore
//! all go through one code path.
//!
//! Nothing here allocates. Little-endian formats only (every Linux target of
//! the workspace is little-endian).

use core::marker::PhantomData;

use tessera_audio_ipc::{Sample, SampleFormat};

#[cfg(not(target_endian = "little"))]
compile_error!("tessera-backend-alsa supports little-endian hosts only");

/// The sample formats the PCM offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PcmFormat {
    /// `S16_LE`.
    S16,
    /// `S24_LE`: 24 bits in the low three bytes of a 32-bit word.
    S24,
    /// `S24_3LE`: packed 24 bits.
    S24Packed,
    /// `S32_LE`.
    S32,
    /// `FLOAT_LE`: IEEE binary32.
    Float,
}

impl PcmFormat {
    /// All formats, in the order they are offered to alsa-lib.
    pub const ALL: [PcmFormat; 5] = [
        PcmFormat::S32,
        PcmFormat::S24,
        PcmFormat::Float,
        PcmFormat::S16,
        PcmFormat::S24Packed,
    ];

    /// Bytes per sample.
    pub const fn bytes(self) -> usize {
        match self {
            PcmFormat::S16 => 2,
            PcmFormat::S24Packed => 3,
            PcmFormat::S24 | PcmFormat::S32 | PcmFormat::Float => 4,
        }
    }

    /// The `snd_pcm_format_t` value.
    pub const fn to_alsa(self) -> u32 {
        match self {
            PcmFormat::S16 => 2,
            PcmFormat::S24 => 6,
            PcmFormat::S32 => 10,
            PcmFormat::Float => 14,
            PcmFormat::S24Packed => 32,
        }
    }

    /// From a `snd_pcm_format_t` value.
    pub fn from_alsa(v: u32) -> Option<PcmFormat> {
        PcmFormat::ALL.into_iter().find(|f| f.to_alsa() == v)
    }
}

/// Converts a left-aligned `i32` sample to `f32` in `[-1, 1)`.
#[inline]
pub fn i32_to_f32(x: i32) -> f32 {
    x as f32 * (1.0 / 2_147_483_648.0)
}

/// Converts an `f32` sample to a left-aligned `i32`, rounding to nearest and
/// saturating (NaN becomes 0).
#[inline]
pub fn f32_to_i32(x: f32) -> i32 {
    (x * 2_147_483_648.0).round() as i32
}

/// One format's sample codec. Samples travel as four bytes, zero padded.
trait Codec {
    const W: usize;
    fn dec_i32(t: [u8; 4]) -> i32;
    fn dec_f32(t: [u8; 4]) -> f32 {
        i32_to_f32(Self::dec_i32(t))
    }
    fn enc_i32(x: i32) -> [u8; 4];
    fn enc_f32(x: f32) -> [u8; 4] {
        Self::enc_i32(f32_to_i32(x))
    }
}

/// Rounds a left-aligned `i32` to `bits` significant bits, saturating.
#[inline]
fn narrow(x: i32, bits: u32) -> i32 {
    let shift = 32 - bits;
    let half = 1i64 << (shift - 1);
    let max = (1i64 << (bits - 1)) - 1;
    let min = -(1i64 << (bits - 1));
    (((i64::from(x) + half) >> shift).clamp(min, max)) as i32
}

struct S16;
struct S24;
struct S24Packed;
struct S32;
struct Float;

impl Codec for S16 {
    const W: usize = 2;
    fn dec_i32(t: [u8; 4]) -> i32 {
        i32::from(i16::from_le_bytes([t[0], t[1]])) << 16
    }
    fn enc_i32(x: i32) -> [u8; 4] {
        let v = narrow(x, 16) as i16;
        let b = v.to_le_bytes();
        [b[0], b[1], 0, 0]
    }
}

impl Codec for S24 {
    const W: usize = 4;
    fn dec_i32(t: [u8; 4]) -> i32 {
        (u32::from_le_bytes(t) << 8) as i32
    }
    fn enc_i32(x: i32) -> [u8; 4] {
        // Sign-extended into the unused byte.
        narrow(x, 24).to_le_bytes()
    }
}

impl Codec for S24Packed {
    const W: usize = 3;
    fn dec_i32(t: [u8; 4]) -> i32 {
        i32::from_le_bytes([0, t[0], t[1], t[2]])
    }
    fn enc_i32(x: i32) -> [u8; 4] {
        let b = narrow(x, 24).to_le_bytes();
        [b[0], b[1], b[2], 0]
    }
}

impl Codec for S32 {
    const W: usize = 4;
    fn dec_i32(t: [u8; 4]) -> i32 {
        i32::from_le_bytes(t)
    }
    fn enc_i32(x: i32) -> [u8; 4] {
        x.to_le_bytes()
    }
}

impl Codec for Float {
    const W: usize = 4;
    fn dec_i32(t: [u8; 4]) -> i32 {
        f32_to_i32(f32::from_le_bytes(t))
    }
    fn dec_f32(t: [u8; 4]) -> f32 {
        f32::from_le_bytes(t)
    }
    fn enc_i32(x: i32) -> [u8; 4] {
        i32_to_f32(x).to_le_bytes()
    }
    fn enc_f32(x: f32) -> [u8; 4] {
        x.to_le_bytes()
    }
}

/// A strided view of one channel of an ALSA buffer (`snd_pcm_channel_area_t`
/// with the frame count made explicit).
///
/// Every access is bounds-checked against `len`, the number of valid bytes
/// behind `ptr`, so the safe API cannot read or write outside the buffer the
/// area was built from.
#[derive(Debug)]
pub struct Area<'a> {
    ptr: *mut u8,
    len: usize,
    first: usize,
    step: usize,
    _buf: PhantomData<&'a mut [u8]>,
}

impl<'a> Area<'a> {
    /// A view into `buf`: sample `i` at byte `first + step * i`.
    pub fn from_slice(buf: &'a mut [u8], first: usize, step: usize) -> Area<'a> {
        Area {
            ptr: buf.as_mut_ptr(),
            len: buf.len(),
            first,
            step,
            _buf: PhantomData,
        }
    }

    /// One area per channel over a single interleaved buffer of `channels`
    /// channels of `sample_bytes`-byte samples.
    ///
    /// The areas alias the buffer; they are not `Send`, and every operation
    /// needs `&mut` access to one area at a time, so no two views are ever
    /// used simultaneously.
    pub fn interleaved(buf: &'a mut [u8], channels: usize, sample_bytes: usize) -> Vec<Area<'a>> {
        let (ptr, len) = (buf.as_mut_ptr(), buf.len());
        (0..channels)
            .map(|c| Area {
                ptr,
                len,
                first: c * sample_bytes,
                step: channels * sample_bytes,
                _buf: PhantomData,
            })
            .collect()
    }

    /// A view onto raw memory.
    ///
    /// # Safety
    /// `ptr` must be valid for reads and writes of `len` bytes for `'a`, and
    /// while an operation on this area runs nothing else may access those
    /// bytes through another reference.
    #[allow(unsafe_code)]
    pub unsafe fn from_raw(ptr: *mut u8, len: usize, first: usize, step: usize) -> Area<'a> {
        Area {
            ptr,
            len,
            first,
            step,
            _buf: PhantomData,
        }
    }

    /// The byte range `[start, start + span)` covering `frames` samples of
    /// `w` bytes from frame `offset`, or `None` if it leaves the buffer.
    fn window(&self, offset: usize, frames: usize, w: usize) -> Option<(usize, usize)> {
        if frames == 0 {
            return Some((0, 0));
        }
        if frames > 1 && self.step < w {
            return None;
        }
        let start = self.step.checked_mul(offset)?.checked_add(self.first)?;
        let span = self.step.checked_mul(frames - 1)?.checked_add(w)?;
        (start.checked_add(span)? <= self.len).then_some((start, span))
    }

    #[allow(unsafe_code)]
    fn bytes(&self, start: usize, span: usize) -> &[u8] {
        // SAFETY: `window` verified `start + span <= len`, and the
        // constructor's contract makes `ptr..ptr+len` valid for reads.
        unsafe { core::slice::from_raw_parts(self.ptr.add(start), span) }
    }

    #[allow(unsafe_code)]
    fn bytes_mut(&mut self, start: usize, span: usize) -> &mut [u8] {
        // SAFETY: as in `bytes`; `&mut self` makes this the only reference
        // derived from this area, and the constructor's contract excludes
        // other users of the bytes while an operation runs.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.add(start), span) }
    }
}

fn gather<C: Codec, T: Copy>(
    area: &Area<'_>,
    offset: usize,
    out: &mut [T],
    dec: fn([u8; 4]) -> T,
) -> bool {
    let Some((start, span)) = area.window(offset, out.len(), C::W) else {
        return false;
    };
    let src = area.bytes(start, span);
    for (i, o) in out.iter_mut().enumerate() {
        let at = i * area.step;
        let mut t = [0u8; 4];
        match src.get(at..at + C::W) {
            Some(b) => t[..C::W].copy_from_slice(b),
            None => return false,
        }
        *o = dec(t);
    }
    true
}

fn scatter<C: Codec, T: Copy>(
    area: &mut Area<'_>,
    offset: usize,
    src: &[T],
    enc: fn(T) -> [u8; 4],
) -> bool {
    let Some((start, span)) = area.window(offset, src.len(), C::W) else {
        return false;
    };
    let step = area.step;
    let dst = area.bytes_mut(start, span);
    for (i, v) in src.iter().enumerate() {
        let at = i * step;
        let t = enc(*v);
        match dst.get_mut(at..at + C::W) {
            Some(b) => b.copy_from_slice(&t[..C::W]),
            None => return false,
        }
    }
    true
}

/// Reads `out.len()` samples of `fmt` starting at frame `offset` of `area`
/// into left-aligned `i32`. Returns false if the area is too small.
pub fn load_i32(fmt: PcmFormat, area: &Area<'_>, offset: usize, out: &mut [i32]) -> bool {
    macro_rules! go {
        ($c:ty) => {
            gather::<$c, i32>(area, offset, out, <$c as Codec>::dec_i32)
        };
    }
    match fmt {
        PcmFormat::S16 => go!(S16),
        PcmFormat::S24 => go!(S24),
        PcmFormat::S24Packed => go!(S24Packed),
        PcmFormat::S32 => go!(S32),
        PcmFormat::Float => go!(Float),
    }
}

/// Reads samples as `f32`.
pub fn load_f32(fmt: PcmFormat, area: &Area<'_>, offset: usize, out: &mut [f32]) -> bool {
    macro_rules! go {
        ($c:ty) => {
            gather::<$c, f32>(area, offset, out, <$c as Codec>::dec_f32)
        };
    }
    match fmt {
        PcmFormat::S16 => go!(S16),
        PcmFormat::S24 => go!(S24),
        PcmFormat::S24Packed => go!(S24Packed),
        PcmFormat::S32 => go!(S32),
        PcmFormat::Float => go!(Float),
    }
}

/// Writes left-aligned `i32` samples as `fmt` (rounding when narrowing).
pub fn store_i32(fmt: PcmFormat, area: &mut Area<'_>, offset: usize, src: &[i32]) -> bool {
    macro_rules! go {
        ($c:ty) => {
            scatter::<$c, i32>(area, offset, src, <$c as Codec>::enc_i32)
        };
    }
    match fmt {
        PcmFormat::S16 => go!(S16),
        PcmFormat::S24 => go!(S24),
        PcmFormat::S24Packed => go!(S24Packed),
        PcmFormat::S32 => go!(S32),
        PcmFormat::Float => go!(Float),
    }
}

/// Writes `f32` samples as `fmt`.
pub fn store_f32(fmt: PcmFormat, area: &mut Area<'_>, offset: usize, src: &[f32]) -> bool {
    macro_rules! go {
        ($c:ty) => {
            scatter::<$c, f32>(area, offset, src, <$c as Codec>::enc_f32)
        };
    }
    match fmt {
        PcmFormat::S16 => go!(S16),
        PcmFormat::S24 => go!(S24),
        PcmFormat::S24Packed => go!(S24Packed),
        PcmFormat::S32 => go!(S32),
        PcmFormat::Float => go!(Float),
    }
}

/// Decodes interleaved samples of `fmt` from `bytes` into `out` (`f32` in
/// `[-1, 1)`): `out.len()` samples, `bytes` must hold at least that many.
/// Samples past the end of `bytes` decode as 0.
pub fn decode_interleaved(fmt: PcmFormat, bytes: &[u8], out: &mut [f32]) {
    fn go<C: Codec>(bytes: &[u8], out: &mut [f32]) {
        for (o, b) in out.iter_mut().zip(bytes.chunks(C::W)) {
            let mut t = [0u8; 4];
            let n = b.len().min(C::W);
            t[..n].copy_from_slice(&b[..n]);
            *o = C::dec_f32(t);
        }
        let done = bytes.len() / C::W;
        if done < out.len() {
            out[done..].fill(0.0);
        }
    }
    match fmt {
        PcmFormat::S16 => go::<S16>(bytes, out),
        PcmFormat::S24 => go::<S24>(bytes, out),
        PcmFormat::S24Packed => go::<S24Packed>(bytes, out),
        PcmFormat::S32 => go::<S32>(bytes, out),
        PcmFormat::Float => go::<Float>(bytes, out),
    }
}

/// Encodes `samples` (`f32`, rounded and saturated for integer formats) as
/// interleaved `fmt` into `bytes`, which must hold `samples.len()` samples;
/// a shorter `bytes` receives a prefix.
pub fn encode_interleaved(fmt: PcmFormat, samples: &[f32], bytes: &mut [u8]) {
    fn go<C: Codec>(samples: &[f32], bytes: &mut [u8]) {
        for (s, b) in samples.iter().zip(bytes.chunks_mut(C::W)) {
            let t = C::enc_f32(*s);
            let n = b.len().min(C::W);
            b[..n].copy_from_slice(&t[..n]);
        }
    }
    match fmt {
        PcmFormat::S16 => go::<S16>(samples, bytes),
        PcmFormat::S24 => go::<S24>(samples, bytes),
        PcmFormat::S24Packed => go::<S24Packed>(samples, bytes),
        PcmFormat::S32 => go::<S32>(samples, bytes),
        PcmFormat::Float => go::<Float>(samples, bytes),
    }
}

/// A ring sample type (`i32` or `f32`) with its PCM conversions.
pub trait RingSample: Sample + Default + PartialEq + core::fmt::Debug {
    /// The ring's sample format.
    const RING: SampleFormat;
    /// Reads samples from an area (see [`load_i32`]).
    fn load(fmt: PcmFormat, area: &Area<'_>, offset: usize, out: &mut [Self]) -> bool;
    /// Writes samples to an area (see [`store_i32`]).
    fn store(fmt: PcmFormat, area: &mut Area<'_>, offset: usize, src: &[Self]) -> bool;
}

impl RingSample for i32 {
    const RING: SampleFormat = SampleFormat::I32;
    fn load(fmt: PcmFormat, area: &Area<'_>, offset: usize, out: &mut [i32]) -> bool {
        load_i32(fmt, area, offset, out)
    }
    fn store(fmt: PcmFormat, area: &mut Area<'_>, offset: usize, src: &[i32]) -> bool {
        store_i32(fmt, area, offset, src)
    }
}

impl RingSample for f32 {
    const RING: SampleFormat = SampleFormat::F32;
    fn load(fmt: PcmFormat, area: &Area<'_>, offset: usize, out: &mut [f32]) -> bool {
        load_f32(fmt, area, offset, out)
    }
    fn store(fmt: PcmFormat, area: &mut Area<'_>, offset: usize, src: &[f32]) -> bool {
        store_f32(fmt, area, offset, src)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_i32(fmt: PcmFormat, values: &[i32]) -> Vec<i32> {
        let mut buf = vec![0xAAu8; values.len() * fmt.bytes() + 7];
        let mut a = Area::from_slice(&mut buf, 0, fmt.bytes());
        assert!(store_i32(fmt, &mut a, 0, values));
        let mut out = vec![0i32; values.len()];
        assert!(load_i32(fmt, &a, 0, &mut out));
        out
    }

    #[test]
    fn s32_is_exact() {
        let v = [i32::MIN, -1, 0, 1, i32::MAX, 0x1234_5678];
        assert_eq!(roundtrip_i32(PcmFormat::S32, &v), v);
    }

    #[test]
    fn narrower_formats_are_exact_on_their_own_grid() {
        let v16: Vec<i32> = [-32768i32, -1, 0, 1, 32767, 12345]
            .iter()
            .map(|x| x << 16)
            .collect();
        assert_eq!(roundtrip_i32(PcmFormat::S16, &v16), v16);
        let v24: Vec<i32> = [-8_388_608i32, -1, 0, 1, 8_388_607, 123_456]
            .iter()
            .map(|x| x << 8)
            .collect();
        assert_eq!(roundtrip_i32(PcmFormat::S24, &v24), v24);
        assert_eq!(roundtrip_i32(PcmFormat::S24Packed, &v24), v24);
    }

    #[test]
    fn narrowing_rounds_and_saturates() {
        // 0x8000 is half an S16 LSB: rounds up.
        let out = roundtrip_i32(
            PcmFormat::S16,
            &[0x0000_8000, 0x0000_7FFF, i32::MAX, i32::MIN],
        );
        assert_eq!(out, [0x0001_0000, 0, 0x7FFF_0000, i32::MIN]);
        let out = roundtrip_i32(PcmFormat::S24Packed, &[i32::MAX]);
        assert_eq!(out, [0x7FFF_FF00]);
    }

    #[test]
    fn s24_wire_layouts() {
        // S24_LE in a 32-bit word, sign-extended on store, high byte ignored on load.
        let mut buf = [0u8; 8];
        let mut a = Area::from_slice(&mut buf, 0, 4);
        assert!(store_i32(PcmFormat::S24, &mut a, 0, &[-256, 0x0012_3400]));
        assert_eq!(buf, [0xFF, 0xFF, 0xFF, 0xFF, 0x34, 0x12, 0x00, 0x00]);
        let mut buf = [0x00, 0xFF, 0xFF, 0x55, 0x00, 0x34, 0x12, 0x00];
        let a = Area::from_slice(&mut buf, 0, 4);
        let mut out = [0i32; 2];
        assert!(load_i32(PcmFormat::S24, &a, 0, &mut out));
        assert_eq!(out, [-65536, 0x1234_0000]);
        // S24_3LE is packed.
        let mut buf = [0u8; 6];
        let mut a = Area::from_slice(&mut buf, 0, 3);
        assert!(store_i32(
            PcmFormat::S24Packed,
            &mut a,
            0,
            &[0x0102_0300, -0x0100]
        ));
        assert_eq!(buf, [0x03, 0x02, 0x01, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn float_conversions() {
        let mut buf = [0u8; 12];
        let mut a = Area::from_slice(&mut buf, 0, 4);
        assert!(store_f32(PcmFormat::Float, &mut a, 0, &[0.5, -1.0, 1.0]));
        let mut out = [0i32; 3];
        assert!(load_i32(PcmFormat::Float, &a, 0, &mut out));
        assert_eq!(out, [1 << 30, i32::MIN, i32::MAX]);
        let mut f = [0.0f32; 3];
        assert!(load_f32(PcmFormat::Float, &a, 0, &mut f));
        assert_eq!(f, [0.5, -1.0, 1.0]);
        assert_eq!(i32_to_f32(1 << 30), 0.5);
        assert_eq!(f32_to_i32(f32::NAN), 0);
        assert_eq!(f32_to_i32(2.0), i32::MAX);
        assert_eq!(f32_to_i32(-2.0), i32::MIN);
    }

    #[test]
    fn interleaved_and_planar_addressing() {
        // Two interleaved S16 channels: frame i is [L, R] at 4 * i.
        let mut buf = vec![0u8; 4 * 4];
        for i in 0..4usize {
            buf[4 * i..4 * i + 2].copy_from_slice(&(10 + i as i16).to_le_bytes());
            buf[4 * i + 2..4 * i + 4].copy_from_slice(&(-(20 + i as i16)).to_le_bytes());
        }
        let l = Area::from_slice(&mut buf, 0, 4);
        let mut out = [0i32; 3];
        assert!(load_i32(PcmFormat::S16, &l, 1, &mut out));
        assert_eq!(out, [11 << 16, 12 << 16, 13 << 16]);
        let r = Area::from_slice(&mut buf, 2, 4);
        assert!(load_i32(PcmFormat::S16, &r, 0, &mut out));
        assert_eq!(out, [-20 << 16, -21 << 16, -22 << 16]);
    }

    #[test]
    fn out_of_bounds_is_refused_not_read() {
        let mut buf = [0u8; 16];
        let a = Area::from_slice(&mut buf, 0, 4);
        let mut out = [0i32; 5];
        assert!(!load_i32(PcmFormat::S32, &a, 0, &mut out));
        let mut out = [0i32; 4];
        assert!(load_i32(PcmFormat::S32, &a, 0, &mut out));
        assert!(!load_i32(PcmFormat::S32, &a, 1, &mut out));
        // A step smaller than the sample is nonsense.
        let a = Area::from_slice(&mut buf, 0, 2);
        assert!(!load_i32(PcmFormat::S32, &a, 0, &mut out));
        // Overflowing arithmetic must not wrap into range.
        let a = Area::from_slice(&mut buf, usize::MAX, 4);
        assert!(!load_i32(PcmFormat::S32, &a, 0, &mut out[..1]));
        let a = Area::from_slice(&mut buf, 0, usize::MAX);
        assert!(!load_i32(PcmFormat::S32, &a, 2, &mut out[..2]));
    }

    #[test]
    fn format_ids_roundtrip() {
        for f in PcmFormat::ALL {
            assert_eq!(PcmFormat::from_alsa(f.to_alsa()), Some(f));
        }
        assert_eq!(PcmFormat::from_alsa(3), None);
    }

    #[test]
    fn interleaved_codecs_round_trip_and_handle_short_input() {
        let samples = [0.0f32, 0.5, -0.5, 0.25, -1.0, 0.999, 1.0, -0.125];
        for fmt in PcmFormat::ALL {
            let mut bytes = vec![0u8; samples.len() * fmt.bytes()];
            encode_interleaved(fmt, &samples, &mut bytes);
            let mut back = [9.0f32; 8];
            decode_interleaved(fmt, &bytes, &mut back);
            let tol = match fmt {
                PcmFormat::S16 => 1.0 / 32768.0,
                PcmFormat::S24 | PcmFormat::S24Packed => 1.0 / 8_388_608.0,
                PcmFormat::S32 => 1e-6,
                PcmFormat::Float => 0.0,
            };
            for (a, b) in samples.iter().zip(&back) {
                // Full scale saturates to the largest code (one step below 1.0).
                assert!((a - b).abs() <= tol, "{fmt:?}: {a} -> {b}");
            }
            // A short input decodes as far as it goes and the rest is silence.
            let mut part = [9.0f32; 8];
            decode_interleaved(fmt, &bytes[..3 * fmt.bytes() + 1], &mut part);
            assert_eq!(part[3..], [0.0; 5], "{fmt:?}");
            // A short output buffer receives a prefix and nothing panics.
            let mut small = vec![0u8; fmt.bytes() * 2 + 1];
            encode_interleaved(fmt, &samples, &mut small);
        }
    }
}
