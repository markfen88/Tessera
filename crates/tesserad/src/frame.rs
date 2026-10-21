//! Length-delimited frames for the control socket (ADR 0008).
//!
//! A frame is a `u32` big-endian payload length followed by that many bytes
//! (JSON, see `tessera-control-types`). This module is sans-IO and total: no
//! input can make it panic, and the memory it holds is bounded by the frame
//! cap plus the size of the last chunk pushed into it.
//!
//! The decoder rejects an oversized frame as soon as its 4-byte header is
//! complete, without waiting for (or allocating for) the payload. After that
//! error the byte stream cannot be re-synchronised, so the decoder stays in
//! the error state and the connection must be closed.

use std::fmt;

/// Bytes in the length prefix.
pub const HEADER_LEN: usize = 4;

/// Default cap on the payload length (ADR 0010: 1 MiB).
pub const DEFAULT_MAX_FRAME_LEN: usize = 1 << 20;

/// Why a frame was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// The announced (or supplied) payload is longer than the cap.
    TooLarge {
        /// The announced length.
        len: u64,
        /// The cap.
        max: usize,
    },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { len, max } => {
                write!(f, "frame of {len} bytes exceeds the {max} byte limit")
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// Prefix `payload` with its length.
///
/// # Errors
/// [`FrameError::TooLarge`] if the payload is longer than `max` (or than
/// `u32::MAX`).
pub fn encode_frame(payload: &[u8], max: usize) -> Result<Vec<u8>, FrameError> {
    let too_large = || FrameError::TooLarge {
        len: payload.len() as u64,
        max,
    };
    if payload.len() > max {
        return Err(too_large());
    }
    let len = u32::try_from(payload.len()).map_err(|_| too_large())?;
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Incremental frame decoder: push bytes as they arrive, pull whole frames.
#[derive(Debug)]
pub struct FrameDecoder {
    max: usize,
    buf: Vec<u8>,
    pos: usize,
    failed: Option<FrameError>,
}

impl FrameDecoder {
    /// A decoder that refuses payloads longer than `max` bytes.
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self {
            max,
            buf: Vec::new(),
            pos: 0,
            failed: None,
        }
    }

    /// Append received bytes. Ignored after an error.
    pub fn push(&mut self, data: &[u8]) {
        if self.failed.is_none() {
            self.buf.extend_from_slice(data);
        }
    }

    /// The next complete frame's payload, `None` if more bytes are needed.
    ///
    /// # Errors
    /// [`FrameError::TooLarge`], and the same error again on every later call.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        let avail = self.buf.get(self.pos..).unwrap_or_default();
        let Some(hdr) = avail.get(..HEADER_LEN) else {
            return Ok(None);
        };
        let mut raw = [0u8; HEADER_LEN];
        raw.copy_from_slice(hdr);
        let len = u32::from_be_bytes(raw);
        let too_large = u64::from(len) > self.max as u64;
        if too_large {
            let e = FrameError::TooLarge {
                len: u64::from(len),
                max: self.max,
            };
            self.failed = Some(e);
            self.buf = Vec::new();
            self.pos = 0;
            return Err(e);
        }
        let Some(end) = HEADER_LEN.checked_add(len as usize) else {
            return Ok(None);
        };
        let Some(body) = avail.get(HEADER_LEN..end) else {
            return Ok(None);
        };
        let frame = body.to_vec();
        self.pos += end;
        if self.pos >= self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        } else if self.pos >= 64 * 1024 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        Ok(Some(frame))
    }

    /// True if bytes of an incomplete frame are buffered (used to time out
    /// peers that stall in the middle of a frame).
    #[must_use]
    pub fn is_partial(&self) -> bool {
        self.failed.is_none() && self.buf.len() > self.pos
    }

    /// Bytes currently held.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn round_trip_single_frame() {
        let f = encode_frame(b"hello", 16).unwrap();
        assert_eq!(f, [0, 0, 0, 5, b'h', b'e', b'l', b'l', b'o']);
        let mut d = FrameDecoder::new(16);
        d.push(&f);
        assert_eq!(d.next_frame().unwrap().unwrap(), b"hello");
        assert_eq!(d.next_frame().unwrap(), None);
        assert!(!d.is_partial());
    }

    #[test]
    fn empty_frame_is_a_frame() {
        let mut d = FrameDecoder::new(16);
        d.push(&encode_frame(b"", 16).unwrap());
        assert_eq!(d.next_frame().unwrap().unwrap(), b"");
    }

    #[test]
    fn oversized_header_fails_without_payload_and_stays_failed() {
        let mut d = FrameDecoder::new(8);
        d.push(&[0xff, 0xff, 0xff, 0xff]);
        let e = d.next_frame().unwrap_err();
        assert_eq!(
            e,
            FrameError::TooLarge {
                len: u64::from(u32::MAX),
                max: 8
            }
        );
        d.push(&[0; 1000]);
        assert_eq!(d.next_frame().unwrap_err(), e);
        assert_eq!(d.buffered(), 0);
    }

    #[test]
    fn encode_refuses_oversized_payload() {
        assert!(matches!(
            encode_frame(&[0; 9], 8),
            Err(FrameError::TooLarge { len: 9, max: 8 })
        ));
    }

    #[test]
    fn partial_frame_is_reported() {
        let mut d = FrameDecoder::new(64);
        let f = encode_frame(b"abcdef", 64).unwrap();
        d.push(&f[..2]);
        assert!(d.is_partial());
        d.push(&f[2..7]);
        assert_eq!(d.next_frame().unwrap(), None);
        d.push(&f[7..]);
        assert_eq!(d.next_frame().unwrap().unwrap(), b"abcdef");
        assert!(!d.is_partial());
    }

    proptest! {
        /// Any sequence of payloads, split at arbitrary points, comes back
        /// unchanged and in order.
        #[test]
        fn frames_survive_arbitrary_chunking(
            payloads in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..300), 0..12),
            cuts in proptest::collection::vec(1usize..64, 1..40),
        ) {
            let mut wire = Vec::new();
            for p in &payloads {
                wire.extend_from_slice(&encode_frame(p, 512).unwrap());
            }
            let mut d = FrameDecoder::new(512);
            let mut got = Vec::new();
            let mut at = 0;
            let mut i = 0;
            while at < wire.len() {
                let n = cuts[i % cuts.len()].min(wire.len() - at);
                i += 1;
                d.push(&wire[at..at + n]);
                at += n;
                while let Some(f) = d.next_frame().unwrap() {
                    got.push(f);
                }
            }
            prop_assert_eq!(got, payloads);
            prop_assert!(!d.is_partial());
        }

        /// Arbitrary bytes never panic, never yield a frame longer than the
        /// cap, and never make the decoder hold more than cap + header +
        /// one chunk.
        #[test]
        fn garbage_is_total_and_bounded(
            chunks in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..200), 0..40),
            max in 0usize..300,
        ) {
            let mut d = FrameDecoder::new(max);
            let mut failed = false;
            for c in &chunks {
                d.push(c);
                loop {
                    match d.next_frame() {
                        Ok(Some(f)) => {
                            prop_assert!(f.len() <= max);
                            prop_assert!(!failed);
                        }
                        Ok(None) => break,
                        Err(_) => { failed = true; break; }
                    }
                }
                prop_assert!(d.buffered() <= max + HEADER_LEN + 200);
            }
        }

        /// A header that announces more than the cap is refused as soon as
        /// the four header bytes are in, whatever follows.
        #[test]
        fn oversize_is_detected_at_the_header(len in 9u32..=u32::MAX, tail in proptest::collection::vec(any::<u8>(), 0..32)) {
            let mut d = FrameDecoder::new(8);
            d.push(&len.to_be_bytes());
            d.push(&tail);
            prop_assert!(d.next_frame().is_err());
        }
    }
}
