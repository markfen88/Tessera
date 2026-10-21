//! A real ALSA PCM (`hw:...`, `plughw:...`, the `snd-aloop` Loopback card) as
//! a [`CardPcm`].

use std::time::Duration;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::poll::{Descriptors, Flags};
use alsa::{Direction, ValueOr};

use super::BridgeError;
use super::card::{CardError, CardPcm};
use crate::convert::PcmFormat;

/// How to open a card.
#[derive(Debug, Clone)]
pub struct CardParams {
    /// ALSA device name, for example `hw:Loopback,0,0` or `plughw:0`.
    pub name: String,
    /// Sample rate; the card must run exactly at it.
    pub rate: u32,
    /// Channels.
    pub channels: u32,
    /// Frames per period (the nearest the card offers is used).
    pub period_frames: usize,
    /// Periods in the buffer (at least 3 for playback: prefill plus one
    /// being filled plus one playing).
    pub periods: usize,
    /// Sample formats to try, in order.
    pub formats: Vec<PcmFormat>,
}

impl CardParams {
    /// `name` at `rate` with `channels` channels, 10 ms periods, 4 periods and
    /// the usual formats.
    pub fn new(name: &str, rate: u32, channels: u32) -> Self {
        Self {
            name: name.to_string(),
            rate,
            channels,
            period_frames: (rate as usize / 100).max(32),
            periods: 4,
            formats: vec![
                PcmFormat::S32,
                PcmFormat::Float,
                PcmFormat::S24,
                PcmFormat::S24Packed,
                PcmFormat::S16,
            ],
        }
    }
}

fn alsa_format(f: PcmFormat) -> Format {
    match f {
        PcmFormat::S16 => Format::s16(),
        PcmFormat::S24 => Format::s24(),
        PcmFormat::S24Packed => Format::s24_3(),
        PcmFormat::S32 => Format::s32(),
        PcmFormat::Float => Format::float(),
    }
}

const MAX_FDS: usize = 8;

/// One direction of an opened ALSA card.
pub struct AlsaCard {
    pcm: PCM,
    fds: [libc::pollfd; MAX_FDS],
    nfds: usize,
    fmt: PcmFormat,
    channels: usize,
    rate: u32,
    period: usize,
    buffer: usize,
    frame_bytes: usize,
    playback: bool,
}

impl std::fmt::Debug for AlsaCard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlsaCard")
            .field("rate", &self.rate)
            .field("channels", &self.channels)
            .field("period", &self.period)
            .field("buffer", &self.buffer)
            .field("format", &self.fmt)
            .finish_non_exhaustive()
    }
}

fn open_err(what: &str, e: alsa::Error) -> BridgeError {
    BridgeError::Open(format!("{what}: {e}"))
}

impl AlsaCard {
    /// Opens `p.name` for playback or capture, configures it (non-blocking,
    /// interleaved read/write access) and prepares it. Control thread.
    ///
    /// # Errors
    /// [`BridgeError::Open`] naming the step that failed, including a card
    /// that cannot run at exactly `p.rate`.
    pub fn open(p: &CardParams, playback: bool) -> Result<AlsaCard, BridgeError> {
        if p.rate == 0 || p.channels == 0 || p.period_frames == 0 || p.periods < 2 {
            return Err(BridgeError::Config(
                "rate, channels, period and periods must be positive".into(),
            ));
        }
        let dir = if playback {
            Direction::Playback
        } else {
            Direction::Capture
        };
        let pcm = PCM::new(&p.name, dir, true).map_err(|e| open_err("open", e))?;
        let (fmt, rate, period, buffer) = {
            let hwp = HwParams::any(&pcm).map_err(|e| open_err("hw_params_any", e))?;
            hwp.set_access(Access::RWInterleaved)
                .map_err(|e| open_err("access", e))?;
            hwp.set_channels(p.channels)
                .map_err(|e| open_err("channels", e))?;
            hwp.set_rate(p.rate, ValueOr::Nearest)
                .map_err(|e| open_err("rate", e))?;
            let rate = hwp.get_rate().map_err(|e| open_err("rate", e))?;
            if rate != p.rate {
                return Err(BridgeError::Open(format!(
                    "{} cannot run at {} Hz (nearest {rate} Hz); the bridge compensates drift, not rate differences",
                    p.name, p.rate
                )));
            }
            let fmt = p
                .formats
                .iter()
                .copied()
                .find(|f| hwp.test_format(alsa_format(*f)).is_ok())
                .ok_or_else(|| {
                    BridgeError::Open(format!(
                        "{} offers none of the supported sample formats",
                        p.name
                    ))
                })?;
            hwp.set_format(alsa_format(fmt))
                .map_err(|e| open_err("format", e))?;
            hwp.set_period_size_near(p.period_frames as alsa::pcm::Frames, ValueOr::Nearest)
                .map_err(|e| open_err("period size", e))?;
            hwp.set_buffer_size_near((p.period_frames * p.periods) as alsa::pcm::Frames)
                .map_err(|e| open_err("buffer size", e))?;
            pcm.hw_params(&hwp).map_err(|e| open_err("hw_params", e))?;
            (
                fmt,
                rate,
                hwp.get_period_size()
                    .map_err(|e| open_err("period size", e))? as usize,
                hwp.get_buffer_size()
                    .map_err(|e| open_err("buffer size", e))? as usize,
            )
        };
        {
            let swp = pcm
                .sw_params_current()
                .map_err(|e| open_err("sw_params", e))?;
            // The bridge starts playback itself after the prefill.
            swp.set_start_threshold(buffer as alsa::pcm::Frames * 2)
                .map_err(|e| open_err("start threshold", e))?;
            swp.set_avail_min(period as alsa::pcm::Frames)
                .map_err(|e| open_err("avail_min", e))?;
            pcm.sw_params(&swp).map_err(|e| open_err("sw_params", e))?;
        }
        pcm.prepare().map_err(|e| open_err("prepare", e))?;
        let mut fds = [libc::pollfd {
            fd: -1,
            events: 0,
            revents: 0,
        }; MAX_FDS];
        let n = Descriptors::count(&pcm);
        if n == 0 || n > MAX_FDS {
            return Err(BridgeError::Open(format!("{n} poll descriptors")));
        }
        let nfds =
            Descriptors::fill(&pcm, &mut fds[..n]).map_err(|e| open_err("poll descriptors", e))?;
        Ok(AlsaCard {
            pcm,
            fds,
            nfds,
            fmt,
            channels: p.channels as usize,
            rate,
            period,
            buffer,
            frame_bytes: p.channels as usize * fmt.bytes(),
            playback,
        })
    }

    fn classify(&self) -> CardError {
        // Not `PCM::state`, which panics on a value it does not know.
        match self.pcm.state_raw() {
            4 => CardError::Xrun,
            7 => CardError::Suspended,
            8 => CardError::Disconnected,
            _ => CardError::Other(libc::EIO),
        }
    }
}

fn map(e: alsa::Error) -> CardError {
    CardError::from_errno(e.errno().abs())
}

impl CardPcm for AlsaCard {
    fn rate(&self) -> u32 {
        self.rate
    }
    fn channels(&self) -> usize {
        self.channels
    }
    fn format(&self) -> PcmFormat {
        self.fmt
    }
    fn period_frames(&self) -> usize {
        self.period
    }
    fn buffer_frames(&self) -> usize {
        self.buffer
    }

    fn set_wake_avail(&mut self, frames: usize) -> Result<(), CardError> {
        let swp = self.pcm.sw_params_current().map_err(map)?;
        swp.set_avail_min(frames.clamp(1, self.buffer) as alsa::pcm::Frames)
            .map_err(map)?;
        self.pcm.sw_params(&swp).map_err(map)
    }

    fn wait(&mut self, timeout: Duration) -> Result<(), CardError> {
        for f in &mut self.fds[..self.nfds] {
            f.revents = 0;
        }
        // Round up so a sub-millisecond timeout does not spin.
        let ms = timeout.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
        match alsa::poll::poll(&mut self.fds[..self.nfds], ms) {
            Ok(0) => Ok(()),
            Ok(_) => {
                let flags = self.pcm.revents(&self.fds[..self.nfds]).map_err(map)?;
                if flags.intersects(Flags::ERR | Flags::HUP | Flags::NVAL) {
                    Err(self.classify())
                } else {
                    Ok(())
                }
            }
            Err(e) if e.errno().abs() == libc::EINTR => Ok(()),
            Err(e) => Err(map(e)),
        }
    }

    fn avail(&mut self) -> Result<usize, CardError> {
        self.pcm
            .avail_update()
            .map(|a| a.max(0) as usize)
            .map_err(map)
    }

    fn delay(&mut self) -> Result<usize, CardError> {
        self.pcm.delay().map(|d| d.max(0) as usize).map_err(map)
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, CardError> {
        debug_assert!(self.playback && bytes.len().checked_rem(self.frame_bytes) == Some(0));
        match self.pcm.io_bytes().writei(bytes) {
            Ok(n) => Ok(n),
            Err(e) if e.errno().abs() == libc::EAGAIN => Ok(0),
            Err(e) => Err(map(e)),
        }
    }

    fn read(&mut self, bytes: &mut [u8]) -> Result<usize, CardError> {
        debug_assert!(!self.playback && bytes.len().checked_rem(self.frame_bytes) == Some(0));
        match self.pcm.io_bytes().readi(bytes) {
            Ok(n) => Ok(n),
            Err(e) if e.errno().abs() == libc::EAGAIN => Ok(0),
            Err(e) => Err(map(e)),
        }
    }

    fn start(&mut self) -> Result<(), CardError> {
        match self.pcm.start() {
            Ok(()) => Ok(()),
            // Already running (the card started itself).
            Err(e) if e.errno().abs() == libc::EBADFD => Ok(()),
            Err(e) => Err(map(e)),
        }
    }

    fn recover(&mut self) -> Result<(), CardError> {
        if self.pcm.state_raw() == 7 {
            // Suspended: try to resume for a short while, then re-prepare.
            for _ in 0..20 {
                match self.pcm.resume() {
                    Ok(()) => break,
                    Err(e) if e.errno().abs() == libc::EAGAIN => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        }
        self.pcm.prepare().map_err(map)
    }
}
