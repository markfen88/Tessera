//! Time representations.
//!
//! All times inside the sans-IO core are `u64` nanoseconds of the local
//! monotonic clock (`CLOCK_MONOTONIC_RAW` on Linux, [`LocalNs`]) or media-clock /
//! leader nanoseconds ([`MediaNs`]). The platform layer converts kernel
//! timestamps (`CLOCK_REALTIME` or PHC) to [`LocalNs`] before they reach the
//! core, so the core never sees a clock that is stepped by NTP.

/// Nanoseconds of the local monotonic clock.
pub type LocalNs = u64;
/// Nanoseconds of the media clock (equal to the leader's PTP time as
/// carried on the wire).
pub type MediaNs = u64;

const NS_PER_S: u64 = 1_000_000_000;

/// A PTPv1 wire timestamp: `u32` seconds and `u32` nanoseconds (spec:2.7,
/// std §13.3, `likely`). The standard types the nanoseconds as signed 32-bit
/// with a sign convention; here the raw bits are kept and
/// [`Timestamp::to_nanos`] rejects anything outside `0..1_000_000_000`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timestamp {
    /// Whole seconds (wraps in 2106).
    pub seconds: u32,
    /// Nanoseconds within the second; valid values are `< 10^9`.
    pub nanoseconds: u32,
}

impl Timestamp {
    /// Builds a timestamp from nanoseconds. The seconds field is the low 32
    /// bits of the second count (wire truncation, ADR 0004).
    #[must_use]
    pub const fn from_nanos(ns: u64) -> Self {
        Timestamp {
            seconds: ((ns / NS_PER_S) & 0xFFFF_FFFF) as u32,
            nanoseconds: (ns % NS_PER_S) as u32,
        }
    }

    /// Total nanoseconds, or `None` if the nanoseconds field is not `< 10^9`.
    #[must_use]
    pub const fn to_nanos(self) -> Option<u64> {
        if self.nanoseconds as u64 >= NS_PER_S {
            None
        } else {
            Some(self.seconds as u64 * NS_PER_S + self.nanoseconds as u64)
        }
    }
}

/// Signed difference `a - b` of two `u64` times, saturating to `i64`.
#[must_use]
pub fn diff_ns(a: u64, b: u64) -> i64 {
    let d = i128::from(a) - i128::from(b);
    d.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// `base + delta` saturating to the `u64` range.
#[must_use]
pub fn add_signed(base: u64, delta: i64) -> u64 {
    let v = i128::from(base) + i128::from(delta);
    v.clamp(0, i128::from(u64::MAX)) as u64
}

/// `log2` seconds interval to nanoseconds, saturating (at least 1 ns).
#[must_use]
pub fn interval_ns(log2_s: i8) -> u64 {
    if log2_s >= 0 {
        let sh = u32::try_from(log2_s).unwrap_or(0);
        if sh >= 34 { u64::MAX } else { NS_PER_S << sh }
    } else {
        let sh = u32::try_from(-i16::from(log2_s)).unwrap_or(63);
        if sh >= 30 { 1 } else { (NS_PER_S >> sh).max(1) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_roundtrip_and_invalid() {
        let t = Timestamp::from_nanos(5_000_000_123);
        assert_eq!(t.seconds, 5);
        assert_eq!(t.nanoseconds, 123);
        assert_eq!(t.to_nanos(), Some(5_000_000_123));
        let bad = Timestamp {
            seconds: 1,
            nanoseconds: 1_000_000_000,
        };
        assert_eq!(bad.to_nanos(), None);
    }

    #[test]
    fn intervals() {
        assert_eq!(interval_ns(0), 1_000_000_000);
        assert_eq!(interval_ns(1), 2_000_000_000);
        assert_eq!(interval_ns(-3), 125_000_000);
        assert_eq!(interval_ns(i8::MAX), u64::MAX);
        assert_eq!(interval_ns(i8::MIN), 1);
    }

    #[test]
    fn signed_helpers_saturate() {
        assert_eq!(diff_ns(0, u64::MAX), i64::MIN);
        assert_eq!(diff_ns(10, 3), 7);
        assert_eq!(add_signed(5, -10), 0);
        assert_eq!(add_signed(u64::MAX, 5), u64::MAX);
    }
}
