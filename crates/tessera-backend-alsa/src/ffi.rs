//! Hand-written mirrors of the alsa-lib ioplug ABI (`<alsa/pcm_external.h>`)
//! and the few libasound entry points the plugin calls.
//!
//! No C compiler or build script is involved. The layouts are checked against
//! the installed header by `tests/abi_layout.rs`, which compiles a small C
//! program with the host compiler and compares `sizeof`/`offsetof`.
#![allow(unsafe_code, non_camel_case_types, non_upper_case_globals)]
#![allow(missing_docs)]

use core::ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void};

pub type snd_pcm_uframes_t = c_ulong;
pub type snd_pcm_sframes_t = c_long;

/// An opaque C type.
#[repr(C)]
#[derive(Debug)]
pub struct Opaque {
    _p: [u8; 0],
}
pub type snd_pcm_t = Opaque;
pub type snd_config_t = Opaque;
pub type snd_pcm_hw_params_t = Opaque;
pub type snd_pcm_sw_params_t = Opaque;
pub type snd_output_t = Opaque;
pub type snd_pcm_chmap_t = Opaque;
pub type snd_pcm_chmap_query_t = Opaque;

pub const SND_PCM_STREAM_PLAYBACK: c_int = 0;
pub const SND_PCM_STREAM_CAPTURE: c_int = 1;

pub const SND_PCM_STATE_OPEN: c_int = 0;
pub const SND_PCM_STATE_SETUP: c_int = 1;
pub const SND_PCM_STATE_PREPARED: c_int = 2;
pub const SND_PCM_STATE_RUNNING: c_int = 3;
pub const SND_PCM_STATE_XRUN: c_int = 4;
pub const SND_PCM_STATE_DRAINING: c_int = 5;
pub const SND_PCM_STATE_PAUSED: c_int = 6;
pub const SND_PCM_STATE_SUSPENDED: c_int = 7;
pub const SND_PCM_STATE_DISCONNECTED: c_int = 8;

pub const SND_PCM_ACCESS_MMAP_INTERLEAVED: c_uint = 0;
pub const SND_PCM_ACCESS_MMAP_NONINTERLEAVED: c_uint = 1;
pub const SND_PCM_ACCESS_MMAP_COMPLEX: c_uint = 2;
pub const SND_PCM_ACCESS_RW_INTERLEAVED: c_uint = 3;
pub const SND_PCM_ACCESS_RW_NONINTERLEAVED: c_uint = 4;

pub const SND_PCM_FORMAT_S16_LE: c_uint = 2;
pub const SND_PCM_FORMAT_S24_LE: c_uint = 6;
pub const SND_PCM_FORMAT_S32_LE: c_uint = 10;
pub const SND_PCM_FORMAT_FLOAT_LE: c_uint = 14;
pub const SND_PCM_FORMAT_S24_3LE: c_uint = 32;

pub const SND_PCM_IOPLUG_HW_ACCESS: c_int = 0;
pub const SND_PCM_IOPLUG_HW_FORMAT: c_int = 1;
pub const SND_PCM_IOPLUG_HW_CHANNELS: c_int = 2;
pub const SND_PCM_IOPLUG_HW_RATE: c_int = 3;
pub const SND_PCM_IOPLUG_HW_PERIOD_BYTES: c_int = 4;
pub const SND_PCM_IOPLUG_HW_BUFFER_BYTES: c_int = 5;
pub const SND_PCM_IOPLUG_HW_PERIODS: c_int = 6;

pub const SND_PCM_IOPLUG_FLAG_BOUNDARY_WA: c_uint = 1 << 2;
/// `SND_PCM_IOPLUG_VERSION` 1.0.2.
pub const SND_PCM_IOPLUG_VERSION: c_uint = (1 << 16) | 2;

pub const SND_CONFIG_TYPE_COMPOUND: c_int = 1024;

/// `snd_pcm_channel_area_t`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct snd_pcm_channel_area_t {
    pub addr: *mut c_void,
    /// Offset of the first sample, in bits.
    pub first: c_uint,
    /// Distance between samples, in bits.
    pub step: c_uint,
}

/// `snd_pcm_ioplug_t`.
#[repr(C)]
#[derive(Debug)]
pub struct snd_pcm_ioplug_t {
    pub version: c_uint,
    pub name: *const c_char,
    pub flags: c_uint,
    pub poll_fd: c_int,
    pub poll_events: c_uint,
    pub mmap_rw: c_uint,
    pub callback: *const snd_pcm_ioplug_callback_t,
    pub private_data: *mut c_void,
    pub pcm: *mut snd_pcm_t,
    pub stream: c_int,
    pub state: c_int,
    pub appl_ptr: snd_pcm_uframes_t,
    pub hw_ptr: snd_pcm_uframes_t,
    pub nonblock: c_int,
    pub access: c_uint,
    pub format: c_uint,
    pub channels: c_uint,
    pub rate: c_uint,
    pub period_size: snd_pcm_uframes_t,
    pub buffer_size: snd_pcm_uframes_t,
}

type io_t = snd_pcm_ioplug_t;

/// `snd_pcm_ioplug_callback_t` (protocol 1.0.2).
#[repr(C)]
#[derive(Debug)]
pub struct snd_pcm_ioplug_callback_t {
    pub start: Option<unsafe extern "C" fn(*mut io_t) -> c_int>,
    pub stop: Option<unsafe extern "C" fn(*mut io_t) -> c_int>,
    pub pointer: Option<unsafe extern "C" fn(*mut io_t) -> snd_pcm_sframes_t>,
    pub transfer: Option<
        unsafe extern "C" fn(
            *mut io_t,
            *const snd_pcm_channel_area_t,
            snd_pcm_uframes_t,
            snd_pcm_uframes_t,
        ) -> snd_pcm_sframes_t,
    >,
    pub close: Option<unsafe extern "C" fn(*mut io_t) -> c_int>,
    pub hw_params: Option<unsafe extern "C" fn(*mut io_t, *mut snd_pcm_hw_params_t) -> c_int>,
    pub hw_free: Option<unsafe extern "C" fn(*mut io_t) -> c_int>,
    pub sw_params: Option<unsafe extern "C" fn(*mut io_t, *mut snd_pcm_sw_params_t) -> c_int>,
    pub prepare: Option<unsafe extern "C" fn(*mut io_t) -> c_int>,
    pub drain: Option<unsafe extern "C" fn(*mut io_t) -> c_int>,
    pub pause: Option<unsafe extern "C" fn(*mut io_t, c_int) -> c_int>,
    pub resume: Option<unsafe extern "C" fn(*mut io_t) -> c_int>,
    pub poll_descriptors_count: Option<unsafe extern "C" fn(*mut io_t) -> c_int>,
    pub poll_descriptors:
        Option<unsafe extern "C" fn(*mut io_t, *mut libc::pollfd, c_uint) -> c_int>,
    pub poll_revents:
        Option<unsafe extern "C" fn(*mut io_t, *mut libc::pollfd, c_uint, *mut u16) -> c_int>,
    pub dump: Option<unsafe extern "C" fn(*mut io_t, *mut snd_output_t)>,
    pub delay: Option<unsafe extern "C" fn(*mut io_t, *mut snd_pcm_sframes_t) -> c_int>,
    pub query_chmaps: Option<unsafe extern "C" fn(*mut io_t) -> *mut *mut snd_pcm_chmap_query_t>,
    pub get_chmap: Option<unsafe extern "C" fn(*mut io_t) -> *mut snd_pcm_chmap_t>,
    pub set_chmap: Option<unsafe extern "C" fn(*mut io_t, *const snd_pcm_chmap_t) -> c_int>,
}

/// `snd_pcm_ioplug_callback_t` has this many members.
pub const CALLBACK_COUNT: usize = 20;

pub type snd_config_iterator_t = *mut c_void;

#[link(name = "asound")]
unsafe extern "C" {
    pub fn snd_pcm_ioplug_create(
        io: *mut snd_pcm_ioplug_t,
        name: *const c_char,
        stream: c_int,
        mode: c_int,
    ) -> c_int;
    pub fn snd_pcm_ioplug_delete(io: *mut snd_pcm_ioplug_t) -> c_int;
    pub fn snd_pcm_ioplug_set_param_minmax(
        io: *mut snd_pcm_ioplug_t,
        kind: c_int,
        min: c_uint,
        max: c_uint,
    ) -> c_int;
    pub fn snd_pcm_ioplug_set_param_list(
        io: *mut snd_pcm_ioplug_t,
        kind: c_int,
        num_list: c_uint,
        list: *const c_uint,
    ) -> c_int;
    pub fn snd_asoundlib_version() -> *const c_char;
    pub fn snd_pcm_ioplug_set_state(io: *mut snd_pcm_ioplug_t, state: c_int) -> c_int;

    pub fn snd_config_iterator_first(config: *const snd_config_t) -> snd_config_iterator_t;
    pub fn snd_config_iterator_next(iterator: snd_config_iterator_t) -> snd_config_iterator_t;
    pub fn snd_config_iterator_end(config: *const snd_config_t) -> snd_config_iterator_t;
    pub fn snd_config_iterator_entry(iterator: snd_config_iterator_t) -> *mut snd_config_t;
    pub fn snd_config_get_id(config: *const snd_config_t, id: *mut *const c_char) -> c_int;
    pub fn snd_config_get_type(config: *const snd_config_t) -> c_int;
    pub fn snd_config_get_ascii(config: *const snd_config_t, value: *mut *mut c_char) -> c_int;

    pub fn snd_pcm_sw_params_get_boundary(
        params: *const snd_pcm_sw_params_t,
        val: *mut snd_pcm_uframes_t,
    ) -> c_int;
    pub fn snd_pcm_sw_params_get_avail_min(
        params: *const snd_pcm_sw_params_t,
        val: *mut snd_pcm_uframes_t,
    ) -> c_int;
    pub fn snd_pcm_sw_params_get_stop_threshold(
        params: *const snd_pcm_sw_params_t,
        val: *mut snd_pcm_uframes_t,
    ) -> c_int;

    /// The error handler; an application may replace it at any time
    /// (`snd_lib_error_set_handler`), so it is read with a volatile load of
    /// its address, never through a reference.
    pub static mut snd_lib_error: Option<
        unsafe extern "C" fn(*const c_char, c_int, *const c_char, c_int, *const c_char, ...),
    >;
}

/// Offsets, sizes and constants of the mirrored ABI, by name; compared with
/// the installed headers by `tests/abi_layout.rs`.
pub fn abi_report() -> Vec<(&'static str, u64)> {
    use core::mem::{offset_of, size_of};
    macro_rules! off {
        ($($f:ident),*) => {
            vec![$((concat!("ioplug.", stringify!($f)), offset_of!(snd_pcm_ioplug_t, $f) as u64)),*]
        };
    }
    macro_rules! cb {
        ($($f:ident),*) => {
            vec![$((concat!("callback.", stringify!($f)), offset_of!(snd_pcm_ioplug_callback_t, $f) as u64)),*]
        };
    }
    let mut v: Vec<(&'static str, u64)> = vec![
        ("sizeof.ioplug", size_of::<snd_pcm_ioplug_t>() as u64),
        (
            "sizeof.callback",
            size_of::<snd_pcm_ioplug_callback_t>() as u64,
        ),
        (
            "count.callback",
            (size_of::<snd_pcm_ioplug_callback_t>() / size_of::<usize>()) as u64,
        ),
        ("sizeof.area", size_of::<snd_pcm_channel_area_t>() as u64),
        ("area.addr", offset_of!(snd_pcm_channel_area_t, addr) as u64),
        (
            "area.first",
            offset_of!(snd_pcm_channel_area_t, first) as u64,
        ),
        ("area.step", offset_of!(snd_pcm_channel_area_t, step) as u64),
        ("sizeof.uframes", size_of::<snd_pcm_uframes_t>() as u64),
        ("sizeof.sframes", size_of::<snd_pcm_sframes_t>() as u64),
        ("sizeof.pollfd", size_of::<libc::pollfd>() as u64),
        ("version", u64::from(SND_PCM_IOPLUG_VERSION)),
        (
            "flag.boundary_wa",
            u64::from(SND_PCM_IOPLUG_FLAG_BOUNDARY_WA),
        ),
        ("stream.playback", SND_PCM_STREAM_PLAYBACK as u64),
        ("stream.capture", SND_PCM_STREAM_CAPTURE as u64),
        ("state.open", SND_PCM_STATE_OPEN as u64),
        ("state.setup", SND_PCM_STATE_SETUP as u64),
        ("state.prepared", SND_PCM_STATE_PREPARED as u64),
        ("state.running", SND_PCM_STATE_RUNNING as u64),
        ("state.xrun", SND_PCM_STATE_XRUN as u64),
        ("state.draining", SND_PCM_STATE_DRAINING as u64),
        ("state.paused", SND_PCM_STATE_PAUSED as u64),
        ("state.suspended", SND_PCM_STATE_SUSPENDED as u64),
        ("state.disconnected", SND_PCM_STATE_DISCONNECTED as u64),
        (
            "access.mmap_interleaved",
            u64::from(SND_PCM_ACCESS_MMAP_INTERLEAVED),
        ),
        (
            "access.mmap_noninterleaved",
            u64::from(SND_PCM_ACCESS_MMAP_NONINTERLEAVED),
        ),
        (
            "access.mmap_complex",
            u64::from(SND_PCM_ACCESS_MMAP_COMPLEX),
        ),
        (
            "access.rw_interleaved",
            u64::from(SND_PCM_ACCESS_RW_INTERLEAVED),
        ),
        (
            "access.rw_noninterleaved",
            u64::from(SND_PCM_ACCESS_RW_NONINTERLEAVED),
        ),
        ("format.s16_le", u64::from(SND_PCM_FORMAT_S16_LE)),
        ("format.s24_le", u64::from(SND_PCM_FORMAT_S24_LE)),
        ("format.s32_le", u64::from(SND_PCM_FORMAT_S32_LE)),
        ("format.float_le", u64::from(SND_PCM_FORMAT_FLOAT_LE)),
        ("format.s24_3le", u64::from(SND_PCM_FORMAT_S24_3LE)),
        ("hw.access", SND_PCM_IOPLUG_HW_ACCESS as u64),
        ("hw.format", SND_PCM_IOPLUG_HW_FORMAT as u64),
        ("hw.channels", SND_PCM_IOPLUG_HW_CHANNELS as u64),
        ("hw.rate", SND_PCM_IOPLUG_HW_RATE as u64),
        ("hw.period_bytes", SND_PCM_IOPLUG_HW_PERIOD_BYTES as u64),
        ("hw.buffer_bytes", SND_PCM_IOPLUG_HW_BUFFER_BYTES as u64),
        ("hw.periods", SND_PCM_IOPLUG_HW_PERIODS as u64),
        ("config.compound", SND_CONFIG_TYPE_COMPOUND as u64),
    ];
    v.extend(off!(
        version,
        name,
        flags,
        poll_fd,
        poll_events,
        mmap_rw,
        callback,
        private_data,
        pcm,
        stream,
        state,
        appl_ptr,
        hw_ptr,
        nonblock,
        access,
        format,
        channels,
        rate,
        period_size,
        buffer_size
    ));
    v.extend(cb!(
        start,
        stop,
        pointer,
        transfer,
        close,
        hw_params,
        hw_free,
        sw_params,
        prepare,
        drain,
        pause,
        resume,
        poll_descriptors_count,
        poll_descriptors,
        poll_revents,
        dump,
        delay,
        query_chmaps,
        get_chmap,
        set_chmap
    ));
    v
}
