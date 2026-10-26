//! The alsa-lib *ioplug* glue of `pcm.tessera`.
//!
//! Everything with logic lives in safe modules ([`crate::stream`],
//! [`crate::client`], [`crate::settings`], [`crate::convert`]); this file only
//! translates between alsa-lib's C callbacks and those. Every `extern "C"`
//! entry point catches panics and returns `-errno` (ADR 0009), writes nothing
//! to stdout, and never blocks on anything the daemon controls.
//!
//! # `unsafe`
//!
//! This module and `ffi.rs` are the only places of the crate that opt out of
//! `unsafe_code = "deny"` (ADR 0002). The blocks are of three kinds, each
//! with a `SAFETY` comment at the site: dereferencing the `snd_pcm_ioplug_t`
//! pointer alsa-lib passes to a callback (valid for the callback's duration
//! and owned by [`Plugin`] until `close`), calling libasound functions with
//! pointers it handed us, and wrapping the application's channel areas.
#![allow(unsafe_code)]

use core::ffi::{CStr, c_char, c_int, c_long, c_uint, c_ulong};
use std::cell::UnsafeCell;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tessera_audio_ipc::{
    ClockOverlayReader, Ring, RingParams, SampleFormat, monotonic_ns, raw_monotonic_ns,
};

use crate::client::{Link, connect_session};
use crate::clock::OverlayClock;
use crate::convert::{Area, PcmFormat};
use crate::error::Error;
use crate::ffi::*;
use crate::settings::{Dir, ENV_SOCKET, Node, Settings, env_socket, from_tree};
use crate::stream::{PcmStream, Pointer, Role, Stream, StreamConfig, StreamError, Wake};

/// `snd_pcm_uframes_t` (`c_ulong`) as `u64`: 32 bits wide on armv7, so a
/// widening there and the identity on 64-bit targets.
#[inline]
#[allow(clippy::unnecessary_cast)]
fn ul(v: c_ulong) -> u64 {
    v as u64
}

/// Runs `f`, returning `on_panic` if it panics. Every callback goes through
/// this: unwinding into alsa-lib's C frames is undefined behaviour.
pub fn guard<R>(on_panic: R, f: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(on_panic)
}

/// Mutable state, behind the plugin's mutex.
struct Inner {
    link: Link,
    ring: Ring,
    overlay: Arc<ClockOverlayReader>,
    params: RingParams,
    stream: Option<Box<dyn PcmStream + Send>>,
    /// Reused per transfer: no allocation in the copy path.
    areas: AreaCache,
    last_pos: u64,
    sock_ticks: u32,
    /// The tick assumed until the wake ticks have been measured.
    default_tick: u64,
    /// The deadline the poll timer is armed for (absolute `CLOCK_MONOTONIC_RAW`
    /// ns; [`EXPIRED`] fires at once), `None` when disarmed.
    armed: Option<u64>,
    /// When `pointer` last drained the wake socket.
    last_drain_ns: u64,
    drain_clock: fn() -> u64,
}

/// The channel areas of the transfer in progress.
struct AreaCache(Vec<Area<'static>>);

// SAFETY: the cached areas hold raw pointers into the application's buffers,
// but only within one `cb_transfer` call: the vector is cleared before the
// callback returns, so no pointer outlives the call or is used from another
// thread.
unsafe impl Send for AreaCache {}

/// One open PCM. alsa-lib and `snd_pcm_ioplug_set_state` write `io` through
/// their own pointer while callbacks hold `&Plugin`, hence the `UnsafeCell`.
///
/// # Concurrency
///
/// Every callback takes `inner` for its whole duration. alsa-lib already
/// serialises `start`, `stop`, `pointer`, `transfer` (and `pause`) with its
/// own PCM lock, so `inner` is only ever contended against the callbacks
/// alsa-lib runs *without* that lock: `poll_revents` (called from another
/// thread by applications that poll and write from different threads),
/// `prepare`, `hw_params`, `hw_free`, `sw_params`, `close`. The poll
/// descriptors (`poll_fd`, `timer_fd`) are immutable; the timer is only armed
/// under the lock.
struct Plugin {
    io: UnsafeCell<snd_pcm_ioplug_t>,
    name: CString,
    dir: Dir,
    settings: Settings,
    /// The PCM's poll descriptor: an epoll set holding [`Plugin::timer_fd`]
    /// and, for its hang-up only, the daemon's wake socket (owned by
    /// `inner.link`). See [`arm`].
    poll_fd: OwnedFd,
    /// Readable exactly when the PCM is ready (`avail >= avail_min`, an xrun,
    /// a lost daemon): armed by [`arm`] for the moment that will be true.
    timer_fd: OwnedFd,
    /// ioplug reports pointers modulo the boundary (`BOUNDARY_WA`) rather
    /// than modulo the buffer size.
    wa: bool,
    max_frames: u64,
    avail_min: AtomicU64,
    boundary: AtomicU64,
    /// The last pointer reported, for the callbacks that must answer even if
    /// `inner` is unusable (a poisoned mutex).
    last_pos: AtomicU64,
    inner: Mutex<Inner>,
}

// SAFETY: `io` is only accessed through the raw pointer alsa-lib owns and
// passes to the callbacks (plain integers and pointers, written by alsa-lib
// under its PCM lock); everything else is `Sync` (atomics, a mutex around
// `Send` data, immutable settings).
unsafe impl Sync for Plugin {}

const _: () = {
    const fn assert_sync<T: Sync>() {}
    assert_sync::<Plugin>();
    const fn assert_send<T: Send>() {}
    assert_send::<Inner>();
};

impl Plugin {
    fn lock(&self) -> Option<MutexGuard<'_, Inner>> {
        self.inner.lock().ok()
    }
}

/// Environment variable: set it to `1` and the plugin prints the stream counters
/// to stderr when a stream ends (diagnostics and tests).
pub const ENV_STATS: &str = "TESSERA_ALSA_STATS";

fn report_stats(g: &Inner) {
    if std::env::var_os(ENV_STATS).is_none() {
        return;
    }
    if let Some(s) = g.stream.as_ref() {
        let st = s.stats();
        eprintln!(
            "tessera-stats xruns={} late={} unwritten={} holes={} overrun={}",
            st.xruns, st.late_frames, st.unwritten_frames, st.hole_frames, st.overrun_frames
        );
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        if let Ok(g) = self.inner.lock() {
            report_stats(&g);
            g.link.detach();
        }
    }
}

/// The callback table (exposed for the tests, which call the callbacks of a
/// mock PCM directly).
pub static CALLBACKS: snd_pcm_ioplug_callback_t = snd_pcm_ioplug_callback_t {
    start: Some(cb_start),
    stop: Some(cb_stop),
    pointer: Some(cb_pointer),
    transfer: Some(cb_transfer),
    close: Some(cb_close),
    hw_params: Some(cb_hw_params),
    hw_free: Some(cb_hw_free),
    sw_params: Some(cb_sw_params),
    prepare: Some(cb_prepare),
    drain: None,
    pause: Some(cb_pause),
    resume: None,
    poll_descriptors_count: Some(cb_poll_descriptors_count),
    poll_descriptors: Some(cb_poll_descriptors),
    poll_revents: Some(cb_poll_revents),
    dump: None,
    delay: Some(cb_delay),
    query_chmaps: None,
    get_chmap: None,
    set_chmap: None,
};

/// Reports an error through alsa-lib's error handler (stderr by default).
fn snderr(msg: &str) {
    let clean: String = msg.chars().filter(|c| !c.is_control()).collect();
    let Ok(text) = CString::new(format!("tessera: {clean}")) else {
        return;
    };
    // SAFETY: `snd_lib_error` is libasound's error-handler variable: either
    // null or a variadic function taking (file, line, function, errno,
    // format, ...); the format consumes exactly one `char *`. The application
    // may replace the handler concurrently, so it is loaded once, volatile,
    // from its address.
    unsafe {
        if let Some(f) = ptr::read_volatile(&raw const snd_lib_error) {
            f(
                c"tessera".as_ptr(),
                0,
                c"pcm".as_ptr(),
                0,
                c"%s".as_ptr(),
                text.as_ptr(),
            );
        }
    }
}

/// The plugin of an `io`, if it is one of ours.
///
/// # Safety
/// `io` must be null or a pointer alsa-lib passed to one of our callbacks.
unsafe fn plugin<'a>(io: *mut snd_pcm_ioplug_t) -> Option<&'a Plugin> {
    if io.is_null() {
        return None;
    }
    // SAFETY: caller contract; `private_data` is set to the boxed `Plugin` in
    // `open` and stays valid until `close` frees it.
    let p = unsafe { (*io).private_data } as *const Plugin;
    // SAFETY: as above; a non-null `private_data` is a live `Plugin`.
    unsafe { p.as_ref() }
}

/// The application pointer (modulo the boundary), read the way alsa-lib
/// publishes it (volatile).
///
/// # Safety
/// `io` must be a pointer alsa-lib passed to one of our callbacks.
unsafe fn appl_ptr(io: *mut snd_pcm_ioplug_t) -> u64 {
    // SAFETY: caller contract; the field is `volatile` in the C header.
    ul(unsafe { ptr::read_volatile(&raw const (*io).appl_ptr) })
}

/// The ioplug's current state.
///
/// # Safety
/// As for [`appl_ptr`].
unsafe fn io_state(io: *mut snd_pcm_ioplug_t) -> c_int {
    // SAFETY: caller contract.
    unsafe { ptr::read_volatile(&raw const (*io).state) }
}

/// Marks the PCM disconnected: alsa-lib then answers `-ENODEV`.
///
/// # Safety
/// As for [`appl_ptr`].
unsafe fn disconnect(p: &Plugin, io: *mut snd_pcm_ioplug_t, g: &mut Inner) -> c_int {
    g.link.mark_lost();
    // SAFETY: caller contract; `snd_pcm_ioplug_set_state` only stores the state.
    unsafe { snd_pcm_ioplug_set_state(io, SND_PCM_STATE_DISCONNECTED) };
    // A poller must wake and learn it.
    arm(p, g, Wake::Now);
    -libc::ENODEV
}

/// Maps a stream failure to the callback's return value, updating the
/// ioplug state where alsa-lib expects the plugin to.
///
/// # Safety
/// As for [`appl_ptr`].
unsafe fn fail(p: &Plugin, io: *mut snd_pcm_ioplug_t, g: &mut Inner, e: StreamError) -> c_int {
    match e {
        // SAFETY: caller contract, forwarded.
        StreamError::Lost => unsafe { disconnect(p, io, g) },
        StreamError::Xrun => {
            // SAFETY: caller contract; only stores the state.
            unsafe { snd_pcm_ioplug_set_state(io, SND_PCM_STATE_XRUN) };
            arm(p, g, Wake::Now);
            -libc::EPIPE
        }
        StreamError::Fault(_) => -libc::EIO,
    }
}

/// The timer value that has always expired: arming it makes the descriptor
/// readable at once.
const EXPIRED: u64 = 1;

/// Added to every computed wake-up. The media clock is the daemon's overlay
/// (an affine map of `CLOCK_MONOTONIC_RAW` it keeps correcting); a wake-up a
/// little late is harmless, one too early makes an application that acts on
/// raw poll events transfer less than it asked for.
const WAKE_MARGIN_NS: u64 = 100_000;

/// How often `pointer` runs [`check_daemon`] (for applications that never
/// call `poll_revents`): at most 100 drains a second, each one or two
/// non-blocking `recv` calls.
const DRAIN_EVERY_NS: u64 = 10_000_000;

/// Creates the PCM's poll descriptor: an epoll set of a fresh timerfd
/// (`EPOLLIN`: see [`arm`]) and the daemon's wake socket `wake_fd` for its
/// hang-up only (`EPOLLRDHUP`; `EPOLLHUP`/`EPOLLERR` are always reported), so
/// a dead daemon still wakes a sleeping application at once. Returns
/// `(epoll set, timer)`.
///
/// The wake socket itself is not the descriptor: it is readable after every
/// engine tick, so an application polling it without
/// `snd_pcm_poll_descriptors_revents` (CamillaDSP, anything written against
/// kernel drivers) would spin, reading a few frames per call.
fn poll_set(wake_fd: c_int) -> std::io::Result<(OwnedFd, OwnedFd)> {
    // SAFETY: plain syscall; a non-negative result is a new descriptor.
    let t = unsafe {
        libc::timerfd_create(
            libc::CLOCK_MONOTONIC,
            libc::TFD_CLOEXEC | libc::TFD_NONBLOCK,
        )
    };
    if t < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `t` is a fresh descriptor nothing else owns.
    let timer = unsafe { OwnedFd::from_raw_fd(t) };
    // SAFETY: plain syscall; a non-negative result is a new descriptor.
    let e = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if e < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `e` is a fresh descriptor nothing else owns.
    let set = unsafe { OwnedFd::from_raw_fd(e) };
    for (fd, events) in [
        (timer.as_raw_fd(), libc::EPOLLIN),
        (wake_fd, libc::EPOLLRDHUP),
    ] {
        let mut ev = libc::epoll_event {
            events: events as u32,
            u64: fd as u64,
        };
        // SAFETY: `set` and `fd` are open descriptors; `ev` is a valid event
        // the kernel copies.
        if unsafe { libc::epoll_ctl(set.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, &raw mut ev) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok((set, timer))
}

/// Monotonic nanoseconds until the media clock has advanced `frames`, never
/// less than one engine tick (the daemon delivers and frees audio once per
/// tick, so finer wake-ups would only cost CPU), plus [`WAKE_MARGIN_NS`].
fn wait_ns(g: &Inner, frames: u64) -> u64 {
    let rate = u128::from(g.params.sample_rate_hz.max(1));
    let frames = u128::from(frames.max(tick_for(g)));
    let media_ns = (frames * 1_000_000_000).div_ceil(rate);
    // The media clock runs `rate_ppb` faster than CLOCK_MONOTONIC_RAW.
    let ppb = g
        .overlay
        .try_read()
        .map_or(0, |s| s.rate_ppb)
        .clamp(-100_000_000, 100_000_000);
    let den = (1_000_000_000 + i128::from(ppb)) as u128;
    let mono = (media_ns * 1_000_000_000).div_ceil(den);
    u64::try_from(mono)
        .unwrap_or(u64::MAX)
        .saturating_add(WAKE_MARGIN_NS)
}

/// Makes the poll descriptor say `wake`: readable now, at the moment the
/// media clock gets there, or not until the application acts. Re-arming a
/// timerfd clears an expiry that was not read, so a descriptor that no longer
/// means "ready" stops being readable. Skips the syscall when nothing changes.
fn arm(p: &Plugin, g: &mut Inner, wake: Wake) {
    let now = raw_monotonic_ns();
    let at = match wake {
        Wake::Never => None,
        Wake::Now => Some(EXPIRED),
        Wake::After(frames) => {
            // timerfd uses MONOTONIC; keep media deadlines in RAW and
            // bound how long a projected timer can run before rechecking.
            let at = now.saturating_add(wait_ns(g, frames).min(200_000_000));
            // Already armed a little later than needed (within one tick and
            // the margin): waking that late is harmless, so keep it and save
            // the syscall. Armed *earlier* always re-arms: an early wake-up
            // would let a raw-polling application transfer too little.
            if let Some(t) = g.armed {
                if t >= at && t - at <= wait_ns(g, 0) {
                    return;
                }
            }
            Some(at)
        }
    };
    match (g.armed, at) {
        (None, None) => return,
        (Some(t), Some(EXPIRED)) if t <= now => return,
        _ => {}
    }
    let v = match at {
        None => 0,
        Some(EXPIRED) => EXPIRED,
        Some(target) => {
            let r1 = raw_monotonic_ns();
            let mono = monotonic_ns();
            let r2 = raw_monotonic_ns();
            let raw_mid = r1 + r2.saturating_sub(r1) / 2;
            mono.saturating_add(target.saturating_sub(raw_mid)).max(1)
        }
    };
    let its = libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: libc::timespec {
            tv_sec: (v / 1_000_000_000) as _,
            tv_nsec: (v % 1_000_000_000) as _,
        },
    };
    // SAFETY: `timer_fd` is our open timerfd, `its` a valid value; the old
    // value is not asked for.
    let r = unsafe {
        libc::timerfd_settime(
            p.timer_fd.as_raw_fd(),
            libc::TFD_TIMER_ABSTIME,
            &raw const its,
            ptr::null_mut(),
        )
    };
    // On failure record a state no request matches, so the next call tries
    // again (whatever it asks for).
    g.armed = if r == 0 { at } else { Some(u64::MAX) };
}

/// True if the daemon closed its end of the wake socket. A zero-timeout poll:
/// a drain is fooled by ticks still waiting in the socket (`WakeWait::drain`
/// reports the end of the stream only when nothing came before it), so it
/// sees a hang-up one drain late.
fn hung_up(fd: c_int) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLRDHUP,
        revents: 0,
    };
    // SAFETY: one valid pollfd, no timeout.
    let r = unsafe { libc::poll(&raw mut pfd, 1, 0) };
    r > 0 && pfd.revents & (libc::POLLHUP | libc::POLLRDHUP | libc::POLLERR | libc::POLLNVAL) != 0
}

/// Consumes the wake ticks (they feed the tick estimate) and checks that the
/// daemon is alive: its end of the wake socket is open and it ticked within
/// `overlay_stale` (a daemon that keeps its clock fresh but stopped ticking
/// is broken). Marks the link lost otherwise. `prompt`: also look for a
/// hang-up now (one more syscall) instead of at the next drain; for
/// `poll_revents`, which a hang-up may be the reason for.
fn check_daemon(p: &Plugin, g: &mut Inner, prompt: bool) {
    g.last_drain_ns = (g.drain_clock)();
    g.link.drain_wake();
    if prompt && hung_up(g.link.wake_fd()) {
        g.link.mark_lost();
    }
    let stale = Duration::from_millis(u64::from(p.settings.overlay_stale_ms));
    if !stale.is_zero() && g.link.tick_age() > stale {
        g.link.mark_lost();
    }
}

/// Re-evaluates the poll descriptor for the application pointer `appl` and
/// the ioplug state `state` (the one the PCM is in once the callback returns).
fn rewake(p: &Plugin, g: &mut Inner, appl: u64, state: c_int) {
    let avail_min = p.avail_min.load(Ordering::Relaxed);
    let lost = g.link.is_lost();
    let wake = match (g.stream.as_mut(), state) {
        _ if lost => Wake::Now,
        (Some(s), SND_PCM_STATE_PREPARED | SND_PCM_STATE_RUNNING | SND_PCM_STATE_PAUSED) => {
            s.wake(appl, avail_min, false)
        }
        (Some(s), SND_PCM_STATE_DRAINING) => s.wake(appl, avail_min, true),
        // Setup, xrun, disconnected: the application must look at the state.
        _ => Wake::Now,
    };
    arm(p, g, wake);
}

unsafe extern "C" fn cb_start(io: *mut snd_pcm_ioplug_t) -> c_int {
    guard(-libc::EIO, || {
        // SAFETY: alsa-lib passes the `io` it was given at `create`.
        let Some(p) = (unsafe { plugin(io) }) else {
            return -libc::EINVAL;
        };
        let Some(mut g) = p.lock() else {
            return -libc::ENODEV;
        };
        // SAFETY: as above.
        let appl = unsafe { appl_ptr(io) };
        // The engine tick is measured from the wake ticks seen so far: count the
        // ones that piled up, then tell the stream (capture lag follows it).
        g.link.drain_wake();
        let tick = g.link.tick_frames(g.params.sample_rate_hz);
        let Some(stream) = g.stream.as_mut() else {
            return -libc::EBADFD;
        };
        if tick > 0 {
            stream.set_tick(tick);
        }
        match stream.start(appl) {
            Ok(()) => {
                rewake(p, &mut g, appl, SND_PCM_STATE_RUNNING);
                0
            }
            Err(e) => {
                // SAFETY: as above.
                let r = unsafe { fail(p, io, &mut g, e) };
                arm(p, &mut g, Wake::Now);
                r
            }
        }
    })
}

unsafe extern "C" fn cb_stop(io: *mut snd_pcm_ioplug_t) -> c_int {
    guard(-libc::EIO, || {
        // SAFETY: alsa-lib passes the `io` it was given at `create`.
        let Some(p) = (unsafe { plugin(io) }) else {
            return -libc::EINVAL;
        };
        if let Some(mut g) = p.lock() {
            if let Some(s) = g.stream.as_mut() {
                s.stop();
            }
            // The PCM is in SETUP now: an application still polling must look.
            arm(p, &mut g, Wake::Now);
        }
        0
    })
}

unsafe extern "C" fn cb_pointer(io: *mut snd_pcm_ioplug_t) -> snd_pcm_sframes_t {
    guard(-(libc::EIO as c_long), || {
        // SAFETY: alsa-lib passes the `io` it was given at `create`.
        let Some(p) = (unsafe { plugin(io) }) else {
            return -(libc::EINVAL as c_long);
        };
        // SAFETY: as above.
        let (appl, state, buffer) = unsafe { (appl_ptr(io), io_state(io), ul((*io).buffer_size)) };
        let modulus = if p.wa {
            p.boundary.load(Ordering::Relaxed)
        } else {
            buffer
        }
        .max(1);
        let Some(mut g) = p.lock() else {
            // A poisoned mutex: a panic left the plugin unusable. A pointer of 0
            // would make `BOUNDARY_WA` arithmetic jump, so keep reporting the
            // last position and disconnect the PCM.
            // SAFETY: as above.
            unsafe { snd_pcm_ioplug_set_state(io, SND_PCM_STATE_DISCONNECTED) };
            return (p.last_pos.load(Ordering::Relaxed) % modulus) as c_long;
        };
        // An application that never calls `poll_revents` must still notice a
        // daemon that is gone: look now and then.
        if (g.drain_clock)().saturating_sub(g.last_drain_ns) >= DRAIN_EVERY_NS {
            check_daemon(p, &mut g, false);
        }
        let lost = g.link.is_lost();
        let Some(stream) = g.stream.as_mut() else {
            return 0;
        };
        let r = if lost {
            Pointer::Lost
        } else {
            stream.pointer(appl, state == SND_PCM_STATE_DRAINING)
        };
        match r {
            Pointer::Pos(h) => {
                g.last_pos = h;
                p.last_pos.store(h, Ordering::Relaxed);
                // ioplug enters DRAINING without a callback; its drain loop
                // (and a non-blocking drain's application) polls for the end
                // of the drain, which this is the first callback to see.
                if state == SND_PCM_STATE_DRAINING {
                    rewake(p, &mut g, appl, state);
                }
                (h % modulus) as c_long
            }
            Pointer::Xrun => {
                arm(p, &mut g, Wake::Now);
                -(libc::EPIPE as c_long)
            }
            Pointer::Lost => {
                let last = g.last_pos;
                // ioplug would turn a negative value into XRUN and the
                // application would loop re-preparing; flag the disconnect
                // and keep the pointer valid. `prepare` answers -ENODEV.
                // SAFETY: as above.
                unsafe { disconnect(p, io, &mut g) };
                arm(p, &mut g, Wake::Now);
                (last % modulus) as c_long
            }
        }
    })
}

unsafe extern "C" fn cb_transfer(
    io: *mut snd_pcm_ioplug_t,
    areas: *const snd_pcm_channel_area_t,
    offset: snd_pcm_uframes_t,
    size: snd_pcm_uframes_t,
) -> snd_pcm_sframes_t {
    guard(-(libc::EIO as c_long), || {
        // SAFETY: alsa-lib passes the `io` it was given at `create`.
        let Some(p) = (unsafe { plugin(io) }) else {
            return -(libc::EINVAL as c_long);
        };
        let Some(mut g) = p.lock() else {
            return -(libc::ENODEV as c_long);
        };
        // SAFETY: as above.
        let (appl, channels, format) =
            unsafe { (appl_ptr(io), (*io).channels as usize, (*io).format) };
        let Some(width) = PcmFormat::from_alsa(format).map(PcmFormat::bytes) else {
            return -(libc::EINVAL as c_long);
        };
        if areas.is_null() || channels == 0 {
            return -(libc::EINVAL as c_long);
        }
        let (offset, size) = (offset as usize, size as usize);
        if size == 0 {
            return 0;
        }
        let Inner {
            stream,
            areas: scratch,
            ..
        } = &mut *g;
        let Some(stream) = stream.as_mut() else {
            return -(libc::EBADFD as c_long);
        };
        let scratch = &mut scratch.0;
        scratch.clear();
        // SAFETY: alsa-lib passes `channels` valid channel areas.
        let raw = unsafe { core::slice::from_raw_parts(areas, channels) };
        for a in raw {
            if a.first % 8 != 0 || a.step % 8 != 0 || a.addr.is_null() {
                scratch.clear();
                return -(libc::EINVAL as c_long);
            }
            let (first, step) = (a.first as usize / 8, a.step as usize / 8);
            // The last byte any access touches: the last frame's sample.
            let Some(len) = step
                .checked_mul(offset.saturating_add(size - 1))
                .and_then(|n| n.checked_add(first))
                .and_then(|n| n.checked_add(width))
            else {
                scratch.clear();
                return -(libc::EINVAL as c_long);
            };
            // SAFETY: alsa-lib guarantees the area describes a buffer that
            // holds `offset + size` frames of this channel (`len` bytes, up to
            // and including the last sample, cover them), and that nothing else touches it during the
            // callback (it runs inside the PCM lock).
            scratch.push(unsafe { Area::from_raw(a.addr.cast::<u8>(), len, first, step) });
        }
        let r = stream.transfer(appl, scratch, offset, size);
        scratch.clear();
        match r {
            Ok(n) => {
                // With read/write access (the only access the plugin offers;
                // `plug` emulates mmap on top of it) ioplug advances the
                // application pointer by `n` once this returns, so the
                // descriptor must already reflect that. ioplug's own mmap
                // capture path calls `transfer` from `mmap_begin`, before the
                // application commits: nothing is consumed yet there.
                // SAFETY: as above.
                let (state, access) = unsafe { (io_state(io), (*io).access) };
                let consumed = match access {
                    SND_PCM_ACCESS_RW_INTERLEAVED | SND_PCM_ACCESS_RW_NONINTERLEAVED => n as u64,
                    _ => 0,
                };
                // ioplug keeps the pointer modulo the boundary.
                let boundary = p.boundary.load(Ordering::Relaxed).max(1);
                let next = (appl % boundary + consumed) % boundary;
                rewake(p, &mut g, next, state);
                n as c_long
            }
            Err(e) => {
                // SAFETY: as above.
                let r = -(unsafe { fail(p, io, &mut g, e) }.unsigned_abs() as c_long);
                arm(p, &mut g, Wake::Now);
                r
            }
        }
    })
}

/// `pause`: silences the output and freezes the pointer; resuming re-anchors to
/// the media clock (see [`PcmStream::pause`]). alsa-lib advertises pause
/// support as soon as this callback exists, and its ioplug flips the PCM to
/// `PAUSED` even without one, so it is implemented rather than refused.
unsafe extern "C" fn cb_pause(io: *mut snd_pcm_ioplug_t, enable: c_int) -> c_int {
    guard(-libc::EIO, || {
        // SAFETY: alsa-lib passes the `io` it was given at `create`.
        let Some(p) = (unsafe { plugin(io) }) else {
            return -libc::EINVAL;
        };
        let Some(mut g) = p.lock() else {
            return -libc::ENODEV;
        };
        // SAFETY: as above.
        let appl = unsafe { appl_ptr(io) };
        let Some(stream) = g.stream.as_mut() else {
            return -libc::EBADFD;
        };
        match stream.pause(enable != 0, appl) {
            Ok(()) => {
                let state = if enable != 0 {
                    SND_PCM_STATE_PAUSED
                } else {
                    SND_PCM_STATE_RUNNING
                };
                rewake(p, &mut g, appl, state);
                0
            }
            // SAFETY: as above.
            Err(e) => unsafe { fail(p, io, &mut g, e) },
        }
    })
}

/// `snd_pcm_delay`: the frames between the application and the wire.
unsafe extern "C" fn cb_delay(io: *mut snd_pcm_ioplug_t, delayp: *mut snd_pcm_sframes_t) -> c_int {
    guard(-libc::EIO, || {
        if delayp.is_null() {
            return -libc::EINVAL;
        }
        // SAFETY: alsa-lib passes the `io` it was given at `create`.
        let Some(p) = (unsafe { plugin(io) }) else {
            return -libc::EINVAL;
        };
        let Some(mut g) = p.lock() else {
            return -libc::ENODEV;
        };
        // SAFETY: as above.
        let (appl, state) = unsafe { (appl_ptr(io), io_state(io)) };
        let Some(stream) = g.stream.as_mut() else {
            return -libc::EBADFD;
        };
        match stream.delay(appl, state == SND_PCM_STATE_DRAINING) {
            Ok(d) => {
                // SAFETY: `delayp` is valid for one write.
                unsafe { *delayp = d as snd_pcm_sframes_t };
                0
            }
            // SAFETY: as above.
            Err(e) => unsafe { fail(p, io, &mut g, e) },
        }
    })
}

unsafe extern "C" fn cb_close(io: *mut snd_pcm_ioplug_t) -> c_int {
    guard(0, || {
        if io.is_null() {
            return 0;
        }
        // SAFETY: `private_data` is the pointer produced by `Box::into_raw`
        // in `open`; alsa-lib calls `close` exactly once and never touches
        // the plugin afterwards.
        let p = unsafe { (*io).private_data } as *mut Plugin;
        if !p.is_null() {
            // SAFETY: see above.
            unsafe { (*io).private_data = ptr::null_mut() };
            // SAFETY: see above; this is the matching `Box::from_raw`.
            drop(unsafe { Box::from_raw(p) });
        }
        0
    })
}

unsafe extern "C" fn cb_hw_params(
    io: *mut snd_pcm_ioplug_t,
    _params: *mut snd_pcm_hw_params_t,
) -> c_int {
    guard(-libc::EIO, || {
        // SAFETY: alsa-lib passes the `io` it was given at `create`.
        let Some(p) = (unsafe { plugin(io) }) else {
            return -libc::EINVAL;
        };
        let Some(mut g) = p.lock() else {
            return -libc::ENODEV;
        };
        // SAFETY: as above.
        let (format, channels, rate, period, buffer) = unsafe {
            (
                (*io).format,
                (*io).channels,
                (*io).rate,
                ul((*io).period_size),
                ul((*io).buffer_size),
            )
        };
        let Some(format) = PcmFormat::from_alsa(format) else {
            snderr("unsupported sample format");
            return -libc::EINVAL;
        };
        if channels != p.settings.channels
            || rate != g.params.sample_rate_hz
            || period == 0
            || period > buffer
            || buffer > p.max_frames
        {
            snderr("hardware parameters outside the granted limits");
            return -libc::EINVAL;
        }
        // A new stream replaces the old one: its ring role must be free.
        g.stream = None;
        let cfg = StreamConfig {
            dir: p.dir,
            format,
            channels: channels as usize,
            buffer_size: buffer,
            period_size: period,
            margin: u64::from(p.settings.latency),
            stale_ns: u64::from(p.settings.overlay_stale_ms) * 1_000_000,
            alias_guard: !p.wa,
            tick_frames: tick_for(&g),
        };
        match build_stream(&g, cfg) {
            Ok(mut s) => {
                s.set_sw(p.boundary.load(Ordering::Relaxed).max(1), buffer);
                g.areas = AreaCache(Vec::with_capacity(channels as usize));
                g.stream = Some(s);
                // SETUP until `prepare` (which alsa-lib runs next) re-arms.
                arm(p, &mut g, Wake::Never);
                0
            }
            Err(e) => {
                snderr(&e.to_string());
                -e.errno()
            }
        }
    })
}

/// The tick the stream assumes: the measured one, else the conservative default.
fn tick_for(g: &Inner) -> u64 {
    let m = g.link.tick_frames(g.params.sample_rate_hz);
    if m > 0 { m } else { g.default_tick }
}

fn build_stream(g: &Inner, cfg: StreamConfig) -> Result<Box<dyn PcmStream + Send>, Error> {
    let clock = OverlayClock::new(Arc::clone(&g.overlay), g.params.sample_rate_hz);
    let role = match cfg.dir {
        Dir::Playback => Role::Tx(g.ring.producer()?),
        Dir::Capture => Role::Rx(g.ring.consumer()?),
    };
    Ok(match g.params.format {
        SampleFormat::I32 => Box::new(Stream::<_, i32>::new(cfg, role, clock)?),
        SampleFormat::F32 => Box::new(Stream::<_, f32>::new(cfg, role, clock)?),
    })
}

unsafe extern "C" fn cb_hw_free(io: *mut snd_pcm_ioplug_t) -> c_int {
    guard(-libc::EIO, || {
        // SAFETY: alsa-lib passes the `io` it was given at `create`.
        if let Some(p) = unsafe { plugin(io) } {
            if let Some(mut g) = p.lock() {
                report_stats(&g);
                g.stream = None;
                arm(p, &mut g, Wake::Now);
            }
        }
        0
    })
}

unsafe extern "C" fn cb_sw_params(
    io: *mut snd_pcm_ioplug_t,
    params: *mut snd_pcm_sw_params_t,
) -> c_int {
    guard(-libc::EIO, || {
        // SAFETY: alsa-lib passes the `io` it was given at `create`.
        let Some(p) = (unsafe { plugin(io) }) else {
            return -libc::EINVAL;
        };
        let (mut boundary, mut avail_min, mut stop): (c_ulong, c_ulong, c_ulong) = (0, 0, 0);
        // SAFETY: `params` is the sw_params alsa-lib passes; the out pointers
        // are valid locals.
        let ok = unsafe {
            snd_pcm_sw_params_get_boundary(params, &raw mut boundary) >= 0
                && snd_pcm_sw_params_get_avail_min(params, &raw mut avail_min) >= 0
                && snd_pcm_sw_params_get_stop_threshold(params, &raw mut stop) >= 0
        };
        if !ok {
            return -libc::EINVAL;
        }
        p.boundary.store(ul(boundary), Ordering::Relaxed);
        p.avail_min.store(ul(avail_min), Ordering::Relaxed);
        if let Some(mut g) = p.lock() {
            if let Some(s) = g.stream.as_mut() {
                s.set_sw(ul(boundary), ul(stop));
            }
            // A new avail_min applies at once, whatever the state.
            // SAFETY: as above.
            let (appl, state) = unsafe { (appl_ptr(io), io_state(io)) };
            rewake(p, &mut g, appl, state);
        }
        0
    })
}

unsafe extern "C" fn cb_prepare(io: *mut snd_pcm_ioplug_t) -> c_int {
    guard(-libc::EIO, || {
        // SAFETY: alsa-lib passes the `io` it was given at `create`.
        let Some(p) = (unsafe { plugin(io) }) else {
            return -libc::EINVAL;
        };
        let Some(mut g) = p.lock() else {
            return -libc::ENODEV;
        };
        // Drain first: ticks that piled up while the application did not poll
        // count. No tick at all for `overlay_stale` means a broken daemon.
        let stale = Duration::from_millis(u64::from(p.settings.overlay_stale_ms));
        let alive = g.link.drain_wake() && g.link.check_sock();
        if !alive || (!stale.is_zero() && g.link.tick_age() > stale) {
            // SAFETY: as above.
            let r = unsafe { disconnect(p, io, &mut g) };
            arm(p, &mut g, Wake::Now);
            return r;
        }
        let Some(stream) = g.stream.as_mut() else {
            return -libc::EBADFD;
        };
        match stream.prepare() {
            Ok(()) => {
                // SAFETY: as above.
                let appl = unsafe { appl_ptr(io) };
                rewake(p, &mut g, appl, SND_PCM_STATE_PREPARED);
                0
            }
            // SAFETY: as above.
            Err(e) => unsafe { fail(p, io, &mut g, e) },
        }
    })
}

unsafe extern "C" fn cb_poll_descriptors_count(_io: *mut snd_pcm_ioplug_t) -> c_int {
    1
}

unsafe extern "C" fn cb_poll_descriptors(
    io: *mut snd_pcm_ioplug_t,
    pfd: *mut libc::pollfd,
    space: c_uint,
) -> c_int {
    guard(-libc::EIO, || {
        // SAFETY: alsa-lib passes the `io` it was given at `create`.
        let Some(p) = (unsafe { plugin(io) }) else {
            return -libc::EINVAL;
        };
        if space < 1 || pfd.is_null() {
            return 0;
        }
        // SAFETY: `pfd` has room for `space >= 1` entries.
        unsafe {
            *pfd = libc::pollfd {
                fd: p.poll_fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
        }
        1
    })
}

unsafe extern "C" fn cb_poll_revents(
    io: *mut snd_pcm_ioplug_t,
    pfd: *mut libc::pollfd,
    nfds: c_uint,
    revents: *mut u16,
) -> c_int {
    guard(-libc::EIO, || {
        if revents.is_null() {
            return -libc::EINVAL;
        }
        // SAFETY: `revents` is valid for one write.
        unsafe { *revents = 0 };
        // SAFETY: alsa-lib passes the `io` it was given at `create`.
        let Some(p) = (unsafe { plugin(io) }) else {
            return -libc::EINVAL;
        };
        if pfd.is_null() || nfds < 1 {
            return 0;
        }
        // SAFETY: `pfd` points to `nfds >= 1` entries.
        let got = unsafe { (*pfd).revents };
        let Some(mut g) = p.lock() else {
            // SAFETY: `revents` is valid for one write.
            unsafe { *revents = libc::POLLERR as u16 };
            return 0;
        };
        // The descriptor is our epoll set: readable when the timer expired or
        // the wake socket hung up, which only the drain can tell apart. An
        // error on the set itself means it is unusable.
        if got & (libc::POLLERR | libc::POLLNVAL) != 0 {
            g.link.mark_lost();
        }
        check_daemon(p, &mut g, true);
        g.sock_ticks = g.sock_ticks.wrapping_add(1);
        if g.sock_ticks % 128 == 0 {
            g.link.peek_alive();
        }
        // SAFETY: as above.
        let (appl, state) = unsafe { (appl_ptr(io), io_state(io)) };
        let avail_min = p.avail_min.load(Ordering::Relaxed).max(1);
        let mut lost = g.link.is_lost();
        let mut ready = false;
        if !lost {
            if let Some(stream) = g.stream.as_mut() {
                let a = stream.avail(appl);
                lost = a.lost;
                // A pending xrun is reported as readiness: the application's
                // next avail_update/pointer call learns about it. Otherwise
                // ready exactly when the descriptor is meant to be readable.
                ready = a.xrun
                    || stream.wake(appl, avail_min, state == SND_PCM_STATE_DRAINING) == Wake::Now;
            }
        }
        let out = if lost || state == SND_PCM_STATE_DISCONNECTED {
            if lost {
                // SAFETY: as above.
                unsafe { disconnect(p, io, &mut g) };
            }
            libc::POLLERR
        } else if !matches!(
            state,
            SND_PCM_STATE_PREPARED
                | SND_PCM_STATE_RUNNING
                | SND_PCM_STATE_PAUSED
                | SND_PCM_STATE_DRAINING
        ) {
            // Xrun, setup (stopped), open, suspended: the descriptor is
            // readable (see `rewake`), and like a kernel driver the answer is
            // an error the application can act on, not a silent spin.
            libc::POLLERR
        } else if ready {
            match p.dir {
                Dir::Playback => libc::POLLOUT,
                Dir::Capture => libc::POLLIN,
            }
        } else {
            0
        };
        // Not ready (an early or stale expiry): re-arm, which also makes the
        // descriptor unreadable again. The state may have changed just now.
        // SAFETY: as above.
        let state = unsafe { io_state(io) };
        rewake(p, &mut g, appl, state);
        // SAFETY: `revents` is valid for one write.
        unsafe { *revents = out as u16 };
        0
    })
}

fn default_client() -> String {
    let comm = std::fs::read_to_string("/proc/self/comm").unwrap_or_default();
    let comm: String = comm.trim().chars().filter(|c| !c.is_control()).collect();
    if comm.is_empty() {
        "alsa".to_string()
    } else {
        format!("alsa:{comm}")
    }
}

/// Reads the children of an alsa-lib configuration node.
///
/// # Safety
/// `conf` must be a valid `snd_config_t` compound node.
unsafe fn read_children(
    conf: *mut snd_config_t,
    depth: u32,
) -> Result<Vec<(String, Node)>, String> {
    if depth > 3 {
        return Err("configuration nested too deeply".into());
    }
    let mut out = Vec::new();
    // SAFETY: `conf` is a valid compound node (caller contract); the
    // iterator functions only walk its child list, and we fetch the next
    // iterator before using the entry, as alsa-lib's own macro does.
    unsafe {
        let mut it = snd_config_iterator_first(conf);
        let end = snd_config_iterator_end(conf);
        while it != end {
            let node = snd_config_iterator_entry(it);
            let next = snd_config_iterator_next(it);
            let mut id: *const c_char = ptr::null();
            if snd_config_get_id(node, &raw mut id) < 0 || id.is_null() {
                return Err("unreadable configuration key".into());
            }
            let id = CStr::from_ptr(id).to_string_lossy().into_owned();
            if snd_config_get_type(node) == SND_CONFIG_TYPE_COMPOUND {
                out.push((id, Node::Compound(read_children(node, depth + 1)?)));
            } else {
                let mut text: *mut c_char = ptr::null_mut();
                if snd_config_get_ascii(node, &raw mut text) < 0 || text.is_null() {
                    return Err(format!("`{id}` has no usable value"));
                }
                let value = CStr::from_ptr(text).to_string_lossy().into_owned();
                libc::free(text.cast());
                out.push((id, Node::Leaf(value)));
            }
            it = next;
        }
    }
    Ok(out)
}

/// The oldest alsa-lib with `SND_PCM_IOPLUG_FLAG_BOUNDARY_WA` is 1.2.1.
fn lib_has_boundary_wa() -> bool {
    // SAFETY: returns a static NUL-terminated version string.
    let v = unsafe { snd_asoundlib_version() };
    if v.is_null() {
        return false;
    }
    // SAFETY: non-null static C string.
    let s = unsafe { CStr::from_ptr(v) }.to_string_lossy();
    let mut it = s.split('.').map(|p| {
        p.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u32>()
            .unwrap_or(0)
    });
    let (a, b, c) = (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    );
    (a, b, c) >= (1, 2, 1)
}

fn open_impl(
    pcmp: *mut *mut snd_pcm_t,
    name: *const c_char,
    conf: *mut snd_config_t,
    stream: c_int,
    mode: c_int,
) -> c_int {
    if pcmp.is_null() || conf.is_null() {
        return -libc::EINVAL;
    }
    let dir = match stream {
        SND_PCM_STREAM_PLAYBACK => Dir::Playback,
        SND_PCM_STREAM_CAPTURE => Dir::Capture,
        _ => return -libc::EINVAL,
    };
    // SAFETY: `conf` is the configuration node alsa-lib hands to `open`.
    let tree = match unsafe { read_children(conf, 0) } {
        Ok(t) => t,
        Err(e) => {
            snderr(&e);
            return -libc::EINVAL;
        }
    };
    let env = env_socket(at_secure(), std::env::var(ENV_SOCKET).ok());
    let settings = match from_tree(&tree, dir, env.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            snderr(&e.to_string());
            return -libc::EINVAL;
        }
    };
    let client = settings.client.clone().unwrap_or_else(default_client);
    let mut attached = match connect_session(&settings, dir, &client) {
        Ok(a) => a,
        Err(e) => {
            snderr(&format!("{}: {e}", settings.socket.display()));
            return -e.errno();
        }
    };
    // Learn the daemon's tick (a few milliseconds at most) before the
    // constraints are set.
    attached.link.measure_tick(Duration::from_millis(50));
    let rate0 = attached.params.sample_rate_hz;
    let tick = match attached.link.tick_frames(rate0) {
        0 => default_tick(rate0),
        m => m,
    };
    let plan = match Plan::new(&attached, &settings, tick) {
        Ok(p) => p,
        Err(e) => {
            snderr(&e);
            return -libc::EINVAL;
        }
    };
    let wa = lib_has_boundary_wa();
    let rate = attached.params.sample_rate_hz;
    let raw = match new_plugin(attached, settings, dir, wa, &plan) {
        Ok(r) => r,
        Err(e) => {
            snderr(&format!("poll descriptor: {e}"));
            return -e.raw_os_error().unwrap_or(libc::EIO);
        }
    };
    // SAFETY: `raw` is the fresh plugin, alive until `cb_close`.
    let poll_fd = unsafe { (*raw).poll_fd.as_raw_fd() };
    let (max_buffer_bytes, min_period_bytes, min_buffer_bytes) = (
        plan.max_buffer_bytes,
        plan.min_period_bytes,
        plan.min_buffer_bytes,
    );
    // SAFETY: `raw` is a fresh, uniquely owned `Plugin`; it stays alive until
    // `cb_close` (called by alsa-lib on close or by `snd_pcm_ioplug_delete`).
    let io = unsafe { (*raw).io.get() };
    // SAFETY: as above; the name pointer lives as long as the plugin.
    unsafe {
        (*io).version = SND_PCM_IOPLUG_VERSION;
        (*io).name = (*raw).name.as_ptr();
        (*io).flags = if wa {
            SND_PCM_IOPLUG_FLAG_BOUNDARY_WA
        } else {
            0
        };
        (*io).poll_fd = poll_fd;
        (*io).poll_events = libc::POLLIN as c_uint;
        (*io).mmap_rw = 0;
        (*io).callback = &raw const CALLBACKS;
        (*io).private_data = raw.cast();
    }
    let _ = name;
    // SAFETY: `io` is initialised as the header requires.
    let err = unsafe { snd_pcm_ioplug_create(io, c"tessera".as_ptr(), stream, mode) };
    if err < 0 {
        // SAFETY: `create` failed before taking ownership; free our box.
        drop(unsafe { Box::from_raw(raw) });
        return err;
    }
    let access = [
        SND_PCM_ACCESS_RW_INTERLEAVED,
        SND_PCM_ACCESS_RW_NONINTERLEAVED,
    ];
    let formats = PcmFormat::ALL.map(PcmFormat::to_alsa);
    // SAFETY: `io` was created above; the lists outlive the calls (alsa-lib
    // copies them).
    let err = unsafe {
        let mut e = snd_pcm_ioplug_set_param_list(
            io,
            SND_PCM_IOPLUG_HW_ACCESS,
            access.len() as c_uint,
            access.as_ptr(),
        );
        if e >= 0 {
            e = snd_pcm_ioplug_set_param_list(
                io,
                SND_PCM_IOPLUG_HW_FORMAT,
                formats.len() as c_uint,
                formats.as_ptr(),
            );
        }
        for (kind, min, max) in [
            (
                SND_PCM_IOPLUG_HW_CHANNELS,
                settings_channels(raw),
                settings_channels(raw),
            ),
            (SND_PCM_IOPLUG_HW_RATE, rate, rate),
            (SND_PCM_IOPLUG_HW_PERIODS, 2, 1 << 20),
            (
                SND_PCM_IOPLUG_HW_PERIOD_BYTES,
                min_period_bytes,
                max_buffer_bytes / 2,
            ),
            (
                SND_PCM_IOPLUG_HW_BUFFER_BYTES,
                min_buffer_bytes,
                max_buffer_bytes,
            ),
        ] {
            if e >= 0 {
                e = snd_pcm_ioplug_set_param_minmax(io, kind, min, max);
            }
        }
        e
    };
    if err < 0 {
        // `delete` closes the PCM, which runs `cb_close` and frees the plugin.
        // SAFETY: `io` was created above.
        unsafe { snd_pcm_ioplug_delete(io) };
        return err;
    }
    // SAFETY: `pcmp` is valid for one write; `io.pcm` was set by `create`.
    unsafe { *pcmp = (*io).pcm };
    0
}

/// The tick assumed until the daemon's wake ticks have been measured: 5 ms.
/// Conservative on purpose (the daemon's default is 1 ms); a smaller guess
/// would read audio the engine has not delivered yet.
fn default_tick(rate: u32) -> u64 {
    u64::from(rate / 200).max(16)
}

/// Whether the kernel marked this process secure (`AT_SECURE`: setuid, setgid
/// or file capabilities): the environment is then not trusted.
fn at_secure() -> bool {
    // SAFETY: `getauxval` only reads the process auxiliary vector.
    unsafe { libc::getauxval(libc::AT_SECURE) != 0 }
}

/// What the daemon's ring allows the PCM to offer.
#[derive(Debug, Clone, Copy)]
struct Plan {
    /// Largest buffer in frames.
    max_frames: u64,
    /// `BUFFER_BYTES` maximum (assuming the smallest, 2-byte, samples).
    max_buffer_bytes: u32,
    /// `PERIOD_BYTES` minimum (4-byte samples: the tick in frames).
    min_period_bytes: u32,
    /// `BUFFER_BYTES` minimum (two periods plus the margin).
    min_buffer_bytes: u32,
    /// The tick the stream assumes until the wake ticks are measured.
    tick_frames: u64,
}

impl Plan {
    // Largest buffer the ring can carry: playback writes `period + latency`
    // ahead of the clock plus up to a buffer; capture reads `effective latency
    // + tick + latency` behind it. Both need `1.5 * buffer + slack <=
    // capacity` (periods are at most half the buffer).
    /// `tick`: the daemon's engine tick in frames, measured or the default. The
    /// application is woken once per tick, so a period shorter than the tick
    /// cannot be fed in time and a buffer must hold the start lead plus two
    /// periods: smaller requests are rounded up by the constraints, not left to
    /// underrun continuously.
    fn new(
        attached: &crate::client::Attached,
        settings: &Settings,
        tick: u64,
    ) -> Result<Plan, String> {
        let cap = attached.params.capacity_frames;
        let room = cap
            .saturating_sub(attached.latency.min(cap))
            .saturating_sub(u64::from(settings.latency) + 128);
        let max_frames = (room / 3 * 2).min(u64::from(settings.max_buffer));
        let channels = u64::from(settings.channels);
        let bytes =
            |frames: u64| u32::try_from(frames.saturating_mul(channels).saturating_mul(2)).ok();
        // The bounds are in bytes and the smallest sample is 2 bytes: the upper
        // bound assumes 2 (so 16-bit data reaches `max_frames`), the lower
        // bound 4 (so 32-bit data is not allowed below the minimum either).
        let bytes4 =
            |frames: u64| u32::try_from(frames.saturating_mul(channels).saturating_mul(4)).ok();
        let min_period = tick.max(16);
        let min_buffer = 2 * min_period + u64::from(settings.latency);
        let (Some(max_buffer_bytes), Some(min_period_bytes), Some(min_buffer_bytes)) =
            (bytes(max_frames), bytes4(min_period), bytes4(min_buffer))
        else {
            return Err("channels x buffer size exceed 4 GiB".into());
        };
        if max_frames < 64 || max_frames < 2 * min_buffer {
            return Err(format!(
                "the daemon's ring is too small for this configuration (buffers of {min_buffer} to {max_frames} frames needed)"
            ));
        }
        Ok(Plan {
            max_frames,
            max_buffer_bytes,
            min_period_bytes,
            min_buffer_bytes,
            tick_frames: tick,
        })
    }
}

/// Allocates the plugin for an attached session and fills the ioplug header
/// (everything but what `snd_pcm_ioplug_create` fills in).
fn new_plugin(
    attached: crate::client::Attached,
    settings: Settings,
    dir: Dir,
    wa: bool,
    plan: &Plan,
) -> std::io::Result<*mut Plugin> {
    let (poll_fd, timer_fd) = poll_set(attached.link.wake_fd())?;
    let plugin = Box::new(Plugin {
        // SAFETY: an all-zero `snd_pcm_ioplug_t` is a valid value (integers
        // and null pointers); `create` fills the rest.
        io: UnsafeCell::new(unsafe { core::mem::zeroed() }),
        name: CString::new("tessera").unwrap_or_default(),
        dir,
        settings,
        poll_fd,
        timer_fd,
        wa,
        max_frames: plan.max_frames,
        avail_min: AtomicU64::new(1),
        boundary: AtomicU64::new(1 << 62),
        last_pos: AtomicU64::new(0),
        inner: Mutex::new(Inner {
            link: attached.link,
            ring: attached.ring,
            overlay: attached.overlay,
            params: attached.params,
            stream: None,
            areas: AreaCache(Vec::new()),
            last_pos: 0,
            sock_ticks: 0,
            default_tick: plan.tick_frames,
            armed: None,
            last_drain_ns: 0,
            drain_clock: raw_monotonic_ns,
        }),
    });
    Ok(Box::into_raw(plugin))
}

fn settings_channels(p: *mut Plugin) -> c_uint {
    // SAFETY: `p` is the live plugin created in `open_impl`.
    unsafe { (*p).settings.channels }
}

/// alsa-lib entry point of `pcm.tessera` (`SND_PCM_PLUGIN_DEFINE_FUNC`).
///
/// # Safety
/// Called by alsa-lib with valid arguments.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _snd_pcm_tessera_open(
    pcmp: *mut *mut snd_pcm_t,
    name: *const c_char,
    _root: *mut snd_config_t,
    conf: *mut snd_config_t,
    stream: c_int,
    mode: c_int,
) -> c_int {
    guard(-libc::EIO, || open_impl(pcmp, name, conf, stream, mode))
}

/// alsa-lib looks this symbol up to verify the plugin ABI version
/// (`SND_PCM_PLUGIN_SYMBOL(tessera)`).
#[unsafe(no_mangle)]
pub static __snd_pcm_tessera_open_dlsym_pcm_001: u8 = 0;

/// Hooks for the tests: a plugin over an attached session without the PCM
/// object alsa-lib would wrap around it, so the real callbacks can be called
/// through [`CALLBACKS`] with a mock `snd_pcm_ioplug_t`.
#[doc(hidden)]
pub mod testing {
    use super::*;

    /// Builds the plugin for `attached` and returns its ioplug header, filled as
    /// `open` fills it (but not registered with alsa-lib). The caller sets the
    /// fields alsa-lib would (`format`, `channels`, `rate`, `period_size`,
    /// `buffer_size`, `appl_ptr`, `state`).
    ///
    /// # Errors
    /// The reason the ring does not fit the settings.
    pub fn mock_plugin(
        attached: crate::client::Attached,
        settings: Settings,
        dir: Dir,
    ) -> Result<*mut snd_pcm_ioplug_t, String> {
        let plan = Plan::new(
            &attached,
            &settings,
            default_tick(attached.params.sample_rate_hz),
        )?;
        let raw = new_plugin(attached, settings, dir, true, &plan).map_err(|e| e.to_string())?;
        // SAFETY: `raw` is a fresh, uniquely owned plugin.
        let io = unsafe { (*raw).io.get() };
        // SAFETY: as above.
        unsafe {
            (*io).version = SND_PCM_IOPLUG_VERSION;
            (*io).name = (*raw).name.as_ptr();
            (*io).flags = SND_PCM_IOPLUG_FLAG_BOUNDARY_WA;
            (*io).callback = &raw const CALLBACKS;
            (*io).private_data = raw.cast();
            (*io).stream = match dir {
                Dir::Playback => SND_PCM_STREAM_PLAYBACK,
                Dir::Capture => SND_PCM_STREAM_CAPTURE,
            };
        }
        Ok(io)
    }

    /// Use the test's monotonic timeline for wake interval measurements.
    ///
    /// # Safety
    /// `io` must come from [`mock_plugin`] and not have been freed. Set this
    /// before the first callback; `now` must be monotonic on this thread.
    pub unsafe fn set_clocks(
        io: *mut snd_pcm_ioplug_t,
        now: fn() -> std::time::Instant,
        raw: fn() -> u64,
    ) {
        // SAFETY: caller contract.
        if let Some(p) = unsafe { plugin(io) } {
            if let Some(mut g) = p.lock() {
                g.link.set_clock(now);
                g.drain_clock = raw;
                g.last_drain_ns = 0;
            }
        }
    }

    /// What `cb_sw_params` stores, without a `snd_pcm_sw_params_t`.
    ///
    /// # Safety
    /// `io` must come from [`mock_plugin`] and not have been freed.
    pub unsafe fn set_sw(io: *mut snd_pcm_ioplug_t, boundary: u64, avail_min: u64, stop: u64) {
        // SAFETY: caller contract.
        let Some(p) = (unsafe { plugin(io) }) else {
            return;
        };
        p.boundary.store(boundary, Ordering::Relaxed);
        p.avail_min.store(avail_min, Ordering::Relaxed);
        if let Some(mut g) = p.lock() {
            if let Some(s) = g.stream.as_mut() {
                s.set_sw(boundary, stop);
            }
            // SAFETY: caller contract.
            let (appl, state) = unsafe { (appl_ptr(io), io_state(io)) };
            rewake(p, &mut g, appl, state);
        }
    }

    /// The daemon's engine tick as the plugin has measured it, frames (0:
    /// not measured yet, the 5 ms default stands in).
    ///
    /// # Safety
    /// As for [`set_sw`].
    pub unsafe fn tick_frames(io: *mut snd_pcm_ioplug_t) -> u64 {
        // SAFETY: caller contract.
        let Some(p) = (unsafe { plugin(io) }) else {
            return 0;
        };
        p.lock()
            .map_or(0, |g| g.link.tick_frames(g.params.sample_rate_hz))
    }

    /// The PCM's poll descriptor (what `poll_descriptors` hands out).
    ///
    /// # Safety
    /// As for [`set_sw`].
    pub unsafe fn poll_fd(io: *mut snd_pcm_ioplug_t) -> c_int {
        // SAFETY: caller contract.
        unsafe { plugin(io) }.map_or(-1, |p| p.poll_fd.as_raw_fd())
    }

    /// Poisons the plugin's mutex the way a panic inside a callback would.
    ///
    /// # Safety
    /// As for [`set_sw`].
    pub unsafe fn poison(io: *mut snd_pcm_ioplug_t) {
        // SAFETY: caller contract.
        let Some(p) = (unsafe { plugin(io) }) else {
            return;
        };
        let _ = std::thread::scope(|s| {
            s.spawn(|| {
                let _guard = p.inner.lock();
                panic!("poisoning the plugin mutex on purpose");
            })
            .join()
        });
    }

    /// The state alsa-lib would hold for the PCM.
    ///
    /// # Safety
    /// As for [`set_sw`].
    pub unsafe fn state(io: *mut snd_pcm_ioplug_t) -> c_int {
        // SAFETY: caller contract.
        unsafe { io_state(io) }
    }

    /// Closes the mock (runs `cb_close`).
    ///
    /// # Safety
    /// As for [`set_sw`]; `io` must not be used afterwards.
    pub unsafe fn free(io: *mut snd_pcm_ioplug_t) {
        // SAFETY: caller contract.
        unsafe { cb_close(io) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn poll_descriptor_uses_a_supported_kernel_timer_clock() {
        let (signal, wait) = tessera_audio_ipc::wake_pair().unwrap();
        let (poll, timer) = poll_set(wait.as_fd().as_raw_fd()).unwrap();
        assert!(poll.as_raw_fd() >= 0 && timer.as_raw_fd() >= 0);
        drop(signal);
    }

    #[test]
    fn guard_turns_a_panic_into_the_fallback() {
        assert_eq!(guard(-5, || panic!("boom")), -5);
        assert_eq!(guard(-5, || 7), 7);
    }

    #[test]
    fn default_client_name_is_clean() {
        let c = default_client();
        assert!(c.starts_with("alsa"));
        assert!(!c.chars().any(char::is_control));
    }
}
