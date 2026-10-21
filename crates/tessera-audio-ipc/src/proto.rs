//! Handshake messages and their sans-IO codec (no sockets here).
//!
//! Wire format: all integers little-endian.
//!
//! ```text
//! frame   := u32 payload_len | payload
//! payload := u16 version | u16 kind | body
//! ```
//!
//! Bodies (see the crate docs for the full table). Decoding is total: any
//! byte string yields either a message or a [`ProtoError`]; nothing panics,
//! every length is checked before use and trailing bytes are an error.

use crate::error::ProtoError;
use crate::layout::{Direction, Limits, RingParams, SampleFormat};

/// The protocol version spoken by this crate.
pub const PROTO_VERSION: u16 = 1;
/// Smallest valid payload (version + kind).
pub const MIN_PAYLOAD_LEN: u32 = 4;
/// Default frame size cap for the handshake socket. Every valid message is
/// well under 1 KiB; ADR 0010's 1 MiB is the upper bound a deployment may
/// configure.
pub const DEFAULT_MAX_FRAME_LEN: u32 = 64 * 1024;
/// Longest string field in bytes.
pub const MAX_STRING_LEN: usize = 255;
/// Number of descriptors carried by an `Attach`: ring memfd, clock overlay
/// memfd, client end of the wake socketpair (in that order).
pub const ATTACH_FD_COUNT: usize = 3;

const KIND_HELLO: u16 = 1;
const KIND_ATTACH: u16 = 2;
const KIND_REJECT: u16 = 3;
const KIND_DETACH: u16 = 4;

/// Client request for an audio endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// `Rx` (network to app) or `Tx` (app to network).
    pub direction: Direction,
    /// Requested sample format.
    pub format: SampleFormat,
    /// First endpoint channel requested.
    pub first_channel: u32,
    /// Number of channels requested (>= 1).
    pub channel_count: u32,
    /// Requested sample rate in Hz; 0 means the device default.
    pub sample_rate_hz: u32,
    /// Largest period (frames) the client will use; 0 means default. The
    /// daemon sizes the ring capacity from it (ADR 0006).
    pub max_period_frames: u32,
    /// Endpoint (logical device) name; may be empty for the default device.
    pub endpoint: String,
    /// Free-form client name for status display.
    pub client: String,
}

/// The daemon's grant. Sent with [`ATTACH_FD_COUNT`] descriptors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attach {
    /// Daemon-assigned session identifier.
    pub session_id: u64,
    /// Ring parameters (must match the ring header; checked on attach).
    pub params: RingParams,
    /// Effective latency in frames at attach time.
    pub effective_latency_frames: u64,
}

/// Why a request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectCode {
    /// Protocol version not supported.
    UnsupportedVersion,
    /// No such endpoint.
    UnknownEndpoint,
    /// The request is malformed or unsatisfiable.
    BadRequest,
    /// The endpoint/channels are already in use.
    Busy,
    /// Not permitted for this peer.
    PermissionDenied,
    /// Internal daemon error.
    Internal,
    /// A code from a newer peer.
    Other(u32),
}

impl RejectCode {
    fn to_u32(self) -> u32 {
        match self {
            RejectCode::UnsupportedVersion => 1,
            RejectCode::UnknownEndpoint => 2,
            RejectCode::BadRequest => 3,
            RejectCode::Busy => 4,
            RejectCode::PermissionDenied => 5,
            RejectCode::Internal => 6,
            RejectCode::Other(v) => v,
        }
    }
    fn from_u32(v: u32) -> Self {
        match v {
            1 => RejectCode::UnsupportedVersion,
            2 => RejectCode::UnknownEndpoint,
            3 => RejectCode::BadRequest,
            4 => RejectCode::Busy,
            5 => RejectCode::PermissionDenied,
            6 => RejectCode::Internal,
            other => RejectCode::Other(other),
        }
    }
}

/// A refusal of a `Hello`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reject {
    /// Machine-readable reason.
    pub code: RejectCode,
    /// Human-readable text (<= 255 bytes).
    pub message: String,
}

/// Why a session is ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachReason {
    /// The client is going away.
    ClientClosing,
    /// The daemon is shutting down.
    DaemonShutdown,
    /// The endpoint was removed or reconfigured.
    EndpointRemoved,
    /// The peer violated the protocol.
    ProtocolError,
    /// A reason from a newer peer.
    Other(u32),
}

impl DetachReason {
    fn to_u32(self) -> u32 {
        match self {
            DetachReason::ClientClosing => 1,
            DetachReason::DaemonShutdown => 2,
            DetachReason::EndpointRemoved => 3,
            DetachReason::ProtocolError => 4,
            DetachReason::Other(v) => v,
        }
    }
    fn from_u32(v: u32) -> Self {
        match v {
            1 => DetachReason::ClientClosing,
            2 => DetachReason::DaemonShutdown,
            3 => DetachReason::EndpointRemoved,
            4 => DetachReason::ProtocolError,
            other => DetachReason::Other(other),
        }
    }
}

/// End of session (either direction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detach {
    /// Why.
    pub reason: DetachReason,
}

/// Any handshake message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Client request.
    Hello(Hello),
    /// Daemon grant (carries descriptors).
    Attach(Attach),
    /// Daemon refusal.
    Reject(Reject),
    /// Session end.
    Detach(Detach),
}

impl Hello {
    /// Checks the request against `limits`.
    pub fn validate(&self, limits: &Limits) -> Result<(), ProtoError> {
        if self.channel_count == 0 || self.channel_count > limits.max_channels {
            return Err(ProtoError::BadField("channel_count"));
        }
        if self.first_channel.checked_add(self.channel_count).is_none() {
            return Err(ProtoError::BadField("first_channel"));
        }
        if self.sample_rate_hz > limits.max_sample_rate_hz {
            return Err(ProtoError::BadField("sample_rate_hz"));
        }
        Ok(())
    }

    /// Builds ring parameters for this request once the daemon has chosen the
    /// sample rate and capacity.
    pub fn ring_params(&self, sample_rate_hz: u32, capacity_frames: u64) -> RingParams {
        RingParams {
            direction: self.direction,
            format: self.format,
            channels: self.channel_count,
            first_channel: self.first_channel,
            sample_rate_hz,
            capacity_frames,
        }
    }
}

impl Attach {
    /// Checks the grant against `limits`.
    pub fn validate(&self, limits: &Limits) -> Result<(), ProtoError> {
        self.params
            .validate(limits)
            .map(|_| ())
            .map_err(|_| ProtoError::BadField("params"))
    }
}

impl Message {
    /// Number of descriptors that accompany this message on the wire.
    pub fn expected_fds(&self) -> usize {
        match self {
            Message::Attach(_) => ATTACH_FD_COUNT,
            _ => 0,
        }
    }

    /// Encodes the full frame (length prefix included).
    pub fn encode_frame(&self) -> Result<Vec<u8>, ProtoError> {
        let mut p: Vec<u8> = Vec::with_capacity(96);
        p.extend_from_slice(&PROTO_VERSION.to_le_bytes());
        match self {
            Message::Hello(h) => {
                p.extend_from_slice(&KIND_HELLO.to_le_bytes());
                p.push(h.direction.to_u32() as u8);
                p.push(h.format.to_u32() as u8);
                p.extend_from_slice(&0u16.to_le_bytes());
                p.extend_from_slice(&h.first_channel.to_le_bytes());
                p.extend_from_slice(&h.channel_count.to_le_bytes());
                p.extend_from_slice(&h.sample_rate_hz.to_le_bytes());
                p.extend_from_slice(&h.max_period_frames.to_le_bytes());
                p.extend_from_slice(&0u32.to_le_bytes()); // flags
                put_str(&mut p, &h.endpoint)?;
                put_str(&mut p, &h.client)?;
            }
            Message::Attach(a) => {
                p.extend_from_slice(&KIND_ATTACH.to_le_bytes());
                p.extend_from_slice(&a.session_id.to_le_bytes());
                p.push(a.params.direction.to_u32() as u8);
                p.push(a.params.format.to_u32() as u8);
                p.extend_from_slice(&0u16.to_le_bytes());
                p.extend_from_slice(&a.params.channels.to_le_bytes());
                p.extend_from_slice(&a.params.first_channel.to_le_bytes());
                p.extend_from_slice(&a.params.sample_rate_hz.to_le_bytes());
                p.extend_from_slice(&a.params.capacity_frames.to_le_bytes());
                p.extend_from_slice(&a.effective_latency_frames.to_le_bytes());
                p.extend_from_slice(&0u32.to_le_bytes()); // flags
            }
            Message::Reject(r) => {
                p.extend_from_slice(&KIND_REJECT.to_le_bytes());
                p.extend_from_slice(&r.code.to_u32().to_le_bytes());
                put_str(&mut p, &r.message)?;
            }
            Message::Detach(d) => {
                p.extend_from_slice(&KIND_DETACH.to_le_bytes());
                p.extend_from_slice(&d.reason.to_u32().to_le_bytes());
            }
        }
        let len = u32::try_from(p.len()).map_err(|_| ProtoError::BadFrameLength(u32::MAX))?;
        let mut frame = Vec::with_capacity(p.len() + 4);
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&p);
        Ok(frame)
    }

    /// Decodes one payload (the bytes after the length prefix).
    pub fn decode_payload(payload: &[u8]) -> Result<Message, ProtoError> {
        let mut c = Cur(payload);
        let version = c.u16()?;
        if version != PROTO_VERSION {
            return Err(ProtoError::BadVersion(version));
        }
        let kind = c.u16()?;
        let msg = match kind {
            KIND_HELLO => {
                let direction = dir(c.u8()?)?;
                let format = fmt(c.u8()?)?;
                if c.u16()? != 0 {
                    return Err(ProtoError::BadField("reserved"));
                }
                let first_channel = c.u32()?;
                let channel_count = c.u32()?;
                let sample_rate_hz = c.u32()?;
                let max_period_frames = c.u32()?;
                if c.u32()? != 0 {
                    return Err(ProtoError::BadField("flags"));
                }
                let endpoint = c.string()?;
                let client = c.string()?;
                if channel_count == 0 {
                    return Err(ProtoError::BadField("channel_count"));
                }
                if first_channel.checked_add(channel_count).is_none() {
                    return Err(ProtoError::BadField("first_channel"));
                }
                Message::Hello(Hello {
                    direction,
                    format,
                    first_channel,
                    channel_count,
                    sample_rate_hz,
                    max_period_frames,
                    endpoint,
                    client,
                })
            }
            KIND_ATTACH => {
                let session_id = c.u64()?;
                let direction = dir(c.u8()?)?;
                let format = fmt(c.u8()?)?;
                if c.u16()? != 0 {
                    return Err(ProtoError::BadField("reserved"));
                }
                let channels = c.u32()?;
                let first_channel = c.u32()?;
                let sample_rate_hz = c.u32()?;
                let capacity_frames = c.u64()?;
                let effective_latency_frames = c.u64()?;
                if c.u32()? != 0 {
                    return Err(ProtoError::BadField("flags"));
                }
                Message::Attach(Attach {
                    session_id,
                    params: RingParams {
                        direction,
                        format,
                        channels,
                        first_channel,
                        sample_rate_hz,
                        capacity_frames,
                    },
                    effective_latency_frames,
                })
            }
            KIND_REJECT => {
                let code = RejectCode::from_u32(c.u32()?);
                let message = c.string()?;
                Message::Reject(Reject { code, message })
            }
            KIND_DETACH => Message::Detach(Detach {
                reason: DetachReason::from_u32(c.u32()?),
            }),
            other => return Err(ProtoError::BadKind(other)),
        };
        c.finish()?;
        Ok(msg)
    }

    /// Sans-IO frame decoder: returns `Ok(None)` if `buf` does not yet hold a
    /// whole frame, else the message and the number of bytes consumed.
    pub fn decode_frame(
        buf: &[u8],
        max_frame_len: u32,
    ) -> Result<Option<(Message, usize)>, ProtoError> {
        let Some(head) = buf.get(..4) else {
            return Ok(None);
        };
        let len = frame_len(head, max_frame_len)?;
        let total = 4usize
            .checked_add(len as usize)
            .ok_or(ProtoError::BadFrameLength(len))?;
        let Some(payload) = buf.get(4..total) else {
            return Ok(None);
        };
        Ok(Some((Message::decode_payload(payload)?, total)))
    }
}

/// Parses and range-checks a 4-byte length prefix.
pub(crate) fn frame_len(head: &[u8], max_frame_len: u32) -> Result<u32, ProtoError> {
    let mut a = [0u8; 4];
    a.copy_from_slice(head.get(..4).ok_or(ProtoError::Truncated)?);
    let len = u32::from_le_bytes(a);
    if len < MIN_PAYLOAD_LEN || len > max_frame_len {
        return Err(ProtoError::BadFrameLength(len));
    }
    Ok(len)
}

fn dir(v: u8) -> Result<Direction, ProtoError> {
    Direction::from_u32(u32::from(v)).ok_or(ProtoError::BadField("direction"))
}

fn fmt(v: u8) -> Result<SampleFormat, ProtoError> {
    SampleFormat::from_u32(u32::from(v)).ok_or(ProtoError::BadField("sample_format"))
}

fn put_str(p: &mut Vec<u8>, s: &str) -> Result<(), ProtoError> {
    let b = s.as_bytes();
    if b.len() > MAX_STRING_LEN {
        return Err(ProtoError::StringTooLong);
    }
    p.push(b.len() as u8);
    p.extend_from_slice(b);
    Ok(())
}

/// A bounds-checked byte cursor.
struct Cur<'a>(&'a [u8]);

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        if self.0.len() < n {
            return Err(ProtoError::Truncated);
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, ProtoError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, ProtoError> {
        let mut a = [0u8; 2];
        a.copy_from_slice(self.take(2)?);
        Ok(u16::from_le_bytes(a))
    }
    fn u32(&mut self) -> Result<u32, ProtoError> {
        let mut a = [0u8; 4];
        a.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(a))
    }
    fn u64(&mut self) -> Result<u64, ProtoError> {
        let mut a = [0u8; 8];
        a.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(a))
    }
    fn string(&mut self) -> Result<String, ProtoError> {
        let n = usize::from(self.u8()?);
        let b = self.take(n)?;
        let s = std::str::from_utf8(b).map_err(|_| ProtoError::BadField("string"))?;
        if s.chars().any(char::is_control) {
            return Err(ProtoError::BadField("string"));
        }
        Ok(s.to_owned())
    }
    fn finish(self) -> Result<(), ProtoError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(ProtoError::TrailingBytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> Message {
        Message::Hello(Hello {
            direction: Direction::Tx,
            format: SampleFormat::F32,
            first_channel: 4,
            channel_count: 8,
            sample_rate_hz: 48_000,
            max_period_frames: 1024,
            endpoint: "main".into(),
            client: "pw ü".into(),
        })
    }

    fn attach() -> Message {
        Message::Attach(Attach {
            session_id: 42,
            params: RingParams {
                direction: Direction::Rx,
                format: SampleFormat::I32,
                channels: 2,
                first_channel: 0,
                sample_rate_hz: 96_000,
                capacity_frames: 4096,
            },
            effective_latency_frames: 192,
        })
    }

    fn all() -> Vec<Message> {
        vec![
            hello(),
            attach(),
            Message::Reject(Reject {
                code: RejectCode::Other(99),
                message: "no".into(),
            }),
            Message::Detach(Detach {
                reason: DetachReason::DaemonShutdown,
            }),
        ]
    }

    #[test]
    fn roundtrip_all() {
        for m in all() {
            let f = m.encode_frame().unwrap();
            let (back, used) = Message::decode_frame(&f, DEFAULT_MAX_FRAME_LEN)
                .unwrap()
                .unwrap();
            assert_eq!(back, m);
            assert_eq!(used, f.len());
            // Every strict prefix is "need more data", never an error/panic.
            for cut in 0..f.len() {
                assert_eq!(
                    Message::decode_frame(&f[..cut], DEFAULT_MAX_FRAME_LEN).map(|o| o.is_some()),
                    Ok(false)
                );
            }
        }
    }

    #[test]
    fn rejects_garbage() {
        let f = hello().encode_frame().unwrap();
        // Trailing byte inside the payload.
        let mut t = f.clone();
        t.push(0);
        let n = (t.len() - 4) as u32;
        t[..4].copy_from_slice(&n.to_le_bytes());
        assert_eq!(
            Message::decode_frame(&t, DEFAULT_MAX_FRAME_LEN),
            Err(ProtoError::TrailingBytes)
        );
        // Bad version.
        let mut v = f.clone();
        v[4] = 9;
        assert_eq!(
            Message::decode_frame(&v, DEFAULT_MAX_FRAME_LEN),
            Err(ProtoError::BadVersion(9))
        );
        // Bad kind.
        let mut k = f.clone();
        k[6] = 77;
        assert_eq!(
            Message::decode_frame(&k, DEFAULT_MAX_FRAME_LEN),
            Err(ProtoError::BadKind(77))
        );
        // Huge and tiny length prefixes.
        assert!(Message::decode_frame(&u32::MAX.to_le_bytes(), 1 << 20).is_err());
        assert!(Message::decode_frame(&0u32.to_le_bytes(), 1 << 20).is_err());
        // Zero channels.
        let m = Message::Hello(Hello {
            channel_count: 0,
            ..match hello() {
                Message::Hello(h) => h,
                _ => unreachable!(),
            }
        });
        let f = m.encode_frame().unwrap();
        assert!(Message::decode_frame(&f, DEFAULT_MAX_FRAME_LEN).is_err());
        // Over-long string cannot be encoded.
        let m = Message::Reject(Reject {
            code: RejectCode::Busy,
            message: "x".repeat(256),
        });
        assert_eq!(m.encode_frame(), Err(ProtoError::StringTooLong));
    }
}
