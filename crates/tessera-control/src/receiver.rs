//! One receiver subscription exchange, driven by the daemon's existing authority.
//!
//! This is wire lifecycle code, not an admission policy or a routing store. The
//! caller must resolve identities, enforce locks/ownership, bind expectations to
//! an operation, and authorize dispatch after the before-read. In particular a
//! matching query grants no runtime-report or saved-name-echo rights. No network
//! I/O, allocation, retry, persistence or automatic restoration happens here.
use crate::arc::{self, Legacy, Source};

/// One expected channel in the operation's reviewed scope, not device completeness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection<'a> {
    /// One-based receiver channel identity.
    pub rx: u16,
    /// Receiver-reported source-name pair.
    pub source: Option<Source<'a>>,
    /// Before-read status to compare when supplied; never a media assertion.
    pub raw_status: Option<u32>,
}

/// A bounded exchange failure; a transport failure must be supplied by the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Target, expected scope or desired selection is invalid or unchanged.
    Plan,
    /// A request/reply arrived in a phase where it cannot be used.
    Phase,
    /// Matched wire decoding or encoding failed.
    Wire(arc::Error),
    /// A selection in the reviewed scope differs from the expectation.
    SelectionChanged,
    /// The returned pages omit a reviewed channel or repeat an identity.
    IncompleteScope,
    /// The caller reported a timeout, send/receive failure or context invalidation.
    Interrupted,
}

/// Facts established by this exchange. None of these outcomes asserts audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No mutation request was emitted; stop without a write.
    Refused(Error),
    /// A mutation may have escaped; reconcile before any later operation.
    Unknown(Error),
    /// Fresh source names match the requested scope; audio remains unverified.
    NamesMatch,
}

/// The next caller action. The authority must revalidate before `WriteReady`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// Another read page can be requested.
    ReadReady,
    /// Before-read expectations match; this is not permission to dispatch.
    WriteReady,
    /// Waiting for a response to the one emitted request.
    AwaitingReply,
    /// The exchange is terminal, including after interruption.
    Finished(Outcome),
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Read {
        after: bool,
        first: u16,
    },
    ReadReply {
        after: bool,
        first: u16,
        sequence: u16,
    },
    Write,
    WriteReply {
        sequence: u16,
    },
    Finished(Outcome),
}

/// A single Connect, Replace or Disconnect wire lifecycle, with no blind resend.
/// The operation borrows immutable daemon-resolved names and expected selections.
/// It cannot grant authority or consume/deduplicate an actor expectation token.
#[derive(Debug)]
pub struct Operation<'a> {
    profile: Legacy,
    rx_count: u16,
    rx: u16,
    expected: &'a [Selection<'a>],
    desired: Option<Source<'a>>,
    seen: [u64; 4],
    phase: Phase,
    attempted: bool,
}

impl<'a> Operation<'a> {
    /// Validate the single-channel change before emitting any request.
    /// Expected scope includes the target and any continuity witnesses. It is
    /// not inferred from a raw slot byte, a saved snapshot or silent channels.
    pub fn new(
        profile: Legacy,
        rx_count: u16,
        rx: u16,
        expected: &'a [Selection<'a>],
        desired: Option<Source<'a>>,
    ) -> Result<Self, Error> {
        arc::subscription_body_len(rx_count, rx, desired).map_err(Error::Wire)?;
        if expected.is_empty() || expected.len() > usize::from(rx_count) {
            return Err(Error::Plan);
        }
        for (index, selection) in expected.iter().enumerate() {
            if selection.rx == 0
                || selection.rx > rx_count
                || expected[..index].iter().any(|s| s.rx == selection.rx)
            {
                return Err(Error::Plan);
            }
            arc::subscription_body_len(rx_count, selection.rx, selection.source)
                .map_err(Error::Wire)?;
        }
        let target = expected.iter().find(|s| s.rx == rx).ok_or(Error::Plan)?;
        if target.source == desired {
            return Err(Error::Plan);
        }
        Ok(Self {
            profile,
            rx_count,
            rx,
            expected,
            desired,
            seen: [0; 4],
            phase: Phase::Read {
                after: false,
                first: 1,
            },
            attempted: false,
        })
    }

    /// Current wire progress, independent of authority admission.
    pub fn progress(&self) -> Progress {
        match self.phase {
            Phase::Read { .. } => Progress::ReadReady,
            Phase::Write => Progress::WriteReady,
            Phase::ReadReply { .. } | Phase::WriteReply { .. } => Progress::AwaitingReply,
            Phase::Finished(outcome) => Progress::Finished(outcome),
        }
    }

    /// Whether mutation bytes were emitted and may have reached the transport.
    /// This is marked before the caller can submit those bytes to a socket.
    pub fn mutation_attempted(&self) -> bool {
        self.attempted
    }

    /// Build exactly the next request. Calling while awaiting a reply cannot
    /// resend. After `WriteReady`, the authority must separately authorize the
    /// write and refuse missing report-preservation premises before calling.
    pub fn request<'b>(&mut self, sequence: u16, output: &'b mut [u8]) -> Result<&'b [u8], Error> {
        if sequence == 0 {
            return self.stop(Error::Phase);
        }
        let (request, next) = match self.phase {
            Phase::Read { after, first } => (
                self.profile.rx_list_request(sequence, first, 0, output),
                Phase::ReadReply {
                    after,
                    first,
                    sequence,
                },
            ),
            Phase::Write if !self.attempted => (
                self.profile.subscription_request(
                    sequence,
                    self.rx_count,
                    self.rx,
                    self.desired,
                    output,
                ),
                Phase::WriteReply { sequence },
            ),
            _ => return self.stop(Error::Phase),
        };
        match request {
            Ok(request) => {
                if matches!(next, Phase::WriteReply { .. }) {
                    self.attempted = true;
                }
                self.phase = next;
                Ok(request)
            }
            Err(error) => self.stop(Error::Wire(error)),
        }
    }

    /// Accept only the original sequence/opcode/profile, then compare names.
    /// A successful write ACK merely permits a fresh read; it proves no effect.
    pub fn receive(&mut self, bytes: &[u8]) -> Result<Progress, Error> {
        let result = self.accept(bytes);
        match result {
            Ok(()) => Ok(self.progress()),
            Err(error) => self.stop(error),
        }
    }

    fn accept(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let (after, first, sequence) = match self.phase {
            Phase::WriteReply { sequence } => {
                self.profile
                    .reply(sequence, 0x3010, bytes)
                    .map_err(Error::Wire)?;
                self.seen = [0; 4];
                self.phase = Phase::Read {
                    after: true,
                    first: 1,
                };
                return Ok(());
            }
            Phase::ReadReply {
                after,
                first,
                sequence,
            } => (after, first, sequence),
            _ => return Err(Error::Phase),
        };
        let reply = self
            .profile
            .reply_page(sequence, 0x3000, bytes)
            .map_err(Error::Wire)?;
        let page = reply.rx_channels().map_err(Error::Wire)?;
        if page.is_empty() {
            return Err(Error::IncompleteScope);
        }
        for record in page.records() {
            if record.id > self.rx_count {
                return Err(Error::IncompleteScope);
            }
            let index = usize::from(record.id - 1);
            let mask = 1u64 << (index % 64);
            if self.seen[index / 64] & mask != 0 {
                return Err(Error::IncompleteScope);
            }
            self.seen[index / 64] |= mask;
            if let Some(expected) = self.expected.iter().find(|s| s.rx == record.id) {
                let source = if after && record.id == self.rx {
                    self.desired
                } else {
                    expected.source
                };
                if record.source != source
                    || (!after && expected.raw_status.is_some_and(|s| s != record.raw_status))
                {
                    return Err(Error::SelectionChanged);
                }
            }
        }
        if page.has_more() {
            let first = first
                .checked_add(page.len() as u16)
                .ok_or(Error::IncompleteScope)?;
            self.phase = Phase::Read { after, first };
        } else {
            if self.expected.iter().any(|s| {
                let index = usize::from(s.rx - 1);
                self.seen[index / 64] & (1u64 << (index % 64)) == 0
            }) {
                return Err(Error::IncompleteScope);
            }
            self.phase = if after {
                Phase::Finished(Outcome::NamesMatch)
            } else {
                Phase::Write
            };
        }
        Ok(())
    }

    /// Stop on transport loss, deadline, cancellation or actor context change.
    /// Interruption cannot restore, retry or make another mutation available.
    pub fn interrupt(&mut self) -> Outcome {
        if let Phase::Finished(outcome) = self.phase {
            return outcome;
        }
        let outcome = self.failure(Error::Interrupted);
        self.phase = Phase::Finished(outcome);
        outcome
    }

    fn failure(&self, error: Error) -> Outcome {
        if self.attempted {
            Outcome::Unknown(error)
        } else {
            Outcome::Refused(error)
        }
    }

    fn stop<T>(&mut self, error: Error) -> Result<T, Error> {
        // A duplicate client action cannot rewrite a retained terminal result.
        if !matches!(self.phase, Phase::Finished(_)) {
            self.phase = Phase::Finished(self.failure(error));
        }
        Err(error)
    }
}
