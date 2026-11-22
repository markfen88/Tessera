//! A contiguous slice of a card's channels as a card of its own.
//!
//! Some cards open only with all their channels (a USB mixer's 16-channel
//! capture), while a bridge may carry just a few of them (its main mix on
//! channels 15 and 16). [`SlicedCard`] opens nothing itself: it wraps an
//! opened [`CardPcm`] with all of the card's channels and presents only the
//! slice to the bridge engine. Capture copies the slice out of each card
//! frame; playback writes the slice into each card frame and silence (zero
//! bytes, silence in every [`PcmFormat`]) everywhere else.
//!
//! Everything is sized when the wrapper is built (control thread): the
//! period loop only copies bytes within a preallocated buffer.

use std::time::Duration;

use super::BridgeError;
use super::card::{CardError, CardPcm};
use crate::convert::PcmFormat;

/// Where a slice sits in a card frame. All counts are channels except
/// `sample_bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardSlice {
    /// Channels in a card frame.
    pub card_channels: usize,
    /// First card channel of the slice, 0-based.
    pub first: usize,
    /// Channels in the slice.
    pub channels: usize,
    /// Bytes per sample.
    pub sample_bytes: usize,
}

impl CardSlice {
    /// The slice of `channels` card channels from `card_first_channel`
    /// (1-based) on, in a card of `card_channels` channels.
    ///
    /// # Errors
    /// [`BridgeError::Config`] for an empty slice, a channel 0, or a slice
    /// beyond the card's channels.
    pub fn new(
        card_channels: usize,
        card_first_channel: usize,
        channels: usize,
        format: PcmFormat,
    ) -> Result<Self, BridgeError> {
        let last = card_first_channel
            .saturating_add(channels)
            .saturating_sub(1);
        if card_first_channel == 0 || channels == 0 || last > card_channels {
            return Err(BridgeError::Config(format!(
                "card channels {card_first_channel}..={last} do not fit a card of {card_channels} channel(s)"
            )));
        }
        Ok(Self {
            card_channels,
            first: card_first_channel - 1,
            channels,
            sample_bytes: format.bytes(),
        })
    }

    /// Bytes of one card frame.
    #[must_use]
    pub fn card_frame_bytes(&self) -> usize {
        self.card_channels * self.sample_bytes
    }

    /// Bytes of one frame of the slice.
    #[must_use]
    pub fn frame_bytes(&self) -> usize {
        self.channels * self.sample_bytes
    }

    /// Is the slice the whole card (nothing to copy)?
    #[must_use]
    pub fn is_whole(&self) -> bool {
        self.first == 0 && self.channels == self.card_channels
    }

    /// Capture: copies the slice of `frames` interleaved card frames in
    /// `card` to `out` (interleaved frames of the slice). Real-time safe.
    pub fn gather(&self, card: &[u8], out: &mut [u8], frames: usize) {
        let (cf, sf) = (self.card_frame_bytes(), self.frame_bytes());
        let off = self.first * self.sample_bytes;
        for (src, dst) in card
            .chunks_exact(cf)
            .zip(out.chunks_exact_mut(sf))
            .take(frames)
        {
            dst.copy_from_slice(&src[off..off + sf]);
        }
    }

    /// Playback: copies `frames` interleaved frames of the slice in `input`
    /// into the slice of the card frames in `card`; the card's other
    /// channels are left as they are (silence, when `card` started zeroed
    /// and only this ever wrote it). Real-time safe.
    pub fn scatter(&self, input: &[u8], card: &mut [u8], frames: usize) {
        let (cf, sf) = (self.card_frame_bytes(), self.frame_bytes());
        let off = self.first * self.sample_bytes;
        for (src, dst) in input
            .chunks_exact(sf)
            .zip(card.chunks_exact_mut(cf))
            .take(frames)
        {
            dst[off..off + sf].copy_from_slice(src);
        }
    }
}

/// An opened card seen through a [`CardSlice`]: the bridge engine sees
/// `slice.channels` channels.
pub struct SlicedCard {
    inner: Box<dyn CardPcm>,
    slice: CardSlice,
    /// Whole card frames, a card buffer's worth; zeroed once, so the
    /// channels outside the slice stay silent on playback.
    scratch: Vec<u8>,
    /// Card frames `scratch` holds.
    scratch_frames: usize,
}

impl std::fmt::Debug for SlicedCard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlicedCard")
            .field("slice", &self.slice)
            .field("scratch_frames", &self.scratch_frames)
            .finish_non_exhaustive()
    }
}

impl SlicedCard {
    /// Wraps `inner` (opened with all the channels it needs) to carry
    /// `channels` card channels from `card_first_channel` (1-based) on.
    /// Control thread: allocates the scratch buffer.
    ///
    /// # Errors
    /// [`BridgeError::Config`] if the slice does not fit the card.
    pub fn new(
        inner: Box<dyn CardPcm>,
        card_first_channel: usize,
        channels: usize,
    ) -> Result<Self, BridgeError> {
        let slice = CardSlice::new(
            inner.channels(),
            card_first_channel,
            channels,
            inner.format(),
        )?;
        let scratch_frames = inner.buffer_frames().max(1);
        Ok(Self {
            scratch: vec![0; scratch_frames * slice.card_frame_bytes()],
            scratch_frames,
            inner,
            slice,
        })
    }

    /// `inner` as the bridge should see it: itself when the slice is the
    /// whole card, otherwise wrapped.
    ///
    /// # Errors
    /// As [`SlicedCard::new`].
    pub fn wrap(
        inner: Box<dyn CardPcm>,
        card_first_channel: usize,
        channels: usize,
    ) -> Result<Box<dyn CardPcm>, BridgeError> {
        let s = Self::new(inner, card_first_channel, channels)?;
        Ok(if s.slice.is_whole() {
            s.inner
        } else {
            Box::new(s)
        })
    }

    /// The slice carried.
    #[must_use]
    pub fn slice(&self) -> CardSlice {
        self.slice
    }

    /// Frames of the slice in `bytes`, limited to what the scratch buffer
    /// holds (a shorter transfer, which the bridge handles like any other).
    fn frames(&self, bytes: usize) -> usize {
        (bytes / self.slice.frame_bytes()).min(self.scratch_frames)
    }
}

impl CardPcm for SlicedCard {
    fn rate(&self) -> u32 {
        self.inner.rate()
    }
    fn channels(&self) -> usize {
        self.slice.channels
    }
    fn format(&self) -> PcmFormat {
        self.inner.format()
    }
    fn period_frames(&self) -> usize {
        self.inner.period_frames()
    }
    fn buffer_frames(&self) -> usize {
        self.inner.buffer_frames()
    }
    fn set_wake_avail(&mut self, frames: usize) -> Result<(), CardError> {
        self.inner.set_wake_avail(frames)
    }
    fn wait(&mut self, timeout: Duration) -> Result<(), CardError> {
        self.inner.wait(timeout)
    }
    fn avail(&mut self) -> Result<usize, CardError> {
        self.inner.avail()
    }
    fn delay(&mut self) -> Result<usize, CardError> {
        self.inner.delay()
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, CardError> {
        let frames = self.frames(bytes.len());
        let card = &mut self.scratch[..frames * self.slice.card_frame_bytes()];
        self.slice.scatter(bytes, card, frames);
        self.inner.write(card)
    }

    fn read(&mut self, bytes: &mut [u8]) -> Result<usize, CardError> {
        let frames = self.frames(bytes.len());
        let card = &mut self.scratch[..frames * self.slice.card_frame_bytes()];
        let got = self.inner.read(card)?.min(frames);
        self.slice.gather(card, bytes, got);
        Ok(got)
    }

    fn start(&mut self) -> Result<(), CardError> {
        self.inner.start()
    }
    fn recover(&mut self) -> Result<(), CardError> {
        self.inner.recover()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16-bit samples `ch * 100 + frame`, `channels` per frame.
    fn frames(channels: usize, n: usize) -> Vec<u8> {
        (0..n)
            .flat_map(|f| (0..channels).map(move |c| (c * 100 + f) as u16))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    fn samples(bytes: &[u8]) -> Vec<u16> {
        bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect()
    }

    #[test]
    fn a_slice_must_fit_the_card() {
        assert!(CardSlice::new(16, 15, 2, PcmFormat::S16).is_ok());
        assert!(CardSlice::new(16, 16, 2, PcmFormat::S16).is_err());
        assert!(CardSlice::new(16, 0, 2, PcmFormat::S16).is_err());
        assert!(CardSlice::new(16, 1, 0, PcmFormat::S16).is_err());
        assert!(CardSlice::new(2, 1, 2, PcmFormat::S32).unwrap().is_whole());
        assert!(!CardSlice::new(16, 1, 2, PcmFormat::S32).unwrap().is_whole());
    }

    #[test]
    fn capture_carries_only_the_slice() {
        // A mixer's main mix on USB 15-16 of 16.
        let s = CardSlice::new(16, 15, 2, PcmFormat::S16).unwrap();
        let card = frames(16, 3);
        let mut out = vec![0u8; 3 * s.frame_bytes()];
        s.gather(&card, &mut out, 3);
        assert_eq!(samples(&out), [1400, 1500, 1401, 1501, 1402, 1502]);
        // Fewer frames delivered than room for: only those are copied.
        let mut out = vec![0xFFu8; 3 * s.frame_bytes()];
        s.gather(&card, &mut out, 1);
        assert_eq!(&samples(&out)[..2], [1400, 1500]);
        assert_eq!(&samples(&out)[2..], [0xFFFF; 4]);
    }

    #[test]
    fn playback_writes_the_slice_and_silence_elsewhere() {
        let s = CardSlice::new(4, 2, 2, PcmFormat::S24Packed).unwrap();
        assert_eq!((s.card_frame_bytes(), s.frame_bytes()), (12, 6));
        let input: Vec<u8> = (1..=12).collect(); // two frames of the slice
        let mut card = vec![0u8; 2 * s.card_frame_bytes()];
        s.scatter(&input, &mut card, 2);
        assert_eq!(
            card,
            [
                0, 0, 0, 1, 2, 3, 4, 5, 6, 0, 0, 0, //
                0, 0, 0, 7, 8, 9, 10, 11, 12, 0, 0, 0
            ]
        );
    }

    /// A card that records what it was written and plays back a fixed
    /// capture.
    struct Fake {
        channels: usize,
        written: Vec<u8>,
        captured: Vec<u8>,
    }

    impl CardPcm for Fake {
        fn rate(&self) -> u32 {
            48_000
        }
        fn channels(&self) -> usize {
            self.channels
        }
        fn format(&self) -> PcmFormat {
            PcmFormat::S16
        }
        fn period_frames(&self) -> usize {
            2
        }
        fn buffer_frames(&self) -> usize {
            4
        }
        fn set_wake_avail(&mut self, _: usize) -> Result<(), CardError> {
            Ok(())
        }
        fn wait(&mut self, _: Duration) -> Result<(), CardError> {
            Ok(())
        }
        fn avail(&mut self) -> Result<usize, CardError> {
            Ok(4)
        }
        fn delay(&mut self) -> Result<usize, CardError> {
            Ok(0)
        }
        fn write(&mut self, bytes: &[u8]) -> Result<usize, CardError> {
            self.written.extend_from_slice(bytes);
            Ok(bytes.len() / (self.channels * 2))
        }
        fn read(&mut self, bytes: &mut [u8]) -> Result<usize, CardError> {
            let n = bytes.len().min(self.captured.len());
            bytes[..n].copy_from_slice(&self.captured[..n]);
            Ok(n / (self.channels * 2))
        }
        fn start(&mut self) -> Result<(), CardError> {
            Ok(())
        }
        fn recover(&mut self) -> Result<(), CardError> {
            Ok(())
        }
    }

    #[test]
    fn a_sliced_card_shows_the_slice_to_the_bridge() {
        let fake = Fake {
            channels: 16,
            written: Vec::new(),
            captured: frames(16, 4),
        };
        let mut c = SlicedCard::new(Box::new(fake), 15, 2).unwrap();
        assert_eq!(c.channels(), 2);
        let mut two = vec![0u8; 2 * 2 * 2];
        assert_eq!(c.read(&mut two).unwrap(), 2);
        assert_eq!(samples(&two), [1400, 1500, 1401, 1501]);
        // More than a buffer: limited to the scratch, a short transfer.
        let mut many = vec![0u8; 10 * 2 * 2];
        assert_eq!(c.read(&mut many).unwrap(), 4);
        assert_eq!(c.write(&[1, 0, 2, 0]).unwrap(), 1);
        // The whole card is passed through unwrapped.
        let whole = Fake {
            channels: 2,
            written: Vec::new(),
            captured: Vec::new(),
        };
        let w = SlicedCard::wrap(Box::new(whole), 1, 2).unwrap();
        assert_eq!(w.channels(), 2);
        assert!(
            SlicedCard::wrap(
                Box::new(Fake {
                    channels: 2,
                    written: Vec::new(),
                    captured: Vec::new(),
                }),
                2,
                2
            )
            .is_err()
        );
    }
}
