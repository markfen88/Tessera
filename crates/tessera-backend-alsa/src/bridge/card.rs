//! One direction of a hardware card as the bridge sees it.
//!
//! The bridge engine is written against [`CardPcm`], so it runs unchanged on a
//! real ALSA PCM ([`crate::bridge::AlsaCard`], for example the `snd-aloop`
//! Loopback card) and on a simulated card with a virtual clock in the tests.

use std::time::Duration;

use crate::convert::PcmFormat;

/// Why a card operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardError {
    /// Underrun (playback) or overrun (capture); [`CardPcm::recover`] restarts.
    Xrun,
    /// The system suspended; [`CardPcm::recover`] tries to resume.
    Suspended,
    /// The card is gone (hot-unplug); nothing can be done but to give it up.
    Disconnected,
    /// Anything else (a negative errno).
    Other(i32),
}

impl CardError {
    /// Classifies a positive errno.
    pub fn from_errno(e: i32) -> CardError {
        match e {
            libc::EPIPE => CardError::Xrun,
            libc::ESTRPIPE => CardError::Suspended,
            // What the kernel answers on a card that was removed (ENOTTY: the
            // ioctl of a driver that is gone).
            libc::ENODEV | libc::ENXIO | libc::ENOTTY => CardError::Disconnected,
            other => CardError::Other(other),
        }
    }
}

impl std::fmt::Display for CardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CardError::Xrun => write!(f, "xrun"),
            CardError::Suspended => write!(f, "card suspended"),
            CardError::Disconnected => write!(f, "card disconnected"),
            CardError::Other(e) => write!(f, "card error (errno {e})"),
        }
    }
}

impl std::error::Error for CardError {}

/// One direction (playback or capture) of an opened, configured card.
///
/// Everything here runs on the bridge's real-time thread: implementations
/// must not allocate or take locks; only the blocking `wait` and the card I/O
/// itself may enter the kernel.
pub trait CardPcm: Send {
    /// Sample rate in Hz.
    fn rate(&self) -> u32;
    /// Channels per frame.
    fn channels(&self) -> usize;
    /// Sample format of the interleaved byte buffers of `write`/`read`.
    fn format(&self) -> PcmFormat;
    /// Frames per hardware period.
    fn period_frames(&self) -> usize;
    /// Frames of the hardware buffer.
    fn buffer_frames(&self) -> usize;
    /// Sets how many frames must be available (free space for playback,
    /// captured data for capture) before [`CardPcm::wait`] reports the card
    /// ready (ALSA `avail_min`). Control thread, before the loop starts.
    ///
    /// # Errors
    /// A card failure.
    fn set_wake_avail(&mut self, frames: usize) -> Result<(), CardError>;
    /// Waits until `avail() >= wake_avail` or `timeout` passes (it may also
    /// return early; callers re-check `avail`).
    ///
    /// # Errors
    /// A card failure noticed while waiting.
    fn wait(&mut self, timeout: Duration) -> Result<(), CardError>;
    /// Frames that can be written (playback) or read (capture) now.
    ///
    /// # Errors
    /// [`CardError::Xrun`] after an under- or overrun, or a card failure.
    fn avail(&mut self) -> Result<usize, CardError>;
    /// Frames between the application and the converter: queued for playback
    /// not yet played, or captured and not yet read.
    ///
    /// # Errors
    /// As for [`CardPcm::avail`].
    fn delay(&mut self) -> Result<usize, CardError>;
    /// Writes whole frames (`bytes.len()` is a multiple of the frame size)
    /// and returns the frames accepted.
    ///
    /// # Errors
    /// As for [`CardPcm::avail`].
    fn write(&mut self, bytes: &[u8]) -> Result<usize, CardError>;
    /// Reads whole frames into `bytes` and returns the frames delivered.
    ///
    /// # Errors
    /// As for [`CardPcm::avail`].
    fn read(&mut self, bytes: &mut [u8]) -> Result<usize, CardError>;
    /// Starts the stream (playback: after the prefill).
    ///
    /// # Errors
    /// A card failure.
    fn start(&mut self) -> Result<(), CardError>;
    /// Recovers from an xrun or a suspend: leaves the stream prepared and
    /// empty (not started).
    ///
    /// # Errors
    /// The failure that could not be recovered (for example a disconnect).
    fn recover(&mut self) -> Result<(), CardError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_classification() {
        assert_eq!(CardError::from_errno(libc::EPIPE), CardError::Xrun);
        assert_eq!(CardError::from_errno(libc::ESTRPIPE), CardError::Suspended);
        for e in [libc::ENODEV, libc::ENXIO, libc::ENOTTY] {
            assert_eq!(CardError::from_errno(e), CardError::Disconnected);
        }
        // Errors that are not a removal must not end the loop as one.
        assert_eq!(
            CardError::from_errno(libc::EIO),
            CardError::Other(libc::EIO)
        );
        assert_eq!(
            CardError::from_errno(libc::EBADFD),
            CardError::Other(libc::EBADFD)
        );
    }
}
