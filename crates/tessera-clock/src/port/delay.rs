//! Median filter for path-delay measurements (fixed window, no heap).

/// Window length.
pub(crate) const DELAY_WINDOW: usize = 8;

#[derive(Debug, Clone, Copy)]
pub(crate) struct DelayFilter {
    buf: [i64; DELAY_WINDOW],
    n: usize,
    head: usize,
}

impl DelayFilter {
    pub(crate) const fn new() -> Self {
        DelayFilter {
            buf: [0; DELAY_WINDOW],
            n: 0,
            head: 0,
        }
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::new();
    }

    pub(crate) fn push(&mut self, v: i64) {
        if let Some(slot) = self.buf.get_mut(self.head) {
            *slot = v;
        }
        self.head = (self.head + 1) % DELAY_WINDOW;
        self.n = (self.n + 1).min(DELAY_WINDOW);
    }

    pub(crate) fn len(&self) -> usize {
        self.n
    }

    pub(crate) fn is_valid(&self) -> bool {
        self.n > 0
    }

    /// Median of the samples held.
    pub(crate) fn value(&self) -> Option<i64> {
        if self.n == 0 {
            return None;
        }
        let mut tmp = self.buf;
        let s = tmp.get_mut(..self.n)?;
        s.sort_unstable();
        let mid = self.n / 2;
        if self.n % 2 == 1 {
            s.get(mid).copied()
        } else {
            let lo = s.get(mid - 1).copied()?;
            let hi = s.get(mid).copied()?;
            Some(lo / 2 + hi / 2 + (lo % 2 + hi % 2) / 2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_rejects_outliers_and_wraps() {
        let mut f = DelayFilter::new();
        assert_eq!(f.value(), None);
        for v in [100, 102, 98, 5_000_000, 101] {
            f.push(v);
        }
        assert_eq!(f.value(), Some(101));
        for v in 0..20 {
            f.push(1000 + v);
        }
        assert!(f.value().unwrap() >= 1012);
        f.reset();
        assert!(!f.is_valid());
    }

    #[test]
    fn even_count_averages_middle_pair() {
        let mut f = DelayFilter::new();
        f.push(10);
        f.push(20);
        assert_eq!(f.value(), Some(15));
    }
}
