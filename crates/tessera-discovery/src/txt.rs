//! DNS-SD TXT records: construction, encoding and total parsing.
//!
//! Follows RFC 6763 section 6. A TXT record is a sequence of strings, each at
//! most 255 bytes (a one-byte length prefix limits it), of the form `key=value`
//! or a bare `key` (boolean flag). Keys are printable ASCII (0x20..=0x7E)
//! without `=`, non-empty, and unique ignoring ASCII case (section 6.4; a
//! receiver keeps the first occurrence of a repeated key).
//!
//! The total size is limited as well: RFC 6763 section 6.2 recommends staying
//! within 1300 bytes so a record fits one packet. Both limits are carried in
//! [`TxtLimits`]; the per-string limit can only be lowered, never raised above
//! the wire maximum of 255.
//!
//! [`TxtRecord::parse`] never panics and never allocates more than the input
//! size.

use core::fmt;

/// Largest string a TXT record can carry (RFC 6763 section 6.1).
pub const WIRE_MAX_STRING_LEN: usize = 255;
/// Default total size limit (RFC 6763 section 6.2 recommendation).
pub const DEFAULT_MAX_TOTAL_LEN: usize = 1300;

/// Size limits for a TXT record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxtLimits {
    /// Maximum bytes of one `key=value` string (clamped to 255).
    pub max_string: usize,
    /// Maximum bytes of the encoded record, length prefixes included.
    pub max_total: usize,
}

impl Default for TxtLimits {
    fn default() -> Self {
        Self {
            max_string: WIRE_MAX_STRING_LEN,
            max_total: DEFAULT_MAX_TOTAL_LEN,
        }
    }
}

impl TxtLimits {
    fn string_limit(&self) -> usize {
        self.max_string.min(WIRE_MAX_STRING_LEN)
    }
}

/// Problems with a TXT record.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TxtError {
    /// A key is empty.
    EmptyKey,
    /// A key contains `=` or a byte outside printable ASCII.
    InvalidKey(String),
    /// A key appears twice (ignoring ASCII case).
    DuplicateKey(String),
    /// A `key=value` string exceeds the per-string limit.
    StringTooLong {
        /// The key.
        key: String,
        /// Its string length in bytes.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The encoded record exceeds the total limit.
    TotalTooLong {
        /// Encoded length in bytes.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// A length prefix at this input offset runs past the end of the input.
    Truncated {
        /// Offset of the offending length byte.
        offset: usize,
    },
}

impl fmt::Display for TxtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TxtError::EmptyKey => write!(f, "TXT key is empty"),
            TxtError::InvalidKey(k) => write!(f, "TXT key {k:?} has an invalid character"),
            TxtError::DuplicateKey(k) => write!(f, "TXT key {k:?} appears twice"),
            TxtError::StringTooLong { key, len, max } => {
                write!(f, "TXT string for {key:?} is {len} bytes (limit {max})")
            }
            TxtError::TotalTooLong { len, max } => {
                write!(f, "TXT record is {len} bytes (limit {max})")
            }
            TxtError::Truncated { offset } => {
                write!(f, "TXT string at offset {offset} runs past the end")
            }
        }
    }
}

impl std::error::Error for TxtError {}

/// One TXT attribute.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TxtEntry {
    /// The key (printable ASCII, no `=`).
    pub key: String,
    /// The value; `None` is a bare flag, `Some(empty)` is `key=`.
    pub value: Option<Vec<u8>>,
}

impl TxtEntry {
    /// Length of the `key[=value]` string in bytes.
    #[must_use]
    pub fn string_len(&self) -> usize {
        self.key.len() + self.value.as_ref().map_or(0, |v| v.len() + 1)
    }

    /// The value as UTF-8, if it is a valid string.
    #[must_use]
    pub fn value_str(&self) -> Option<&str> {
        self.value
            .as_deref()
            .and_then(|v| std::str::from_utf8(v).ok())
    }
}

/// An ordered set of TXT attributes with unique keys.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct TxtRecord {
    entries: Vec<TxtEntry>,
}

/// Result of parsing wire data: the record plus how many strings were
/// ignored (empty strings, empty or invalid keys, repeated keys).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTxt {
    /// The attributes that were kept, in wire order.
    pub record: TxtRecord,
    /// Number of strings that were skipped.
    pub skipped: usize,
}

fn check_key(key: &str) -> Result<(), TxtError> {
    if key.is_empty() {
        return Err(TxtError::EmptyKey);
    }
    if key.bytes().all(valid_key_byte) {
        Ok(())
    } else {
        Err(TxtError::InvalidKey(key.to_owned()))
    }
}

fn valid_key_byte(b: u8) -> bool {
    (0x20..=0x7e).contains(&b) && b != b'='
}

impl TxtRecord {
    /// An empty record.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `key=value`.
    ///
    /// # Errors
    /// [`TxtError::EmptyKey`], [`TxtError::InvalidKey`] or
    /// [`TxtError::DuplicateKey`]. Sizes are checked by [`TxtRecord::validate`]
    /// and [`TxtRecord::encode`], because the limits are configurable.
    pub fn push(
        &mut self,
        key: impl Into<String>,
        value: impl AsRef<[u8]>,
    ) -> Result<(), TxtError> {
        self.push_entry(key.into(), Some(value.as_ref().to_vec()))
    }

    /// Adds a bare flag `key`.
    ///
    /// # Errors
    /// As for [`TxtRecord::push`].
    pub fn push_flag(&mut self, key: impl Into<String>) -> Result<(), TxtError> {
        self.push_entry(key.into(), None)
    }

    fn push_entry(&mut self, key: String, value: Option<Vec<u8>>) -> Result<(), TxtError> {
        check_key(&key)?;
        if self.get(&key).is_some() {
            return Err(TxtError::DuplicateKey(key));
        }
        self.entries.push(TxtEntry { key, value });
        Ok(())
    }

    /// Builder form of [`TxtRecord::push`].
    ///
    /// # Errors
    /// As for [`TxtRecord::push`].
    pub fn with(
        mut self,
        key: impl Into<String>,
        value: impl AsRef<[u8]>,
    ) -> Result<Self, TxtError> {
        self.push(key, value)?;
        Ok(self)
    }

    /// Builder form of [`TxtRecord::push_flag`].
    ///
    /// # Errors
    /// As for [`TxtRecord::push`].
    pub fn with_flag(mut self, key: impl Into<String>) -> Result<Self, TxtError> {
        self.push_flag(key)?;
        Ok(self)
    }

    /// The entry for `key` (ASCII case-insensitive).
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&TxtEntry> {
        self.entries
            .iter()
            .find(|e| e.key.eq_ignore_ascii_case(key))
    }

    /// The value of `key` as UTF-8 (`None` for a missing key, a bare flag or
    /// a non-UTF-8 value).
    #[must_use]
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(TxtEntry::value_str)
    }

    /// True if `key` is present (as a flag or with a value).
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// The entries in order.
    #[must_use]
    pub fn entries(&self) -> &[TxtEntry] {
        &self.entries
    }

    /// Number of attributes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if there are no attributes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Size of the wire encoding: one length byte plus the string for each
    /// attribute; an empty record encodes as a single zero byte
    /// (RFC 6763 section 6.1).
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        if self.entries.is_empty() {
            1
        } else {
            self.entries.iter().map(|e| 1 + e.string_len()).sum()
        }
    }

    /// Checks the record against `limits`.
    ///
    /// # Errors
    /// [`TxtError::StringTooLong`] or [`TxtError::TotalTooLong`].
    pub fn validate(&self, limits: &TxtLimits) -> Result<(), TxtError> {
        let max = limits.string_limit();
        for e in &self.entries {
            let len = e.string_len();
            if len > max {
                return Err(TxtError::StringTooLong {
                    key: e.key.clone(),
                    len,
                    max,
                });
            }
        }
        let len = self.encoded_len();
        if len > limits.max_total {
            return Err(TxtError::TotalTooLong {
                len,
                max: limits.max_total,
            });
        }
        Ok(())
    }

    /// Encodes to wire format (RDATA of a TXT record).
    ///
    /// # Errors
    /// The errors of [`TxtRecord::validate`].
    pub fn encode(&self, limits: &TxtLimits) -> Result<Vec<u8>, TxtError> {
        self.validate(limits)?;
        let mut out = Vec::with_capacity(self.encoded_len());
        if self.entries.is_empty() {
            out.push(0);
            return Ok(out);
        }
        for e in &self.entries {
            // validate() bounded string_len() by 255.
            out.push(u8::try_from(e.string_len()).unwrap_or(u8::MAX));
            out.extend_from_slice(e.key.as_bytes());
            if let Some(v) = &e.value {
                out.push(b'=');
                out.extend_from_slice(v);
            }
        }
        Ok(out)
    }

    /// Parses wire data. Total: any input yields `Ok` or a [`TxtError`].
    ///
    /// Empty strings, strings with an empty or invalid key and repeated keys
    /// (the first occurrence wins, RFC 6763 section 6.4) are skipped and
    /// counted in [`ParsedTxt::skipped`].
    ///
    /// # Errors
    /// [`TxtError::TotalTooLong`] if `bytes` exceeds `limits.max_total`,
    /// [`TxtError::Truncated`] if a length prefix runs past the end.
    pub fn parse(bytes: &[u8], limits: &TxtLimits) -> Result<ParsedTxt, TxtError> {
        if bytes.len() > limits.max_total {
            return Err(TxtError::TotalTooLong {
                len: bytes.len(),
                max: limits.max_total,
            });
        }
        let mut strings = Vec::new();
        let mut offset = 0usize;
        while let Some(&len) = bytes.get(offset) {
            let start = offset + 1;
            let end = start + usize::from(len);
            let s = bytes
                .get(start..end)
                .ok_or(TxtError::Truncated { offset })?;
            strings.push(s);
            offset = end;
        }
        Ok(Self::from_strings(strings.iter().copied()))
    }

    /// Builds a record from already-split strings (for example the `aay` of
    /// the Avahi API), with the same skipping rules as [`TxtRecord::parse`].
    pub fn from_strings<'a>(strings: impl IntoIterator<Item = &'a [u8]>) -> ParsedTxt {
        let mut record = TxtRecord::new();
        let mut skipped = 0;
        for s in strings {
            let (key, value) = match s.iter().position(|&b| b == b'=') {
                Some(i) => (s.get(..i), s.get(i + 1..)),
                None => (Some(s), None),
            };
            let ok = key
                .and_then(|k| std::str::from_utf8(k).ok())
                .map(|k| (k, value))
                .and_then(|(k, v)| record.push_entry(k.to_owned(), v.map(<[u8]>::to_vec)).ok());
            if ok.is_none() {
                skipped += 1;
            }
        }
        ParsedTxt { record, skipped }
    }

    /// The strings of the record as `key[=value]` byte vectors, in order (the
    /// form the Avahi D-Bus API takes).
    #[must_use]
    pub fn to_strings(&self) -> Vec<Vec<u8>> {
        self.entries
            .iter()
            .map(|e| {
                let mut s = e.key.as_bytes().to_vec();
                if let Some(v) = &e.value {
                    s.push(b'=');
                    s.extend_from_slice(v);
                }
                s
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn lim() -> TxtLimits {
        TxtLimits::default()
    }

    #[test]
    fn encodes_pairs_and_flags() {
        let r = TxtRecord::new()
            .with("txtvers", "2")
            .unwrap()
            .with_flag("default")
            .unwrap();
        assert_eq!(
            r.encode(&lim()).unwrap(),
            b"\x09txtvers=2\x07default".to_vec()
        );
    }

    #[test]
    fn empty_record_is_one_zero_byte() {
        assert_eq!(TxtRecord::new().encode(&lim()).unwrap(), vec![0]);
        let p = TxtRecord::parse(&[0], &lim()).unwrap();
        assert!(p.record.is_empty());
        assert_eq!(p.skipped, 1);
        assert!(TxtRecord::parse(&[], &lim()).unwrap().record.is_empty());
    }

    #[test]
    fn key_rules() {
        let mut r = TxtRecord::new();
        assert_eq!(r.push("", "x"), Err(TxtError::EmptyKey));
        assert!(matches!(r.push("a=b", "x"), Err(TxtError::InvalidKey(_))));
        assert!(matches!(
            r.push("a\u{e9}", "x"),
            Err(TxtError::InvalidKey(_))
        ));
        assert!(matches!(r.push("a\n", "x"), Err(TxtError::InvalidKey(_))));
        r.push("Key", "1").unwrap();
        assert!(matches!(r.push("KEY", "2"), Err(TxtError::DuplicateKey(_))));
        assert_eq!(r.get_str("kEy"), Some("1"));
        r.push("with space", "").unwrap();
    }

    #[test]
    fn string_limit_is_255_inclusive() {
        let key = "k";
        let ok = "v".repeat(255 - 2);
        let r = TxtRecord::new().with(key, &ok).unwrap();
        assert_eq!(r.entries()[0].string_len(), 255);
        assert!(r.encode(&lim()).is_ok());
        let r = TxtRecord::new().with(key, format!("{ok}v")).unwrap();
        assert_eq!(
            r.validate(&lim()),
            Err(TxtError::StringTooLong {
                key: "k".into(),
                len: 256,
                max: 255
            })
        );
    }

    #[test]
    fn string_limit_cannot_be_raised_above_wire_max() {
        let l = TxtLimits {
            max_string: 10_000,
            max_total: 10_000,
        };
        let r = TxtRecord::new().with("k", "v".repeat(300)).unwrap();
        assert!(matches!(
            r.encode(&l),
            Err(TxtError::StringTooLong { max: 255, .. })
        ));
    }

    #[test]
    fn total_limit() {
        let mut r = TxtRecord::new();
        for i in 0..6 {
            r.push(format!("k{i}"), "x".repeat(200)).unwrap();
        }
        // 6 * (1 + 2 + 1 + 200) = 1224
        assert_eq!(r.encoded_len(), 1224);
        assert!(r.validate(&lim()).is_ok());
        r.push("k6", "x".repeat(200)).unwrap();
        assert_eq!(
            r.validate(&lim()),
            Err(TxtError::TotalTooLong {
                len: 1428,
                max: 1300
            })
        );
        let big = TxtLimits {
            max_string: 255,
            max_total: 2000,
        };
        assert!(r.validate(&big).is_ok());
    }

    #[test]
    fn parse_skips_junk_and_keeps_first_duplicate() {
        let mut wire = Vec::new();
        for s in [&b"a=1"[..], b"", b"=x", b"A=2", b"b", b"c=\xff"] {
            wire.push(s.len() as u8);
            wire.extend_from_slice(s);
        }
        let p = TxtRecord::parse(&wire, &lim()).unwrap();
        assert_eq!(p.skipped, 3); // empty, "=x", duplicate A
        assert_eq!(p.record.get_str("a"), Some("1"));
        assert!(p.record.get("b").unwrap().value.is_none());
        assert_eq!(
            p.record.get("c").unwrap().value.as_deref(),
            Some(&b"\xff"[..])
        );
        assert_eq!(p.record.get_str("c"), None);
    }

    #[test]
    fn parse_detects_truncation_with_offset() {
        assert_eq!(
            TxtRecord::parse(b"\x03a=1\x05ab", &lim()),
            Err(TxtError::Truncated { offset: 4 })
        );
    }

    #[test]
    fn parse_rejects_oversize_input() {
        let l = TxtLimits {
            max_string: 255,
            max_total: 4,
        };
        assert!(matches!(
            TxtRecord::parse(b"\x03a=1\x00", &l),
            Err(TxtError::TotalTooLong { len: 5, max: 4 })
        ));
    }

    fn arb_record() -> impl Strategy<Value = TxtRecord> {
        proptest::collection::vec(
            (
                "[ -<>-~]{1,20}",
                proptest::option::of(proptest::collection::vec(any::<u8>(), 0..40)),
            ),
            0..12,
        )
        .prop_map(|v| {
            let mut r = TxtRecord::new();
            for (k, val) in v {
                let _ = match val {
                    Some(val) => r.push(k, val),
                    None => r.push_flag(k),
                };
            }
            r
        })
    }

    proptest! {
        #[test]
        fn parse_is_total(bytes in proptest::collection::vec(any::<u8>(), 0..3000)) {
            let _ = TxtRecord::parse(&bytes, &lim());
            let big = TxtLimits { max_string: 255, max_total: usize::MAX };
            let _ = TxtRecord::parse(&bytes, &big);
        }

        #[test]
        fn encode_parse_roundtrip(r in arb_record()) {
            let wire = r.encode(&lim()).unwrap();
            prop_assert_eq!(wire.len(), r.encoded_len());
            let p = TxtRecord::parse(&wire, &lim()).unwrap();
            if r.is_empty() {
                prop_assert!(p.record.is_empty());
            } else {
                prop_assert_eq!(p.skipped, 0);
                prop_assert_eq!(p.record, r);
            }
        }

        #[test]
        fn encode_respects_limits(r in arb_record(), max_string in 0usize..300, max_total in 0usize..600) {
            let l = TxtLimits { max_string, max_total };
            match r.encode(&l) {
                Ok(w) => {
                    prop_assert!(w.len() <= max_total);
                    for e in r.entries() {
                        prop_assert!(e.string_len() <= max_string.min(255));
                    }
                }
                Err(TxtError::StringTooLong { len, max, .. }) => prop_assert!(len > max),
                Err(TxtError::TotalTooLong { len, max }) => prop_assert!(len > max),
                Err(e) => prop_assert!(false, "unexpected {e:?}"),
            }
        }

        #[test]
        fn strings_roundtrip(r in arb_record()) {
            let strings = r.to_strings();
            let p = TxtRecord::from_strings(strings.iter().map(Vec::as_slice));
            prop_assert_eq!(p.skipped, 0);
            prop_assert_eq!(p.record, r);
        }
    }
}
