//! Media time: the clock trait this crate depends on and sample/nanosecond
//! conversions.
//!
//! All media time is `u64` samples or `u64` nanoseconds (ADR 0004). The
//! conversions use 128-bit intermediates and never overflow.

const NANOS_PER_SEC: u128 = 1_000_000_000;

/// One consistent reading of the media clock: a media time together with the
/// discontinuity epoch it belongs to.
///
/// The pair must come from a single read of the clock state (for the
/// `tessera-clock` overlay: one stable pass of the seqlock), never from two
/// separate reads, otherwise a media time of one epoch could be paired with
/// the epoch counter of another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSnapshot {
    /// Discontinuity counter (ADR 0005). It changes whenever media time
    /// stepped, the grandmaster changed or holdover began; consumers must
    /// re-anchor and must not trust positions from an earlier epoch.
    pub epoch: u64,
    /// Media time in nanoseconds.
    pub media_ns: u64,
}

/// The view of the media clock this crate needs (ADR 0005).
///
/// `tessera-clock` implements this on top of its seqlock overlay; tests
/// implement it on a virtual clock. The engines never read a wall clock, and
/// every engine call takes exactly one snapshot, so time and epoch are never
/// torn.
pub trait MediaClock {
    /// Media time now (reads `CLOCK_MONOTONIC_RAW` and maps it).
    fn now(&self) -> ClockSnapshot;

    /// Maps a local `CLOCK_MONOTONIC_RAW` reading (for example a packet receive
    /// timestamp) to media time under the current mapping.
    fn at_local(&self, local_ns: u64) -> ClockSnapshot;
}

impl<T: MediaClock + ?Sized> MediaClock for &T {
    fn now(&self) -> ClockSnapshot {
        (**self).now()
    }
    fn at_local(&self, local_ns: u64) -> ClockSnapshot {
        (**self).at_local(local_ns)
    }
}

fn clamp_u64(v: u128) -> u64 {
    u64::try_from(v).unwrap_or(u64::MAX)
}

/// `floor(ns * rate / 1e9)`: the sample index containing the instant `ns`.
#[must_use]
pub fn ns_to_samples(ns: u64, rate: u32) -> u64 {
    clamp_u64(u128::from(ns) * u128::from(rate) / NANOS_PER_SEC)
}

/// `ceil(ns * rate / 1e9)`.
#[must_use]
pub fn ns_to_samples_ceil(ns: u64, rate: u32) -> u64 {
    clamp_u64((u128::from(ns) * u128::from(rate)).div_ceil(NANOS_PER_SEC))
}

/// `floor(samples * 1e9 / rate)`; a zero rate yields `u64::MAX`.
#[must_use]
pub fn samples_to_ns(samples: u64, rate: u32) -> u64 {
    if rate == 0 {
        return u64::MAX;
    }
    clamp_u64(u128::from(samples) * NANOS_PER_SEC / u128::from(rate))
}

/// `ceil(samples * 1e9 / rate)`: the first instant at or after the start of
/// sample `samples`; a zero rate yields `u64::MAX`.
#[must_use]
pub fn samples_to_ns_ceil(samples: u64, rate: u32) -> u64 {
    if rate == 0 {
        return u64::MAX;
    }
    clamp_u64((u128::from(samples) * NANOS_PER_SEC).div_ceil(u128::from(rate)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn known_values() {
        assert_eq!(ns_to_samples(1_000_000_000, 48_000), 48_000);
        assert_eq!(ns_to_samples(20_833, 48_000), 0);
        assert_eq!(ns_to_samples(20_834, 48_000), 1);
        assert_eq!(ns_to_samples_ceil(20_833, 48_000), 1);
        assert_eq!(samples_to_ns(1, 48_000), 20_833);
        assert_eq!(samples_to_ns_ceil(1, 48_000), 20_834);
        assert_eq!(samples_to_ns(5, 0), u64::MAX);
    }

    proptest! {
        #[test]
        fn ceil_is_at_least_floor_and_roundtrips(s in 0u64..(1u64 << 33), rate in 1u32..=384_000) {
            let lo = samples_to_ns(s, rate);
            let hi = samples_to_ns_ceil(s, rate);
            prop_assert!(hi >= lo && hi - lo <= 1);
            // The instant at the start of sample s lies in sample s.
            prop_assert_eq!(ns_to_samples(hi, rate), s);
        }

        #[test]
        fn never_overflows(ns in any::<u64>(), rate in any::<u32>(), s in any::<u64>()) {
            let _ = ns_to_samples(ns, rate);
            let _ = ns_to_samples_ceil(ns, rate);
            let _ = samples_to_ns(s, rate);
            let _ = samples_to_ns_ceil(s, rate);
        }
    }
}
