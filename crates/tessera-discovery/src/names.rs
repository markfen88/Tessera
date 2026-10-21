//! DNS name helpers shared by the backends.

use crate::model::{ServiceId, ServiceKind};

/// Escapes an instance name for use in a presentation-format DNS name
/// (dots and backslashes get a backslash).
pub(crate) fn escape_instance(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    for c in name.chars() {
        if c == '.' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Reverses [`escape_instance`]. Total: a trailing lone backslash is kept.
pub(crate) fn unescape_instance(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(n) => out.push(n),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Makes text from the network safe to print or log: control characters and
/// the bidirectional-formatting characters (which can reorder what a reader
/// sees) are written as `\u{..}`. Everything else is kept.
pub(crate) fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// See [`is_bidi_control`].
pub(crate) fn sanitize(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() || is_bidi_control(c) {
            let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
        } else {
            out.push(c);
        }
    }
    out
}

/// `<escaped instance>.<type>.local.`
pub(crate) fn full_name(kind: ServiceKind, instance: &str) -> String {
    format!("{}.{}", escape_instance(instance), kind.type_domain())
}

/// Splits `<instance>.<type>.local.` (case-insensitive type) into a
/// [`ServiceId`]. `None` if the type is not one of ours.
pub(crate) fn parse_full_name(full: &str) -> Option<ServiceId> {
    for kind in ServiceKind::ALL {
        let suffix = format!(".{}", kind.type_domain());
        let lower = full.to_ascii_lowercase();
        if lower.ends_with(&suffix.to_ascii_lowercase()) {
            let inst = full.get(..full.len() - suffix.len())?;
            return Some(ServiceId::new(kind, unescape_instance(inst)));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn escape_roundtrip(s in "\\PC{0,40}") {
            prop_assert_eq!(unescape_instance(&escape_instance(&s)), s);
        }

        #[test]
        fn full_name_roundtrip(s in "[ -~]{1,40}") {
            for k in ServiceKind::ALL {
                let id = parse_full_name(&full_name(k, &s)).unwrap();
                prop_assert_eq!(id.kind, k);
                prop_assert_eq!(id.instance, s.clone());
            }
        }

        #[test]
        fn parsers_are_total(s in "\\PC{0,80}") {
            let _ = parse_full_name(&s);
            let _ = unescape_instance(&s);
        }
    }

    proptest! {
        #[test]
        fn sanitized_text_has_no_control_or_bidi_characters(s in "\\PC{0,40}|[\\x00-\\x1f\\u{202a}-\\u{202e}\\u{2066}-\\u{2069}a-z]{0,40}") {
            let o = sanitize(&s);
            let bad = o.chars().any(|c| c.is_control() || is_bidi_control(c));
            prop_assert!(!bad);
        }
    }

    #[test]
    fn sanitize_escapes_and_keeps_the_rest() {
        assert_eq!(
            sanitize("a\u{202e}b\n\u{7f}{e9}"),
            "a\\u{202e}b\\u{a}\\u{7f}{e9}"
        );
    }

    #[test]
    fn foreign_types_are_not_ours() {
        assert!(parse_full_name("x._http._tcp.local.").is_none());
        let id = parse_full_name("Mic 1@host (2)._NetAudio-Chan._UDP.local.").unwrap();
        assert_eq!(id.kind, ServiceKind::Chan);
        assert_eq!(id.instance, "Mic 1@host (2)");
    }
}
