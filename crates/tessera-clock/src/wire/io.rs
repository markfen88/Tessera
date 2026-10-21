//! Checked byte cursors and the codec error types.

use core::fmt;

/// Why a parse failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// Fewer bytes than the message needs.
    Truncated,
    /// `versionPTP` is not 1 (for example a PTPv2 packet on a shared group,
    /// spec:3). Callers count these separately and drop them silently.
    NotPtpV1(u16),
    /// `control` is not one of 0..=4.
    UnknownControl(u8),
}

/// A parse failure with the byte offset at which it was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError {
    /// Offset into the datagram.
    pub offset: usize,
    /// What went wrong.
    pub kind: ParseErrorKind,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ParseErrorKind::Truncated => {
                write!(f, "truncated PTPv1 message at offset {}", self.offset)
            }
            ParseErrorKind::NotPtpV1(v) => write!(f, "not PTPv1 (versionPTP {v})"),
            ParseErrorKind::UnknownControl(c) => {
                write!(
                    f,
                    "unknown PTPv1 control value {c} at offset {}",
                    self.offset
                )
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ParseError {}

/// Why an encode failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// The output buffer is shorter than the message.
    BufferTooSmall {
        /// Bytes required.
        needed: usize,
    },
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::BufferTooSmall { needed } => {
                write!(f, "output buffer too small, {needed} bytes needed")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for EncodeError {}

/// Big-endian checked reader.
#[derive(Debug)]
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn truncated(&self) -> ParseError {
        ParseError {
            offset: self.pos,
            kind: ParseErrorKind::Truncated,
        }
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn take<const N: usize>(&mut self) -> Result<[u8; N], ParseError> {
        let end = self.pos.checked_add(N).ok_or_else(|| self.truncated())?;
        let s = self
            .buf
            .get(self.pos..end)
            .ok_or_else(|| self.truncated())?;
        let a: [u8; N] = s.try_into().map_err(|_| self.truncated())?;
        self.pos = end;
        Ok(a)
    }

    pub(crate) fn skip(&mut self, n: usize) -> Result<(), ParseError> {
        let end = self.pos.checked_add(n).ok_or_else(|| self.truncated())?;
        if end > self.buf.len() {
            return Err(self.truncated());
        }
        self.pos = end;
        Ok(())
    }

    pub(crate) fn u8(&mut self) -> Result<u8, ParseError> {
        let [b] = self.take::<1>()?;
        Ok(b)
    }
    pub(crate) fn i8(&mut self) -> Result<i8, ParseError> {
        Ok(i8::from_be_bytes(self.take::<1>()?))
    }
    pub(crate) fn u16(&mut self) -> Result<u16, ParseError> {
        Ok(u16::from_be_bytes(self.take()?))
    }
    pub(crate) fn i16(&mut self) -> Result<i16, ParseError> {
        Ok(i16::from_be_bytes(self.take()?))
    }
    pub(crate) fn u32(&mut self) -> Result<u32, ParseError> {
        Ok(u32::from_be_bytes(self.take()?))
    }
    pub(crate) fn i32(&mut self) -> Result<i32, ParseError> {
        Ok(i32::from_be_bytes(self.take()?))
    }

    /// All remaining bytes.
    pub(crate) fn rest(&mut self) -> &'a [u8] {
        let s = self.buf.get(self.pos..).unwrap_or(&[]);
        self.pos = self.buf.len();
        s
    }
}

/// Big-endian checked writer over a caller buffer.
#[derive(Debug)]
pub(crate) struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    pub(crate) fn new(buf: &'a mut [u8]) -> Self {
        Writer { buf, pos: 0 }
    }

    pub(crate) fn len(&self) -> usize {
        self.pos
    }

    fn too_small(&self, extra: usize) -> EncodeError {
        EncodeError::BufferTooSmall {
            needed: self.pos.saturating_add(extra),
        }
    }

    pub(crate) fn put(&mut self, b: &[u8]) -> Result<(), EncodeError> {
        let end = self
            .pos
            .checked_add(b.len())
            .ok_or_else(|| self.too_small(b.len()))?;
        let pos = self.pos;
        let dst = self
            .buf
            .get_mut(pos..end)
            .ok_or(EncodeError::BufferTooSmall { needed: end })?;
        dst.copy_from_slice(b);
        self.pos = end;
        Ok(())
    }

    pub(crate) fn zeros(&mut self, n: usize) -> Result<(), EncodeError> {
        let end = self.pos.checked_add(n).ok_or_else(|| self.too_small(n))?;
        let pos = self.pos;
        let dst = self
            .buf
            .get_mut(pos..end)
            .ok_or(EncodeError::BufferTooSmall { needed: end })?;
        dst.fill(0);
        self.pos = end;
        Ok(())
    }

    pub(crate) fn u8(&mut self, v: u8) -> Result<(), EncodeError> {
        self.put(&[v])
    }
    pub(crate) fn i8(&mut self, v: i8) -> Result<(), EncodeError> {
        self.put(&v.to_be_bytes())
    }
    pub(crate) fn u16(&mut self, v: u16) -> Result<(), EncodeError> {
        self.put(&v.to_be_bytes())
    }
    pub(crate) fn i16(&mut self, v: i16) -> Result<(), EncodeError> {
        self.put(&v.to_be_bytes())
    }
    pub(crate) fn u32(&mut self, v: u32) -> Result<(), EncodeError> {
        self.put(&v.to_be_bytes())
    }
    pub(crate) fn i32(&mut self, v: i32) -> Result<(), EncodeError> {
        self.put(&v.to_be_bytes())
    }
}
