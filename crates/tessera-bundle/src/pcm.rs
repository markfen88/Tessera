//! Which sample formats a sound card plays, asked of the kernel's ALSA PCM interface
//! (`SNDRV_PCM_IOCTL_HW_REFINE` on the card's `/dev/snd/pcmC<card>D<device>p` node): the
//! query `snd_pcm_hw_params_any` makes in libasound, without libasound. The installer
//! runs before the packages that depend on `libasound2` are installed, so it cannot link
//! the library, and the question is a single read-only ioctl.
//!
//! The one `unsafe` call is that ioctl on a descriptor this module opened and a
//! `repr(C)` structure it owns; everything the kernel writes is plain integers.
#![allow(unsafe_code)]

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// `SNDRV_MASK_MAX` (256) bits in 32-bit words.
const MASK_WORDS: usize = 8;
/// Index of `SNDRV_PCM_HW_PARAM_FORMAT` among the masks (after `ACCESS`).
const FORMAT: usize = 1;

/// The playback formats the seeded configuration may name, best first: CamillaDSP's
/// spelling for an ALSA device (4.1.3 reads `S16_LE`, `S24_3_LE`, `S24_4_LE`, `S32_LE`,
/// `F32_LE`, `F64_LE` and rejects ALSA's own `S24_3LE` and `S24_LE`), with the number
/// ALSA gives the format (`SNDRV_PCM_FORMAT_*`: `S32_LE` 10, `S24_3LE` 32, `S24_LE` 6
/// and `S16_LE` 2). The first that the card plays is used.
pub const PREFERRED: [(&str, u32); 4] = [
    ("S32_LE", 10),
    ("S24_3_LE", 32),
    ("S24_4_LE", 6),
    ("S16_LE", 2),
];

/// `struct snd_mask`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Mask {
    bits: [u32; MASK_WORDS],
}

/// `struct snd_interval` (the four flag bits share one word).
#[repr(C)]
#[derive(Clone, Copy)]
struct Interval {
    min: u32,
    max: u32,
    flags: u32,
}

/// `struct snd_pcm_hw_params`.
#[repr(C)]
struct HwParams {
    flags: u32,
    /// `ACCESS`, `FORMAT`, `SUBFORMAT`.
    masks: [Mask; 3],
    mres: [Mask; 5],
    /// `SAMPLE_BITS` to `TICK_TIME`.
    intervals: [Interval; 12],
    ires: [Interval; 9],
    rmask: u32,
    cmask: u32,
    info: u32,
    msbits: u32,
    rate_num: u32,
    rate_den: u32,
    /// `snd_pcm_uframes_t`, an `unsigned long`.
    fifo_size: std::ffi::c_ulong,
    reserved: [u8; 64],
}

impl HwParams {
    /// Every format, access, rate, channel count and size allowed, and every
    /// parameter requested: what `snd_pcm_hw_params_any` sends to be narrowed.
    fn any() -> Self {
        let full = Mask {
            bits: [u32::MAX; MASK_WORDS],
        };
        let none = Mask {
            bits: [0; MASK_WORDS],
        };
        let open = Interval {
            min: 0,
            max: u32::MAX,
            flags: 0,
        };
        let unused = Interval {
            min: 0,
            max: 0,
            flags: 0,
        };
        Self {
            flags: 0,
            masks: [full; 3],
            mres: [none; 5],
            intervals: [open; 12],
            ires: [unused; 9],
            rmask: u32::MAX,
            cmask: 0,
            info: u32::MAX,
            msbits: 0,
            rate_num: 0,
            rate_den: 0,
            fifo_size: 0,
            reserved: [0; 64],
        }
    }
}

mod sys {
    use super::HwParams;

    // SNDRV_PCM_IOCTL_HW_REFINE = _IOWR('A', 0x10, struct snd_pcm_hw_params)
    nix::ioctl_readwrite!(hw_refine, b'A', 0x10, HwParams);
}

/// The first of [`PREFERRED`] that a format mask holds, by its name.
#[must_use]
pub fn first_supported(mask: &[u32; MASK_WORDS]) -> Option<&'static str> {
    PREFERRED
        .iter()
        .find(|(_, number)| (mask[(number / 32) as usize] >> (number % 32)) & 1 == 1)
        .map(|(name, _)| *name)
}

/// The format mask of the playback device node `path`: bit `n` is set when
/// `SNDRV_PCM_FORMAT_*` number `n` is played. The node is opened without blocking and
/// closed again; nothing is configured, but a device that is in use cannot be opened.
///
/// # Errors
/// The node cannot be opened (no such card, in use, no permission) or is not a PCM device.
pub fn playback_formats(path: &Path) -> io::Result<[u32; MASK_WORDS]> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)?;
    let mut params = HwParams::any();
    // SAFETY: `file` is an open descriptor for the whole call and `params` is a live,
    // exclusively borrowed `repr(C)` structure of the size the request code was built
    // from; the kernel reads and writes within it.
    unsafe { sys::hw_refine(file.as_raw_fd(), &mut params) }.map_err(io::Error::from)?;
    Ok(params.masks[FORMAT].bits)
}

/// The playback format for the seeded configuration of card `index`, device `device`:
/// the first of [`PREFERRED`] the card plays. `None` when the card cannot be asked.
#[must_use]
pub fn playback_format(index: u32, device: u32) -> Option<&'static str> {
    let path = format!("/dev/snd/pcmC{index}D{device}p");
    first_supported(&playback_formats(Path::new(&path)).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(numbers: &[u32]) -> [u32; MASK_WORDS] {
        let mut bits = [0; MASK_WORDS];
        for number in numbers {
            bits[(number / 32) as usize] |= 1 << (number % 32);
        }
        bits
    }

    #[test]
    fn the_structure_has_the_kernels_layout() {
        use std::mem::{offset_of, size_of};
        assert_eq!(size_of::<Mask>(), 32);
        assert_eq!(size_of::<Interval>(), 12);
        assert_eq!(offset_of!(HwParams, masks), 4);
        assert_eq!(offset_of!(HwParams, mres), 100);
        assert_eq!(offset_of!(HwParams, intervals), 260);
        assert_eq!(offset_of!(HwParams, ires), 404);
        assert_eq!(offset_of!(HwParams, rmask), 512);
        assert_eq!(offset_of!(HwParams, reserved) % 8, 0);
        if size_of::<std::ffi::c_ulong>() == 8 {
            // SNDRV_PCM_IOCTL_HW_REFINE on a 64-bit kernel: _IOWR('A', 0x10, 608 bytes).
            assert_eq!(size_of::<HwParams>(), 608);
            assert_eq!(offset_of!(HwParams, fifo_size), 536);
            assert_eq!(
                nix::request_code_readwrite!(b'A', 0x10, size_of::<HwParams>()),
                0xc260_4110
            );
        }
    }

    #[test]
    fn the_seeded_names_are_the_ones_camilladsp_reads_for_an_alsa_device() {
        // CamillaDSP 4.1.3 (`camilladsp -c`): these six are valid, `S24_3LE` and `S24_LE`
        // (ALSA's spelling) are "unknown variant".
        const VALID: [&str; 6] = [
            "S16_LE", "S24_3_LE", "S24_4_LE", "S32_LE", "F32_LE", "F64_LE",
        ];
        for (name, _) in PREFERRED {
            assert!(VALID.contains(&name), "{name}");
        }
        assert_eq!(PREFERRED[0].0, crate::dsp::DEFAULT_FORMAT);
    }

    #[test]
    fn the_first_preferred_format_the_card_plays_wins() {
        // ALSA's format numbers: S16_LE 2, S24_LE 6, S32_LE 10, S24_3LE 32; the names are
        // CamillaDSP's.
        for (numbers, expected) in [
            (&[2, 6, 10, 32][..], Some("S32_LE")),
            (&[2, 6, 32], Some("S24_3_LE")),
            (&[2, 6], Some("S24_4_LE")),
            (&[2], Some("S16_LE")),
            // A USB DAC with 24-bit packed and 16-bit only.
            (&[2, 32], Some("S24_3_LE")),
            // Float and 8-bit only (a card that plays none of the four).
            (&[14, 1], None),
            (&[], None),
        ] {
            assert_eq!(first_supported(&mask(numbers)), expected, "{numbers:?}");
        }
    }

    #[test]
    fn a_node_that_is_missing_or_not_a_pcm_device_is_not_asked_twice() {
        let missing = playback_formats(Path::new("/dev/snd/pcmC254D254p")).unwrap_err();
        assert!(
            matches!(
                missing.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ),
            "{missing}"
        );
        // /dev/null opens, and the PCM request is refused as the wrong kind of device.
        assert!(playback_formats(Path::new("/dev/null")).is_err());
        assert_eq!(playback_format(254, 254), None);
    }
}
