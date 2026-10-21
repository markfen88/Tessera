//! Making peer-chosen text safe to log or to echo back.
//!
//! Anything a client controls (client names, endpoint names, parser error
//! text) can contain newlines that forge journal lines, terminal escape
//! sequences, or bidirectional-override characters that reorder what an
//! operator reads. [`escape`] turns all of those into visible `\u{..}`
//! sequences, [`truncate`] bounds the length, [`log_safe`] does both.

/// Is `c` a character that must not reach a log or a terminal unescaped?
///
/// Control characters (C0 and C1, including newline and ESC), the Unicode
/// line and paragraph separators, zero-width and word-joiner characters, the
/// byte-order mark, and every bidirectional control (marks, embeddings,
/// overrides, isolates, Arabic letter mark).
#[must_use]
pub fn is_unsafe(c: char) -> bool {
    c.is_control()
        || matches!(
            u32::from(c),
            0x061C
                | 0x200B..=0x200F
                | 0x2028..=0x202E
                | 0x2060..=0x2064
                | 0x2066..=0x206F
                | 0xFEFF
        )
}

/// Replace unsafe characters (and the backslash, so the output is
/// unambiguous) by `\u{hex}` escapes.
#[must_use]
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\\' {
            out.push_str("\\\\");
        } else if is_unsafe(c) {
            out.push_str(&format!("\\u{{{:x}}}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out
}

/// At most `max` characters of `s`; a longer input ends in `…(+N)` where `N`
/// is the number of characters cut.
#[must_use]
pub fn truncate(s: &str, max: usize) -> String {
    let mut it = s.chars();
    let head: String = it.by_ref().take(max).collect();
    let rest = it.count();
    if rest == 0 {
        head
    } else {
        format!("{head}…(+{rest})")
    }
}

/// [`truncate`] then [`escape`]: for log lines.
#[must_use]
pub fn log_safe(s: &str, max: usize) -> String {
    escape(&truncate(s, max))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn newlines_and_escape_sequences_cannot_forge_lines() {
        let s = "good\nERROR tesserad: forged\r\u{1b}[31mred";
        let e = escape(s);
        assert!(!e.contains('\n') && !e.contains('\r') && !e.contains('\u{1b}'));
        assert_eq!(e, "good\\u{a}ERROR tesserad: forged\\u{d}\\u{1b}[31mred");
    }

    #[test]
    fn bidi_and_invisible_characters_are_made_visible() {
        for c in [
            '\u{202e}', '\u{2066}', '\u{200f}', '\u{061c}', '\u{200b}', '\u{feff}', '\u{2028}',
            '\u{85}',
        ] {
            let e = escape(&format!("a{c}b"));
            assert!(e.starts_with("a\\u{") && e.ends_with("}b"), "{c:?} -> {e}");
        }
        assert_eq!(escape("plain ünïcode 音"), "plain ünïcode 音");
        assert_eq!(escape("a\\b"), "a\\\\b");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcdef", 3), "abc…(+3)");
        assert_eq!(truncate("ééééé", 2), "éé…(+3)");
        assert_eq!(truncate("", 0), "");
    }

    proptest! {
        #[test]
        fn output_is_always_safe_and_bounded(s in ".*", max in 0usize..64) {
            let out = log_safe(&s, max);
            prop_assert!(!out.chars().any(is_unsafe));
            // Each kept character expands at most to 10 (`\u{10ffff}`).
            prop_assert!(out.chars().count() <= max * 10 + 24);
        }

        #[test]
        fn safe_text_is_unchanged(s in "[a-zA-Z0-9 ._:/-]{0,40}") {
            prop_assert_eq!(escape(&s), s);
        }
    }
}
